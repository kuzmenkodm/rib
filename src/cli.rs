use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::app::build::{
    CacheSpec, CopyDirective, ImageBuildSpec, RegistryClientSpec, default_jobs, parse_argv,
};
use crate::output_target::OutputTarget;
use crate::progress::ProgressMode;
use crate::registry::{DEFAULT_CONNECT_TIMEOUT, DEFAULT_MAX_ATTEMPTS, DEFAULT_READ_TIMEOUT};

#[derive(Parser)]
#[command(name = "rib", version, about = "Minimal jib-like OCI image builder")]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

/// Arguments shared verbatim between `rib` (where they are global) and
/// `cargo rib build`
#[derive(Args)]
pub struct GlobalArgs {
    /// Path to docker config.json. Default: $DOCKER_CONFIG/config.json or ~/.docker/config.json
    #[arg(long, global = true)]
    pub docker_config: Option<PathBuf>,

    /// Explicit credential, repeatable. Form: <registry>=<user>:<pass>
    /// Takes precedence over docker config
    #[arg(long = "credential", global = true)]
    pub credential: Vec<String>,

    /// Progress output format
    #[arg(long, global = true, value_enum, default_value_t = ProgressMode::Auto)]
    pub progress: ProgressMode,
}

#[derive(Subcommand)]
pub enum Command {
    /// Build an image and write it to one or more outputs
    Build(Box<BuildArgs>),
}

/// Container-config arguments accepted by both `rib build` and
/// `cargo rib build`
#[derive(Args)]
pub struct ImageConfigArgs {
    /// Entrypoint argv as one shell-quoted line
    /// Quotes group words: '/bin/sh -c "echo hello world"'
    #[arg(long = "entrypoint", verbatim_doc_comment)]
    pub entrypoint: Option<String>,

    /// Cmd argv as one shell-quoted line, like the entrypoint
    #[arg(long = "cmd")]
    pub cmd: Option<String>,

    /// Image label. Repeatable. Form: key=value
    #[arg(long = "label")]
    pub labels: Vec<String>,

    /// Exposed port. Repeatable. Form: 8080 or 8080/tcp
    #[arg(long = "port")]
    pub ports: Vec<String>,

    /// Working directory for the container
    #[arg(long)]
    pub workdir: Option<String>,

    /// User the container runs as
    #[arg(long)]
    pub user: Option<String>,

    /// Keep the base image Cmd when overriding entrypoint
    #[arg(long)]
    pub keep_cmd: bool,
}

/// Registry transport arguments shared by both binaries. Timeouts stay
/// `Option` so `cargo rib` can tell "user passed a flag" apart from the
/// default and let the flag override `package.metadata.rib`
#[derive(Args)]
pub struct RegistryClientArgs {
    /// Use HTTP for the source registry
    #[arg(long)]
    pub from_plain_http: bool,

    /// Disable TLS certificate verification for the source registry
    /// INSECURE: accepts any certificate, including a forged one, so a
    /// network attacker could serve a malicious base image undetected. The
    /// underlying HTTP client has no per-host toggle, so this weakens every
    /// request this build makes to the source registry. Use only for a
    /// trusted local or development registry, never in production
    #[arg(long)]
    pub from_skip_tls: bool,

    /// Use HTTP for registry output targets
    #[arg(long)]
    pub to_plain_http: bool,

    /// Disable TLS certificate verification for registry output targets
    /// INSECURE: accepts any certificate, including a forged one. All `--to
    /// registry:...` targets in this run share one HTTP client, so this
    /// weakens TLS for every target registry, not just the one you intend
    /// Use only for a trusted local or development registry, never in
    /// production
    #[arg(long)]
    pub to_skip_tls: bool,

    /// Registry read timeout in seconds [default: 60]
    #[arg(long)]
    pub read_timeout: Option<u64>,

    /// Registry connect timeout in seconds [default: 30]
    #[arg(long)]
    pub connect_timeout: Option<u64>,

    /// Attempts per registry operation before giving up [default: 3]
    #[arg(long)]
    pub max_attempts: Option<usize>,
}

impl From<RegistryClientArgs> for RegistryClientSpec {
    fn from(arguments: RegistryClientArgs) -> Self {
        Self {
            source_plain_http: arguments.from_plain_http,
            source_skip_tls: arguments.from_skip_tls,
            target_plain_http: arguments.to_plain_http,
            target_skip_tls: arguments.to_skip_tls,
            connect_timeout: arguments
                .connect_timeout
                .map_or(DEFAULT_CONNECT_TIMEOUT, Duration::from_secs),
            read_timeout: arguments
                .read_timeout
                .map_or(DEFAULT_READ_TIMEOUT, Duration::from_secs),
            max_attempts: arguments.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS),
        }
    }
}

