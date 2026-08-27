// Adapter around oci-client. OCI descriptors are converted to oci-spec at the
// boundary; layer blobs remain lazy so pushes can cross-mount them

use std::fs::File;
use std::future::Future;
use std::io::{Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::TryStreamExt;
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::errors::{OciDistributionError, OciErrorCode};
use oci_client::manifest::{OciDescriptor, OciManifest};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use oci_spec::image::{Descriptor, DescriptorBuilder, Digest, ImageConfiguration, MediaType};
use secrecy::ExposeSecret;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use tracing::warn;

use sha2::{Digest as _, Sha256};

use crate::auth::{Credential, Credentials};
use crate::digest::{finish_sha256, sha256_digest};
use crate::image::{Image, ImageSource, Platform};

const UPLOAD_CHUNK_SIZE: usize = 4 * 1024 * 1024;

pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_MAX_ATTEMPTS: usize = 3;

#[derive(Clone, Copy)]
struct RetryPolicy {
    max_attempts: usize,
    initial_delay: Duration,
    max_delay: Duration,
}

/// Transport options for one registry-client role. Pull and push use separate
/// clients so disabling TLS verification for a development source cannot
/// silently weaken a production target
#[derive(Debug, Clone)]
pub struct RegistryOptions {
    pub plain_http_hosts: Vec<String>,
    /// If true, accept invalid TLS certs and hostnames. Applies globally —
    /// oci-client's reqwest backend has no per-host hook for this
    pub skip_tls_verify: bool,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub max_attempts: usize,
}

impl Default for RegistryOptions {
    fn default() -> Self {
        Self {
            plain_http_hosts: Vec::new(),
            skip_tls_verify: false,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            read_timeout: DEFAULT_READ_TIMEOUT,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        }
    }
}

#[derive(Clone)]
pub struct RegistryClient {
    inner: Client,
    credentials: Arc<Credentials>,
    platform: Platform,
    retry_policy: RetryPolicy,
}

impl RegistryClient {
    #[allow(unused)]
    pub fn new(credentials: Credentials, platform: Platform) -> Result<Self> {
        Self::with_options(credentials, platform, RegistryOptions::default())
    }

    pub fn with_options(
        credentials: Credentials,
        platform: Platform,
        options: RegistryOptions,
    ) -> Result<Self> {
        let protocol = if options.plain_http_hosts.is_empty() {
            ClientProtocol::Https
        } else {
            ClientProtocol::HttpsExcept(options.plain_http_hosts)
        };

        let resolver_platform = platform.clone();
        let cfg = ClientConfig {
            // The default resolver selects the host platform, which is wrong
            // for cross-builds
            platform_resolver: Some(Box::new(move |entries| {
                entries
                    .iter()
                    .find(|e| {
                        e.platform.as_ref().is_some_and(|p| {
                            resolver_platform.matches(
                                &p.os.to_string(),
                                &p.architecture.to_string(),
                                p.variant.as_deref(),
                            )
                        })
                    })
                    .map(|e| e.digest.clone())
            })),
            protocol,
            accept_invalid_certificates: options.skip_tls_verify,
            connect_timeout: Some(options.connect_timeout),
            read_timeout: Some(options.read_timeout),
            ..Default::default()
        };
        let inner = Client::try_from(cfg).context("creating OCI registry HTTP client")?;
        Ok(Self {
            inner,
            credentials: Arc::new(credentials),
            platform,
            retry_policy: RetryPolicy {
                max_attempts: options.max_attempts,
                initial_delay: Duration::from_millis(250),
                max_delay: Duration::from_secs(2),
            },
        })
    }

    /// Pull base image: resolve manifest list -> platform manifest -> config
    /// Layer bytes are not fetched
    pub async fn pull_base(&self, reference: &Reference) -> Result<Image> {
        let registry = reference.resolve_registry().to_string();

        let auth = self.registry_auth_for(&registry);

        let (manifest, manifest_digest, config_json) =
            retry_registry("pull manifest and config", self.retry_policy, || {
                self.inner.pull_manifest_and_config(reference, &auth)
            })
            .await
            .with_context(|| format!("pulling manifest+config for {reference}"))?;

        let config_bytes = config_json.into_bytes();
        let config: ImageConfiguration =
            serde_json::from_slice(&config_bytes).context("parsing image config JSON")?;
        let computed = sha256_digest(&config_bytes);
        let from_manifest: Digest = manifest
            .config
            .digest
            .parse()
            .context("parsing config descriptor digest")?;
        if computed != from_manifest {
            bail!(
                "config blob digest mismatch: manifest says {}, computed {}",
                from_manifest,
                computed
            );
        }
        validate_platform(&self.platform, &config)?;

        let layers: Vec<Descriptor> = manifest
            .layers
            .iter()
            .map(to_oci_spec_descriptor)
            .collect::<Result<_>>()?;

        Ok(Image {
            source: Some(ImageSource {
                reference: reference.clone(),
                manifest_digest: manifest_digest.parse().context("parsing manifest digest")?,
            }),
            config,
            layers,
        })
    }

    fn registry_auth_for(&self, registry: &str) -> RegistryAuth {
        match self.credentials.resolve(registry) {
            None => RegistryAuth::Anonymous,
            Some(c) => match c {
                Credential::IdentityToken {
                    identity_token,
                    source: _,
                } => RegistryAuth::Bearer(identity_token.expose_secret().to_string()),
                Credential::Basic {
                    username,
                    password,
                    source: _,
                } => RegistryAuth::Basic(username, password.expose_secret().to_string()),
            },
        }
    }

    async fn ensure_auth(&self, reference: &Reference) {
        let auth = self.registry_auth_for(reference.resolve_registry());
        self.inner
            .store_auth_if_needed(reference.resolve_registry(), &auth)
            .await;
    }
    /// Fetch a blob (layer or any other content-addressable object)
    /// `repo_ref` should reference the repository the blob lives in (registry + repo);
    /// the digest in the descriptor identifies the actual blob
    pub async fn fetch_layer_to_file<F>(
        &self,
        reference: &Reference,
        descriptor: &Descriptor,
        file_path: Option<&PathBuf>,
        on_progress: F,
    ) -> Result<File>
    where
        F: Fn(u64) + Send + Sync,
    {
        self.ensure_auth(reference).await;

        let oc = OciDescriptor {
            media_type: descriptor.media_type().to_string(),
            digest: descriptor.digest().to_string(),
            size: i64::try_from(descriptor.size())
                .map_err(|_| anyhow!("descriptor size doesn't fit in i64"))?,
            urls: descriptor.urls().clone(),
            annotations: descriptor
                .annotations()
                .as_ref()
                .map(|a| a.clone().into_iter().collect()),
            artifact_type: descriptor.artifact_type().as_ref().map(ToString::to_string),
        };

        let on_progress = Arc::new(on_progress);
        let reported_bytes = AtomicU64::new(0);
        retry_registry("pull blob", self.retry_policy, || {
            self.fetch_layer_once(
                file_path,
                reference,
                &oc,
                on_progress.as_ref(),
                &reported_bytes,
            )
        })
        .await
        .with_context(|| format!("pulling blob {} from {}", descriptor.digest(), reference))
    }

    async fn fetch_layer_once<F>(
        &self,
        file_path: Option<&PathBuf>,
        reference: &Reference,
        descriptor: &OciDescriptor,
        on_progress: &F,
        reported_bytes: &AtomicU64,
    ) -> oci_client::errors::Result<File>
    where
        F: Fn(u64) + Sync + ?Sized,
    {
        let std_file = match file_path {
            Some(path) => {
                tokio::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(path)
                    .await?
                    .into_std()
                    .await
            }
            None => tokio::task::spawn_blocking(tempfile::tempfile)
                .await
                .expect("blocking task panicked")?,
        };

        let mut writer = tokio::fs::File::from_std(std_file);
        let mut stream = self.inner.pull_blob_stream(reference, descriptor).await?;
        let mut attempt_bytes = 0_u64;
        let mut hasher = Sha256::new();

        while let Some(chunk) = stream.try_next().await? {
            hasher.update(&chunk);
            writer.write_all(&chunk).await?;
            attempt_bytes += chunk.len() as u64;
            report_progress_high_det(reported_bytes, attempt_bytes, on_progress);
        }

        writer.flush().await?;
        writer.sync_all().await?;
        verify_layer_digest(&descriptor.digest, hasher)?;
        Ok(writer.into_std().await)
    }

    /// Check if a blob already exists in the target repository
    /// Avoids re-uploading layers that the registry already has
    pub async fn blob_exists(&self, reference: &Reference, digest: &Digest) -> Result<bool> {
        self.ensure_auth(reference).await;
        retry_registry("check blob", self.retry_policy, || {
            self.inner.blob_exists(reference, digest.as_ref())
        })
        .await
        .with_context(|| format!("checking blob {digest} in {reference}"))
    }

    /// Attempt a cross-repository mount in the same registry
    pub async fn mount_blob(
        &self,
        target: &Reference,
        source: &Reference,
        digest: &Digest,
    ) -> Result<()> {
        self.ensure_auth(target).await;
        retry_registry("mount blob", self.retry_policy, || {
            self.inner.mount_blob(target, source, digest.as_ref())
        })
        .await
        .with_context(|| format!("mounting blob {digest} from {} to {}", source, target))
    }

    pub async fn push_blob_from_file<F>(
        &self,
        reference: &Reference,
        digest: &Digest,
        file: &mut std::fs::File,
        on_progress: F,
    ) -> Result<()>
    where
        F: Fn(u64) + Send + Sync + 'static,
    {
        file.seek(SeekFrom::Start(0))
            .context("rewinding blob file")?;
        self.ensure_auth(reference).await;

        let source = file.try_clone().context("cloning blob file handle")?;
        let on_progress = Arc::new(on_progress);
        let reported_bytes = AtomicU64::new(0);
        retry_registry("push blob", self.retry_policy, || {
            self.push_blob_file_once(
                reference,
                digest,
                &source,
                on_progress.as_ref(),
                &reported_bytes,
            )
        })
        .await
        .with_context(|| format!("pushing blob {digest} to {reference}"))
    }

    async fn push_blob_file_once<F>(
        &self,
        reference: &Reference,
        digest: &Digest,
        source: &std::fs::File,
        on_progress: &F,
        reported_bytes: &AtomicU64,
    ) -> oci_client::errors::Result<()>
    where
        F: Fn(u64) + Sync + ?Sized,
    {
        let mut file = source.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        let mut async_file = tokio::fs::File::from_std(file);
        // Tokio files cap each blocking read at 2 MiB by default. Raise that
        // cap as well as ReaderStream's capacity so one stream item maps to
        // one intended registry PATCH
        async_file.set_max_buf_size(UPLOAD_CHUNK_SIZE);
        let mut attempt_bytes = 0_u64;
        let stream = ReaderStream::with_capacity(async_file, UPLOAD_CHUNK_SIZE)
            .map_ok(|chunk| {
                attempt_bytes += chunk.len() as u64;
                report_progress_high_det(reported_bytes, attempt_bytes, on_progress);
                chunk
            })
            .map_err(oci_client::errors::OciDistributionError::from);
        self.inner
            .push_blob_stream(reference, stream, digest.as_ref())
            .await?;
        Ok(())
    }

    pub async fn push_blob_bytes(
        &self,
        reference: &Reference,
        digest: &Digest,
        bytes: &[u8],
    ) -> Result<()> {
        self.ensure_auth(reference).await;
        let bytes = bytes::Bytes::copy_from_slice(bytes);
        retry_registry("push config blob", self.retry_policy, || {
            self.inner
                .push_blob(reference, bytes.clone(), digest.as_ref())
        })
        .await
        .with_context(|| format!("pushing blob {digest} to {reference}"))?;
        Ok(())
    }

    pub async fn push_manifest(&self, reference: &Reference, manifest_json: &[u8]) -> Result<()> {
        self.ensure_auth(reference).await;
        let manifest: OciManifest =
            serde_json::from_slice(manifest_json).context("parsing OCI manifest JSON")?;
        retry_registry("push manifest", self.retry_policy, || {
            self.inner.push_manifest(reference, &manifest)
        })
        .await
        .with_context(|| format!("pushing manifest to {reference}"))?;
        Ok(())
    }
}

fn verify_layer_digest(expected: &str, hasher: Sha256) -> oci_client::errors::Result<()> {
    if !expected.starts_with("sha256:") {
        warn!(
            digest = expected,
            "skipping verification of non-sha256 blob"
        );
        return Ok(());
    }
    let actual = finish_sha256(hasher);
    if actual.as_ref() != expected {
        return Err(OciDistributionError::IoError(std::io::Error::other(
            oci_client::errors::DigestError::VerificationError {
                expected: expected.to_string(),
                actual: actual.to_string(),
            },
        )));
    }
    Ok(())
}

fn validate_platform(requested: &Platform, config: &ImageConfiguration) -> Result<()> {
    let os = config.os().to_string();
    let architecture = config.architecture().to_string();
    let variant = config.variant().as_deref();
    if requested.matches(&os, &architecture, variant) {
        return Ok(());
    }

    let actual = match variant {
        Some(variant) => format!("{os}/{architecture}/{variant}"),
        None => format!("{os}/{architecture}"),
    };
    bail!(
        "base image platform {actual} does not match requested platform {requested}; \
         use a matching base image manifest"
    );
}

fn report_progress_high_det<F>(reported: &AtomicU64, observed: u64, on_progress: &F)
where
    F: Fn(u64) + ?Sized,
{
    let previous = reported.fetch_max(observed, Ordering::Relaxed);
    if observed > previous {
        on_progress(observed - previous);
    }
}

async fn retry_registry<T, F, Fut>(
    operation: &str,
    policy: RetryPolicy,
    mut action: F,
) -> oci_client::errors::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = oci_client::errors::Result<T>>,
{
    debug_assert!(policy.max_attempts > 0);
    let mut attempt = 1;
    loop {
        match action().await {
            Ok(value) => return Ok(value),
            Err(error) if attempt < policy.max_attempts && is_retryable_registry_error(&error) => {
                let delay = policy.delay_after(attempt);
                warn!(
                    operation,
                    attempt,
                    max_attempts = policy.max_attempts,
                    delay_ms = delay.as_millis(),
                    error = %error,
                    "transient registry operation failed; retrying"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

impl RetryPolicy {
    fn delay_after(self, failed_attempt: usize) -> Duration {
        let exponent = u32::try_from(failed_attempt.saturating_sub(1))
            .unwrap_or(u32::MAX)
            .min(31);
        let base = self
            .initial_delay
            .saturating_mul(1_u32 << exponent)
            .min(self.max_delay);
        if base.is_zero() || base == self.max_delay {
            return base;
        }

        // A small additive jitter prevents concurrent builders from retrying
        let window = base / 4;
        let window_nanos = u64::try_from(window.as_nanos()).unwrap_or(u64::MAX);
        let entropy = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let jitter = Duration::from_nanos(entropy % window_nanos.saturating_add(1));
        base.saturating_add(jitter).min(self.max_delay)
    }
}

fn is_retryable_registry_error(error: &OciDistributionError) -> bool {
    match error {
        OciDistributionError::RequestError(error) => {
            error.is_timeout()
                || error.is_connect()
                || error.is_body()
                || error
                    .status()
                    .is_some_and(|status| is_retryable_status(status.as_u16()))
        }
        OciDistributionError::ServerError { code, .. } => is_retryable_status(*code),
        OciDistributionError::RegistryError { envelope, .. } => {
            !envelope.errors.is_empty()
                && envelope
                    .errors
                    .iter()
                    .all(|error| error.code == OciErrorCode::Toomanyrequests)
        }
        OciDistributionError::IoError(error) => is_retryable_io_error(error),
        _ => false,
    }
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502..=599)
}

fn is_retryable_io_error(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;

    match error.kind() {
        ErrorKind::BrokenPipe
        | ErrorKind::ConnectionAborted
        | ErrorKind::ConnectionRefused
        | ErrorKind::ConnectionReset
        | ErrorKind::HostUnreachable
        | ErrorKind::Interrupted
        | ErrorKind::NetworkDown
        | ErrorKind::NetworkUnreachable
        | ErrorKind::NotConnected
        | ErrorKind::TimedOut
        | ErrorKind::UnexpectedEof
        | ErrorKind::WouldBlock => true,
        ErrorKind::Other => error.get_ref().is_some_and(|source| {
            source
                .downcast_ref::<oci_client::errors::DigestError>()
                .is_none()
        }),
        _ => false,
    }
}

/// Bridge oci-client's descriptor type to oci-spec's
fn to_oci_spec_descriptor(d: &OciDescriptor) -> Result<Descriptor> {
    let media_type = MediaType::from(d.media_type.as_str());
    let size: u64 =
        u64::try_from(d.size).map_err(|_| anyhow!("descriptor size is negative: {}", d.size))?;
    let digest: oci_spec::image::Digest = d
        .digest
        .parse()
        .map_err(|e| anyhow!("malformed descriptor digest {}: {}", d.digest, e))?;
    let mut b = DescriptorBuilder::default()
        .media_type(media_type)
        .digest(digest)
        .size(size);
    if let Some(urls) = &d.urls {
        b = b.urls(urls.clone());
    }
    if let Some(ann) = &d.annotations {
        b = b.annotations(
            ann.clone()
                .into_iter()
                .collect::<std::collections::HashMap<_, _>>(),
        );
    }
    if let Some(artifact_type) = &d.artifact_type {
        b = b.artifact_type(MediaType::from(artifact_type.as_str()));
    }
    b.build().map_err(|e| anyhow!("building descriptor: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oci_spec::image::{Arch, ImageConfigurationBuilder, Os, RootFsBuilder};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    fn retry_policy_for_test(max_attempts: usize) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    fn server_error(code: u16) -> OciDistributionError {
        OciDistributionError::ServerError {
            code,
            url: "https://registry.example.test/v2/".to_string(),
            message: "fixture".to_string(),
        }
    }

    fn image_config(os: Os, architecture: Arch, variant: Option<&str>) -> ImageConfiguration {
        let rootfs = RootFsBuilder::default()
            .diff_ids(Vec::<String>::new())
            .build()
            .unwrap();
        let mut builder = ImageConfigurationBuilder::default()
            .os(os)
            .architecture(architecture)
            .rootfs(rootfs);
        if let Some(variant) = variant {
            builder = builder.variant(variant.to_string());
        }
        builder.build().unwrap()
    }

    #[test]
    fn descriptor_bridge_round_trips_basic_fields() {
        let oc = OciDescriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
            digest: format!("sha256:{}", "a".repeat(64)),
            size: 1234,
            urls: Some(vec!["https://example.com/blob".into()]),
            annotations: None,
            artifact_type: None,
        };
        let d = to_oci_spec_descriptor(&oc).unwrap();
        assert_eq!(d.size(), 1234);
        assert_eq!(d.urls().as_ref().unwrap().len(), 1);
        assert!(matches!(d.media_type(), MediaType::ImageLayerGzip));
    }

    #[test]
    fn descriptor_bridge_keeps_unknown_media_type() {
        let oc = OciDescriptor {
            media_type: "application/vnd.example.weird+stuff".into(),
            digest: "sha256:".to_string() + &"a".repeat(64),
            size: 1,
            urls: None,
            annotations: None,
            artifact_type: None,
        };
        let d = to_oci_spec_descriptor(&oc).unwrap();
        match d.media_type() {
            MediaType::Other(s) => assert_eq!(s, "application/vnd.example.weird+stuff"),
            other => panic!("unexpected media type: {:?}", other),
        }
    }

    #[test]
    fn descriptor_rejects_negative_size() {
        let oc = OciDescriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
            digest: "sha256:".to_string() + &"a".repeat(64),
            size: -1,
            urls: None,
            annotations: None,
            artifact_type: None,
        };
        assert!(to_oci_spec_descriptor(&oc).is_err());
    }

    #[test]
    fn base_config_must_match_requested_platform() {
        let config = image_config(Os::Linux, Arch::Amd64, None);
        assert!(validate_platform(&Platform::parse("linux/amd64").unwrap(), &config).is_ok());

        let error = validate_platform(&Platform::parse("linux/arm64").unwrap(), &config)
            .unwrap_err()
            .to_string();
        assert!(error.contains("linux/amd64"));
        assert!(error.contains("linux/arm64"));
    }

    #[test]
    fn platform_variant_is_checked_when_both_sides_provide_one() {
        let config = image_config(Os::Linux, Arch::ARM64, Some("v8"));
        assert!(validate_platform(&Platform::parse("linux/arm64/v8").unwrap(), &config).is_ok());
        assert!(validate_platform(&Platform::parse("linux/arm64/v7").unwrap(), &config).is_err());
    }

    #[test]
    fn layer_digest_mismatch_is_a_non_retryable_error() {
        let mut hasher = Sha256::new();
        hasher.update(b"tampered bytes");
        let expected = format!("sha256:{}", "0".repeat(64));
        let error = verify_layer_digest(&expected, hasher).unwrap_err();
        assert!(!is_retryable_registry_error(&error));
        // The message must name both digests so the user can compare them
        let message = error.to_string();
        assert!(message.contains(&expected), "unexpected error: {message}");
        assert!(
            message.contains(sha256_digest(b"tampered bytes").as_ref()),
            "unexpected error: {message}"
        );

        let mut hasher = Sha256::new();
        hasher.update(b"payload");
        assert!(verify_layer_digest(sha256_digest(b"payload").as_ref(), hasher).is_ok());
    }

    #[test]
    fn retry_classifier_rejects_integrity_and_authentication_errors() {
        let digest_error = oci_client::errors::DigestError::VerificationError {
            expected: "sha256:expected".to_string(),
            actual: "sha256:actual".to_string(),
        };
        let digest_io = OciDistributionError::IoError(std::io::Error::other(digest_error));
        assert!(!is_retryable_registry_error(&digest_io));
        assert!(!is_retryable_registry_error(
            &OciDistributionError::UnauthorizedError {
                url: "https://registry.example.test/v2/".to_string()
            }
        ));
        assert!(is_retryable_registry_error(&server_error(429)));
        assert!(is_retryable_registry_error(&server_error(503)));
    }

    #[test]
    fn retry_delay_has_bounded_jitter() {
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(1),
        };

        let delay = policy.delay_after(1);

        assert!(delay >= Duration::from_millis(100));
        assert!(delay <= Duration::from_millis(125));
    }

    #[tokio::test]
    async fn transient_registry_errors_are_retried_with_a_bound() {
        let attempts = AtomicUsize::new(0);
        let result = retry_registry("fixture", retry_policy_for_test(3), || {
            let attempt = attempts.fetch_add(1, AtomicOrdering::Relaxed);
            std::future::ready(if attempt < 2 {
                Err(server_error(503))
            } else {
                Ok(42)
            })
        })
        .await
        .unwrap();

        assert_eq!(result, 42);
        assert_eq!(attempts.load(AtomicOrdering::Relaxed), 3);
    }

    #[tokio::test]
    async fn permanent_registry_errors_are_not_retried() {
        let attempts = AtomicUsize::new(0);
        let result = retry_registry("fixture", retry_policy_for_test(3), || {
            attempts.fetch_add(1, AtomicOrdering::Relaxed);
            std::future::ready(Err::<(), _>(OciDistributionError::UnauthorizedError {
                url: "https://registry.example.test/v2/".to_string(),
            }))
        })
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.load(AtomicOrdering::Relaxed), 1);
    }

    #[tokio::test]
    async fn upload_reader_emits_one_item_per_configured_chunk() {
        let file = tempfile::tempfile().unwrap();
        file.set_len((UPLOAD_CHUNK_SIZE * 2 + 1) as u64).unwrap();
        let mut async_file = tokio::fs::File::from_std(file);
        async_file.set_max_buf_size(UPLOAD_CHUNK_SIZE);
        let mut stream = ReaderStream::with_capacity(async_file, UPLOAD_CHUNK_SIZE);
        let mut sizes = Vec::new();

        while let Some(chunk) = stream.try_next().await.unwrap() {
            sizes.push(chunk.len());
        }

        assert_eq!(sizes, [UPLOAD_CHUNK_SIZE, UPLOAD_CHUNK_SIZE, 1]);
    }
}
