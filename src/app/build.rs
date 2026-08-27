use std::sync::Arc;
use std::time::Duration;
use std::{collections::BTreeMap, io::IsTerminal};

use anyhow::{Context, Result, bail};
use indicatif::HumanBytes;
use oci_client::Reference;
use tokio_util::sync::CancellationToken;

use super::push::{LocalLayers, push_registry};
use crate::archive::{write_docker_archive, write_oci_archive};
use crate::auth::Credentials;
use crate::blob::{DownloadedLayers, LayerProvider};
use crate::builder::copy::CopySpec;
use crate::builder::layer::build_copy_layer_with_options;
use crate::cache::CacheController;
use crate::digest::short_digest;
use crate::image::assemble::{AssembleOptions, ImageBundle, assemble};
use crate::image::layer::Layer;
use crate::image::{Image, Platform};
use crate::output_target::OutputTarget;
use crate::progress::Progress;
use crate::registry::{RegistryClient, RegistryOptions};

#[derive(Debug, Clone)]
pub(crate) struct CopyDirective {
    description: String,
    specification: CopySpec,
}

impl CopyDirective {
    pub(crate) fn parse(raw: String) -> Result<Self> {
        let specification =
            CopySpec::parse(&raw).with_context(|| format!("parsing COPY directive {raw:?}"))?;
        Ok(Self::new(raw, specification))
    }

    pub(crate) fn new(description: String, specification: CopySpec) -> Self {
        Self {
            description,
            specification,
        }
    }

    #[cfg(test)]
    pub(crate) fn description(&self) -> &str {
        &self.description
    }

    #[cfg(test)]
    pub(crate) fn specification(&self) -> &CopySpec {
        &self.specification
    }