/// Layer-cache arguments shared by both binaries
#[derive(Args)]
pub struct CacheArgs {
    /// Cache downloaded base layers. Uses ./.rib-cache unless --cache-path is set
    #[arg(long = "cache")]
    pub cache: bool,

    /// Directory for cached layers
    #[arg(long = "cache-path")]
    pub cache_path: Option<String>,
}

impl From<CacheArgs> for CacheSpec {
    fn from(arguments: CacheArgs) -> Self {
        Self {
            enable: arguments.cache,
            cache_path: arguments.cache_path,
        }
    }
}

#[derive(Args)]
pub struct BuildArgs {
    /// Base image reference or the exact value `scratch`
    #[arg(long)]
    pub from: String,

    /// Output target. Repeatable. Forms:
    ///   oci-archive:<path>[@<tag>]
    ///   docker-archive:<path>[@<tag>]
    ///   registry:<image-ref>
    #[arg(long = "to", required = true, verbatim_doc_comment)]
    pub targets: Vec<OutputTarget>,

    /// Copy a local path into the image. Repeatable; each --copy
    /// becomes its own layer in flag order
    ///
    /// Form: <src>:<dst>[,key=value,...]
    ///
    /// Destination semantics follow Dockerfile COPY:
    ///   /app/              destination directory
    ///   /app/file         exact destination path for a single file
    ///   <directory>:/app/ copy directory contents into /app/
    ///
    /// Quote globs so the shell does not expand them before rib:
    ///   --copy 'target/release/*.so:/lib/'
    ///
    /// Options:
    ///   mode=<octal>          file mode, for example mode=0755
    ///   chown=<uid>[:<gid>]   ownership; gid defaults to 0
    #[arg(long = "copy", verbatim_doc_comment)]
    pub copies: Vec<String>,

    /// Maximum number of COPY layers and blob transfers processed concurrently
    #[arg(long, default_value_t = default_jobs())]
    pub jobs: usize,

    #[command(flatten)]
    pub image: ImageConfigArgs,

    /// Target platform
    #[arg(long, default_value = "linux/amd64")]
    pub platform: String,

    /// Image creation timestamp: epoch, now, or an RFC3339 value
    #[arg(long, default_value = "epoch")]
    pub creation_time: String,

    #[command(flatten)]
    pub client: RegistryClientArgs,

    #[command(flatten)]
    pub cache: CacheArgs,
}

impl TryFrom<BuildArgs> for ImageBuildSpec {
    type Error = anyhow::Error;

    fn try_from(arguments: BuildArgs) -> Result<Self> {
        let copies = arguments
            .copies
            .into_iter()
            .map(CopyDirective::parse)
            .collect::<Result<_>>()?;

        Ok(Self {
            from: arguments.from,
            targets: arguments.targets,
            copies,
            jobs: arguments.jobs,
            platform: arguments.platform,
            entrypoint: arguments
                .image
                .entrypoint
                .as_deref()
                .map(parse_argv)
                .transpose()?,
            cmd: arguments.image.cmd.as_deref().map(parse_argv).transpose()?,
            labels: arguments.image.labels,
            ports: arguments.image.ports,
            workdir: arguments.image.workdir,
            user: arguments.image.user,
            creation_time: arguments.creation_time,
            keep_cmd: arguments.image.keep_cmd,
            transport: arguments.client.into(),
            cache: arguments.cache.into(),
        })
    }
}

pub fn parse() -> Cli {
    Cli::parse()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn global_arguments_parse_after_the_subcommand() {
        let cli = Cli::try_parse_from([
            "rib",
            "build",
            "--from",
            "scratch",
            "--to",
            "oci-archive:image.tar",
            "--docker-config",
            "/tmp/config.json",
            "--progress",
            "quiet",
        ])
        .unwrap();
        assert_eq!(
            cli.global.docker_config.as_deref(),
            Some(Path::new("/tmp/config.json"))
        );
    }

    #[test]
    fn registry_client_arguments_fall_back_to_defaults() {
        let spec: RegistryClientSpec = RegistryClientArgs {
            from_plain_http: false,
            from_skip_tls: false,
            to_plain_http: false,
            to_skip_tls: false,
            read_timeout: None,
            connect_timeout: None,
            max_attempts: Some(5),
        }
        .into();
        assert_eq!(spec.read_timeout, DEFAULT_READ_TIMEOUT);
        assert_eq!(spec.connect_timeout, DEFAULT_CONNECT_TIMEOUT);
        assert_eq!(spec.max_attempts, 5);
    }
}