    fn into_parts(self) -> (String, CopySpec) {
        (self.description, self.specification)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RegistryClientSpec {
    pub(crate) source_plain_http: bool,
    pub(crate) source_skip_tls: bool,
    pub(crate) target_plain_http: bool,
    pub(crate) target_skip_tls: bool,
    pub(crate) connect_timeout: Duration,
    pub(crate) read_timeout: Duration,
    pub(crate) max_attempts: usize,
}

impl Default for RegistryClientSpec {
    fn default() -> Self {
        Self {
            source_plain_http: false,
            source_skip_tls: false,
            target_plain_http: false,
            target_skip_tls: false,
            connect_timeout: crate::registry::DEFAULT_CONNECT_TIMEOUT,
            read_timeout: crate::registry::DEFAULT_READ_TIMEOUT,
            max_attempts: crate::registry::DEFAULT_MAX_ATTEMPTS,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CacheSpec {
    pub(crate) enable: bool,
    pub(crate) cache_path: Option<String>,
}

/// Parse a shell-quoted command line into argv, so values like
/// `/bin/sh -c "echo hello world"` keep quoted words together
pub(crate) fn parse_argv(line: &str) -> Result<Vec<String>> {
    shlex::split(line)
        .ok_or_else(|| anyhow::anyhow!("invalid shell quoting in command line {line:?}"))
}

#[derive(Debug)]
pub(crate) struct ImageBuildSpec {
    pub(crate) from: String,
    pub(crate) targets: Vec<OutputTarget>,
    pub(crate) copies: Vec<CopyDirective>,
    pub(crate) jobs: usize,
    pub(crate) platform: String,
    pub(crate) entrypoint: Option<Vec<String>>,
    pub(crate) cmd: Option<Vec<String>>,
    pub(crate) labels: Vec<String>,
    pub(crate) ports: Vec<String>,
    pub(crate) workdir: Option<String>,
    pub(crate) user: Option<String>,
    pub(crate) creation_time: String,
    pub(crate) keep_cmd: bool,
    pub(crate) transport: RegistryClientSpec,
    pub(crate) cache: CacheSpec,
}

pub(crate) fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(4)
}

pub struct BuildRequest {
    source: Option<Reference>,
    targets: Vec<OutputTarget>,
    copies: Vec<CopyDirective>,
    jobs: usize,
    platform: Platform,
    entrypoint: Option<Vec<String>>,
    cmd: Option<Vec<String>>,
    labels: BTreeMap<String, String>,
    ports: Vec<String>,
    workdir: Option<String>,
    user: Option<String>,
    creation_time: Option<String>,
    keep_cmd: bool,
    source_registry: RegistryOptions,
    target_registry: RegistryOptions,
    cache: CacheSpec,
}

impl TryFrom<ImageBuildSpec> for BuildRequest {
    type Error = anyhow::Error;

    fn try_from(specification: ImageBuildSpec) -> Result<Self> {
        if specification.jobs == 0 {
            bail!("jobs must be greater than zero");
        }
        if specification.transport.max_attempts == 0 {
            bail!("max attempts must be greater than zero");
        }
        if specification.transport.connect_timeout.is_zero()
            || specification.transport.read_timeout.is_zero()
        {
            bail!("registry timeouts must be greater than zero");
        }
        if specification.targets.is_empty() {
            bail!("at least one output target is required");
        }
        let source = if specification.from == "scratch" {
            None
        } else {
            Some(specification.from.parse().with_context(|| {
                format!("parsing source image reference {:?}", specification.from)
            })?)
        };
        let labels = specification
            .labels
            .into_iter()
            .map(|label| {
                let (key, value) = label.split_once('=').ok_or_else(|| {
                    anyhow::anyhow!("invalid label {label:?}; expected key=value")
                })?;
                Ok((key.to_string(), value.to_string()))
            })
            .collect::<Result<_>>()?;
        let ports = specification
            .ports
            .into_iter()
            .map(|port| {
                if port.contains('/') {
                    port
                } else {
                    format!("{port}/tcp")
                }
            })
            .collect();
        let (source_registry, target_registry) = registry_options(
            source.as_ref(),
            &specification.targets,
            specification.transport,
        );

        Ok(Self {
            source,
            targets: specification.targets,
            copies: specification.copies,
            jobs: specification.jobs,
            platform: Platform::parse(&specification.platform)?,
            entrypoint: specification.entrypoint,
            cmd: specification.cmd,
            labels,
            ports,
            workdir: specification.workdir,
            user: specification.user,
            creation_time: parse_creation_time(&specification.creation_time)?,
            keep_cmd: specification.keep_cmd,
            source_registry,
            target_registry,
            cache: specification.cache,
        })
    }
}

impl BuildRequest {
    pub async fn run(self, credentials: Credentials, progress: Progress) -> Result<()> {
        report_transport_options(&self.source_registry, &self.target_registry, &progress);
        let source_client = if self.source.is_some() {
            Some(
                RegistryClient::with_options(
                    credentials.clone(),
                    self.platform.clone(),
                    self.source_registry,
                )
                .context("creating source registry client")?,
            )
        } else {
            None
        };
        let target_client =
            RegistryClient::with_options(credentials, self.platform.clone(), self.target_registry)
                .context("creating target registry client")?;

        let resolve_base = async {
            match (&self.source, source_client.as_ref()) {
                (Some(source), Some(client)) => {
                    let task = progress.spinner(format!("pull manifest {source}"));
                    let base = client.pull_base(source).await?;
                    task.done_with(format!("{} layer(s)", base.layers.len()));
                    Ok::<_, anyhow::Error>(base)
                }
                (None, None) => {
                    let task = progress.spinner(format!("initialize scratch ({})", self.platform));
                    let base = Image::scratch(&self.platform)?;
                    task.done_with("empty base");
                    Ok(base)
                }
                _ => unreachable!("source reference and client are created together"),
            }
        };
        let retain_uncompressed = self
            .targets
            .iter()
            .any(|target| matches!(target, OutputTarget::DockerArchive { .. }));
        let copies = build_copy_layers(
            self.copies,
            self.jobs,
            retain_uncompressed,
            progress.clone(),
        );
        let (base, new_layers) = tokio::try_join!(resolve_base, copies)?;

        let cache_controller = if self.cache.enable {
            Some(CacheController::new(self.cache.cache_path.as_deref())?)
        } else {
            None
        };

        let downloaded_layers = if self.targets.iter().any(OutputTarget::is_archive) {
            let layers = match source_client.as_ref() {
                Some(client) => {
                    DownloadedLayers::fetch(
                        client,
                        &base,
                        self.jobs,
                        cache_controller.clone(),
                        &progress,
                    )
                    .await?
                }
                None => DownloadedLayers::empty(),
            };
            Some(Arc::new(layers))
        } else {
            None
        };

        let task = progress.spinner("assemble manifest + config");
        let bundle = assemble(AssembleOptions {
            base,
            new_layers,
            entrypoint: self.entrypoint,
            cmd: self.cmd,
            labels: self.labels,
            ports: self.ports,
            workdir: self.workdir,
            user: self.user,
            creation_time: self.creation_time,
            keep_cmd: self.keep_cmd,
        })?;
        task.done_with(format!(
            "manifest {}",
            short_digest(&bundle.manifest_digest)
        ));

        for target in self.targets {
            match target {
                OutputTarget::OciArchive { path, tag } => {
                    write_archive_target(
                        "oci-archive",
                        write_oci_archive,
                        &bundle,
                        &downloaded_layers,
                        path,
                        tag,
                        &progress,
                    )
                    .await?;
                }
                OutputTarget::DockerArchive { path, tag } => {
                    write_archive_target(
                        "docker-archive",
                        write_docker_archive,
                        &bundle,
                        &downloaded_layers,
                        path,
                        tag,
                        &progress,
                    )
                    .await?;
                }
                OutputTarget::Registry { reference } => {
                    push_registry(
                        source_client.as_ref(),
                        &target_client,
                        &reference,
                        &bundle,
                        LocalLayers {
                            downloaded: downloaded_layers.clone(),
                            cache: cache_controller.clone(),
                        },
                        self.jobs,
                        progress.clone(),
                    )
                    .await?;
                }
            }
        }

        print_summary(&bundle);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn copies(&self) -> &[CopyDirective] {
        &self.copies
    }

    #[cfg(test)]
    pub(crate) fn cmd(&self) -> Option<&[String]> {
        self.cmd.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn entrypoint(&self) -> Option<&[String]> {
        self.entrypoint.as_deref()
    }
}

async fn write_archive_target(
    kind: &str,
    write: fn(&ImageBundle, &dyn LayerProvider, &std::path::Path, Option<&str>) -> Result<()>,
    bundle: &ImageBundle,
    downloaded_layers: &Option<Arc<DownloadedLayers>>,
    path: std::path::PathBuf,
    tag: Option<String>,
    progress: &Progress,
) -> Result<()> {
    let task = progress.spinner(format!("write {kind} → {}", path.display()));
    let bundle = bundle.clone();
    let layers = downloaded_layers
        .clone()
        .expect("archive layers are fetched when an archive target exists");
    let archive_path = path.clone();
    tokio::task::spawn_blocking(move || {
        write(&bundle, layers.as_ref(), &archive_path, tag.as_deref())
    })
    .await
    .with_context(|| format!("{kind} worker panicked"))??;
    task.done_with(HumanBytes(std::fs::metadata(path)?.len()).to_string());
    Ok(())
}

fn registry_options(
    source: Option<&Reference>,
    targets: &[OutputTarget],
    transport: RegistryClientSpec,
) -> (RegistryOptions, RegistryOptions) {
    let source = RegistryOptions {
        plain_http_hosts: source
            .filter(|_| transport.source_plain_http)
            .map(|source| source.resolve_registry().to_string())
            .into_iter()
            .collect(),
        skip_tls_verify: source.is_some() && transport.source_skip_tls,
        connect_timeout: transport.connect_timeout,
        read_timeout: transport.read_timeout,
        max_attempts: transport.max_attempts,
    };
    let mut target_hosts: Vec<String> = if transport.target_plain_http {
        targets
            .iter()
            .filter_map(|target| match target {
                OutputTarget::Registry { reference } => {
                    Some(reference.resolve_registry().to_string())
                }
                _ => None,
            })
            .collect()
    } else {
        Vec::new()
    };
    target_hosts.sort();
    target_hosts.dedup();
    let target = RegistryOptions {
        plain_http_hosts: target_hosts,
        skip_tls_verify: transport.target_skip_tls,
        connect_timeout: transport.connect_timeout,
        read_timeout: transport.read_timeout,
        max_attempts: transport.max_attempts,
    };
    (source, target)
}

fn report_transport_options(
    source: &RegistryOptions,
    target: &RegistryOptions,
    progress: &Progress,
) {
    let mut plain_http_hosts = source.plain_http_hosts.clone();
    plain_http_hosts.extend(target.plain_http_hosts.iter().cloned());
    plain_http_hosts.sort();
    plain_http_hosts.dedup();
    if !plain_http_hosts.is_empty() {
        progress.note(format!("plain http: {}", plain_http_hosts.join(", ")));
    }
    if source.skip_tls_verify || target.skip_tls_verify {
        progress.note("⚠ TLS verification disabled (--*-skip-tls)");
    }
}

async fn build_copy_layers(
    copies: Vec<CopyDirective>,
    jobs: usize,
    retain_uncompressed: bool,
    progress: Progress,
) -> Result<Vec<Layer>> {
    if copies.is_empty() {
        return Ok(Vec::new());
    }

    let count = copies.len();
    let cancellation = CancellationToken::new();
    let cancel_on_drop = cancellation.clone().drop_guard();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(jobs));
    let mut tasks = tokio::task::JoinSet::new();
    for (index, directive) in copies.into_iter().enumerate() {
        let (description, specification) = directive.into_parts();
        let semaphore = semaphore.clone();
        let cancellation = cancellation.clone();
        let task = progress.spinner(format!("copy [{}/{}] {description}", index + 1, count));
        tasks.spawn(async move {
            let permit = semaphore
                .acquire_owned()
                .await
                .context("COPY worker semaphore closed")?;
            let layer = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                build_copy_layer_with_options(&specification, retain_uncompressed, cancellation)
            })
            .await
            .with_context(|| format!("COPY worker panicked for {description:?}"))?
            .with_context(|| format!("building COPY layer {description:?}"))?;
            task.done_with(format!(
                "{} → {}",
                HumanBytes(layer.uncompressed_size),
                HumanBytes(layer.size())
            ));
            Ok::<_, anyhow::Error>((index, layer))
        });
    }

    let mut ordered = vec![None; count];
    while let Some(result) = tasks.join_next().await {
        let error = match result {
            Ok(Ok((index, layer))) => {
                ordered[index] = Some(layer);
                continue;
            }
            Ok(Err(error)) => error,
            Err(error) => anyhow::Error::new(error).context("COPY coordinator task failed"),
        };
        cancellation.cancel();
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        return Err(error);
    }
    let result: Result<Vec<Layer>> = ordered
        .into_iter()
        .enumerate()
        .map(|(index, layer)| {
            layer.ok_or_else(|| anyhow::anyhow!("COPY layer {index} produced no result"))
        })
        .collect();
    if result.is_ok() {
        cancel_on_drop.disarm();
    }
    result
}

fn parse_creation_time(value: &str) -> Result<Option<String>> {
    match value {
        "epoch" => Ok(None),
        "now" => Ok(Some(
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )),
        value => {
            chrono::DateTime::parse_from_rfc3339(value).map_err(|error| {
                anyhow::anyhow!(
                    "invalid creation time {value:?} (expected `epoch`, `now`, or RFC3339): {error}"
                )
            })?;
            Ok(Some(value.to_string()))
        }
    }
}

fn print_summary(bundle: &ImageBundle) {
    eprintln!();
    eprintln!("layers:           {}", bundle.layers.len());
    eprintln!("config digest:    {}", bundle.config_digest);
    eprintln!("image digest:     {}", bundle.manifest_digest);
    if !std::io::stdout().is_terminal() {
        print!("{}", bundle.manifest_digest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::ProgressMode;
    use tempfile::TempDir;

    #[test]
    fn argv_lines_support_shell_quoting() {
        assert_eq!(
            parse_argv(r#"/bin/sh -c "echo hello world""#).unwrap(),
            vec!["/bin/sh", "-c", "echo hello world"]
        );
        assert_eq!(
            parse_argv("java -jar app.jar").unwrap(),
            vec!["java", "-jar", "app.jar"]
        );
        assert!(parse_argv(r#"unterminated "quote"#).is_err());
    }

    #[test]
    fn zero_transport_values_are_rejected() {
        let target = "oci-archive:image.tar".parse::<OutputTarget>().unwrap();
        let specification = |transport: RegistryClientSpec| ImageBuildSpec {
            from: "scratch".to_string(),
            targets: vec![target.clone()],
            copies: Vec::new(),
            jobs: 1,
            platform: "linux/amd64".to_string(),
            entrypoint: None,
            cmd: None,
            labels: Vec::new(),
            ports: Vec::new(),
            workdir: None,
            user: None,
            creation_time: "epoch".to_string(),
            keep_cmd: false,
            transport,
            cache: CacheSpec::default(),
        };

        let zero_attempts = RegistryClientSpec {
            max_attempts: 0,
            ..RegistryClientSpec::default()
        };
        assert!(
            BuildRequest::try_from(specification(zero_attempts))
                .map(|_| ())
                .unwrap_err()
                .to_string()
                .contains("max attempts")
        );

        let zero_timeout = RegistryClientSpec {
            read_timeout: Duration::ZERO,
            ..RegistryClientSpec::default()
        };
        assert!(
            BuildRequest::try_from(specification(zero_timeout))
                .map(|_| ())
                .unwrap_err()
                .to_string()
                .contains("timeouts")
        );

        assert!(BuildRequest::try_from(specification(RegistryClientSpec::default())).is_ok());
    }

    #[test]
    fn creation_time_modes_are_validated() {
        assert!(parse_creation_time("epoch").unwrap().is_none());
        let now = parse_creation_time("now").unwrap().unwrap();
        assert!(chrono::DateTime::parse_from_rfc3339(&now).is_ok());
        assert_eq!(
            parse_creation_time("2024-01-15T12:00:00Z").unwrap(),
            Some("2024-01-15T12:00:00Z".to_string())
        );
        assert!(parse_creation_time("yesterday").is_err());
    }

    #[tokio::test]
    async fn concurrent_copy_keeps_input_order() {
        let directory = TempDir::new().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        let copies = vec![
            CopyDirective::new(
                first.display().to_string(),
                CopySpec::parse(&format!("{}:/same", first.display())).unwrap(),
            ),
            CopyDirective::new(
                second.display().to_string(),
                CopySpec::parse(&format!("{}:/same", second.display())).unwrap(),
            ),
        ];
        let layers = build_copy_layers(copies, 2, false, Progress::new(ProgressMode::Quiet))
            .await
            .unwrap();
        assert_eq!(layers.len(), 2);
        assert_ne!(layers[0].descriptor.digest(), layers[1].descriptor.digest());
    }

    #[test]
    fn registry_transport_options_are_role_scoped() {
        let source: Reference = "source.test/base:latest".parse().unwrap();
        let targets = vec![
            "registry:target.test/app:v1"
                .parse::<OutputTarget>()
                .unwrap(),
            "registry:target.test/other:v1"
                .parse::<OutputTarget>()
                .unwrap(),
        ];
        let (source_options, target_options) = registry_options(
            Some(&source),
            &targets,
            RegistryClientSpec {
                source_plain_http: true,
                source_skip_tls: true,
                target_plain_http: true,
                target_skip_tls: false,
                read_timeout: Duration::from_secs(30),
                connect_timeout: Duration::from_secs(60),
                max_attempts: 3,
            },
        );

        assert_eq!(source_options.plain_http_hosts, vec!["source.test"]);
        assert!(source_options.skip_tls_verify);
        assert_eq!(target_options.plain_http_hosts, vec!["target.test"]);
        assert!(!target_options.skip_tls_verify);
    }

    #[test]
    fn registry_transport_defaults_are_strict() {
        let source: Reference = "source.test/base:latest".parse().unwrap();
        let (source_options, target_options) =
            registry_options(Some(&source), &[], RegistryClientSpec::default());
        assert!(source_options.plain_http_hosts.is_empty());
        assert!(!source_options.skip_tls_verify);
        assert!(target_options.plain_http_hosts.is_empty());
        assert!(!target_options.skip_tls_verify);
    }

    #[test]
    fn exact_scratch_is_not_parsed_as_a_registry_reference() {
        let target = "oci-archive:image.tar".parse::<OutputTarget>().unwrap();
        let specification = |from: &str| ImageBuildSpec {
            from: from.to_string(),
            targets: vec![target.clone()],
            copies: Vec::new(),
            jobs: 1,
            platform: "linux/amd64".to_string(),
            entrypoint: None,
            cmd: None,
            labels: Vec::new(),
            ports: Vec::new(),
            workdir: None,
            user: None,
            creation_time: "epoch".to_string(),
            keep_cmd: false,
            transport: RegistryClientSpec::default(),
            cache: CacheSpec {
                enable: false,
                cache_path: None,
            },
        };

        assert!(
            BuildRequest::try_from(specification("scratch"))
                .unwrap()
                .source
                .is_none()
        );
        assert!(
            BuildRequest::try_from(specification("scratch:latest"))
                .unwrap()
                .source
                .is_some()
        );
    }

    #[test]
    fn scratch_ignores_source_registry_transport_flags() {
        let (source_options, _) = registry_options(
            None,
            &[],
            RegistryClientSpec {
                source_plain_http: true,
                source_skip_tls: true,
                ..RegistryClientSpec::default()
            },
        );

        assert!(source_options.plain_http_hosts.is_empty());
        assert!(!source_options.skip_tls_verify);
    }
}
