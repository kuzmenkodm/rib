use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use serde::Deserialize;
use tokio::process::Command;

use crate::app::build::{BuildRequest, CopyDirective, ImageBuildSpec, default_jobs, parse_argv};
use crate::auth::Credentials;
use crate::builder::copy::CopySpec;
use crate::cli::{CacheArgs, GlobalArgs, ImageConfigArgs, RegistryClientArgs};
use crate::output_target::OutputTarget;
use crate::progress::Progress;

#[derive(Parser)]
#[command(
    name = "cargo-rib",
    bin_name = "cargo rib",
    version,
    about = "Build a Rust package and package its explicit artifact as an OCI image"
)]
struct CargoRibCli {
    #[command(subcommand)]
    command: CargoRibCommand,
}

#[derive(Subcommand)]
enum CargoRibCommand {
    /// Build an image and write it to one or more outputs
    Build(Box<CargoRibArgs>),
}

#[derive(Args)]
struct CargoRibArgs {
    /// Path to the binary produced by cargo build. Required here or in package.metadata.rib
    #[arg(long)]
    artifact: Option<PathBuf>,

    /// OCI platform, for example linux/amd64. Required here or in package.metadata.rib
    #[arg(long)]
    platform: Option<String>,

    /// Base image reference or the exact value `scratch`
    #[arg(long)]
    from: Option<String>,

    /// Output target. Repeatable; appended to targets from package.metadata.rib
    #[arg(long = "to")]
    targets: Vec<OutputTarget>,

    /// Exact path of the artifact inside the image. Defaults to /app/<artifact-name>
    #[arg(long)]
    destination: Option<String>,

    /// Additional file or directory to copy. Repeatable and appended after configured copies
    #[arg(long = "copy")]
    copies: Vec<String>,

    #[command(flatten)]
    image: ImageConfigArgs,

    /// Image creation timestamp: epoch, now, or an RFC3339 value
    #[arg(long)]
    creation_time: Option<String>,

    /// Maximum number of COPY layers and blob transfers processed concurrently
    #[arg(long)]
    jobs: Option<usize>,

    #[command(flatten)]
    global: GlobalArgs,

    #[command(flatten)]
    client: RegistryClientArgs,

    #[command(flatten)]
    cache: CacheArgs,

    /// Arguments passed to cargo build. Must follow `--`
    #[arg(last = true, allow_hyphen_values = true)]
    cargo_args: Vec<String>,
}

/// An argv value in `package.metadata.rib`: either one shell-quoted line
/// (`entrypoint = "/bin/sh -c 'echo hi'"`) or an explicit array
/// (`entrypoint = ["/bin/sh", "-c", "echo hi"]`)
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ConfigArgv {
    Line(String),
    Argv(Vec<String>),
}

impl ConfigArgv {
    fn into_argv(self) -> Result<Vec<String>> {
        match self {
            Self::Line(line) => parse_argv(&line),
            Self::Argv(argv) => Ok(argv),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct CargoRibConfig {
    artifact: Option<PathBuf>,
    platform: Option<String>,
    from: Option<String>,
    to: Vec<String>,
    destination: Option<String>,
    copies: Vec<String>,
    entrypoint: Option<ConfigArgv>,
    cmd: Option<ConfigArgv>,
    labels: Vec<String>,
    ports: Vec<String>,
    workdir: Option<String>,
    user: Option<String>,
    keep_cmd: bool,
    creation_time: Option<String>,
    jobs: Option<usize>,
    cargo_args: Vec<String>,
    from_plain_http: bool,
    from_skip_tls: bool,
    to_plain_http: bool,
    to_skip_tls: bool,
    cache: bool,
    cache_path: Option<String>,
    read_timeout: Option<u64>,
    connect_timeout: Option<u64>,
    max_attempts: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
    #[serde(default)]
    workspace_default_members: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    id: String,
    name: String,
    manifest_path: PathBuf,
    metadata: serde_json::Value,
}

struct CargoBuildPlan {
    artifact: PathBuf,
    cargo_args: Vec<String>,
    manifest_path: PathBuf,
    package_name: String,
    image: BuildRequest,
}

pub(crate) async fn run() -> Result<()> {
    let cli = parse_args();
    let arguments = match cli.command {
        CargoRibCommand::Build(arguments) => *arguments,
    };
    let progress = Progress::new(arguments.global.progress);
    crate::init_tracing(&progress);
    let credentials = Credentials::load(
        &arguments.global.credential,
        arguments.global.docker_config.as_deref(),
    )?;
    let current_dir = std::env::current_dir().context("reading current directory")?;
    let cargo = cargo_executable();
    let metadata = read_metadata(&cargo, &current_dir).await?;
    let package = select_package(&metadata, &current_dir)?;
    let plan = resolve_plan(&arguments, package, &current_dir)?;

    run_cargo_build(&cargo, &plan, &progress).await?;
    validate_artifact(&plan.artifact)?;

    plan.image.run(credentials, progress).await
}

fn parse_args() -> CargoRibCli {
    let mut arguments: Vec<OsString> = std::env::args_os().collect();
    if arguments.get(1).is_some_and(|argument| argument == "rib") {
        arguments.remove(1);
    }
    CargoRibCli::parse_from(arguments)
}

fn cargo_executable() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
}

async fn read_metadata(cargo: &OsString, current_dir: &Path) -> Result<CargoMetadata> {
    let mut command = Command::new(cargo);
    command
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(current_dir)
        .kill_on_drop(true);
    let output = command.output().await.context("running cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed with {}:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("parsing cargo metadata output")
}

fn select_package<'a>(metadata: &'a CargoMetadata, current_dir: &Path) -> Result<&'a CargoPackage> {
    let current_dir = current_dir
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", current_dir.display()))?;
    let mut containing: Vec<&CargoPackage> = metadata
        .packages
        .iter()
        .filter(|package| {
            package
                .manifest_path
                .parent()
                .and_then(|path| path.canonicalize().ok())
                .is_some_and(|path| current_dir.starts_with(path))
        })
        .collect();
    containing.sort_by_key(|package| {
        package
            .manifest_path
            .parent()
            .map(Path::components)
            .map(Iterator::count)
            .unwrap_or(0)
    });
    if let Some(package) = containing.pop() {
        return Ok(package);
    }

    if metadata.workspace_default_members.len() == 1 {
        let id = &metadata.workspace_default_members[0];
        if let Some(package) = metadata.packages.iter().find(|package| &package.id == id) {
            return Ok(package);
        }
    }
    if metadata.packages.len() == 1 {
        return Ok(&metadata.packages[0]);
    }

    bail!("cannot select a package from this workspace; run cargo rib from the package directory")
}

fn resolve_plan(
    arguments: &CargoRibArgs,
    package: &CargoPackage,
    current_dir: &Path,
) -> Result<CargoBuildPlan> {
    let manifest_dir = package.manifest_path.parent().ok_or_else(|| {
        anyhow!(
            "manifest has no parent: {}",
            package.manifest_path.display()
        )
    })?;
    let mut config = parse_config(package)?;

    let artifact = match &arguments.artifact {
        Some(path) => resolve_path(path, current_dir),
        None => {
            let path = config.artifact.take().ok_or_else(|| {
                anyhow!("artifact is required; set --artifact or package.metadata.rib.artifact")
            })?;
            resolve_path(&path, manifest_dir)
        }
    };
    let platform = arguments
        .platform
        .clone()
        .or(config.platform.take())
        .ok_or_else(|| {
            anyhow!("platform is required; set --platform or package.metadata.rib.platform")
        })?;
    let from = arguments
        .from
        .clone()
        .or(config.from.take())
        .ok_or_else(|| {
            anyhow!("base image is required; set --from or package.metadata.rib.from")
        })?;
    let destination = match arguments.destination.clone().or(config.destination.take()) {
        Some(destination) => destination,
        None => default_destination(&artifact)?,
    };
    validate_artifact_destination(&destination)?;

    let mut targets = config
        .to
        .iter()
        .map(|target| parse_config_target(target, manifest_dir))
        .collect::<Result<Vec<_>>>()?;
    targets.extend(arguments.targets.iter().cloned());
    if targets.is_empty() {
        bail!("at least one output is required; set --to or package.metadata.rib.to");
    }

    let mut copies = parse_copies_at(&config.copies, manifest_dir)?;
    copies.extend(parse_copies_at(&arguments.copies, current_dir)?);
    copies.push(CopyDirective::new(
        format!("artifact {} → {destination}", artifact.display()),
        CopySpec::executable(&artifact, &destination)?,
    ));

    let entrypoint = match (&arguments.image.entrypoint, config.entrypoint.take()) {
        (Some(line), _) => Some(parse_argv(line)?),
        (None, Some(configured)) => Some(configured.into_argv()?),
        (None, None) => Some(vec![destination.clone()]),
    };
    let cmd = match (&arguments.image.cmd, config.cmd.take()) {
        (Some(line), _) => Some(parse_argv(line)?),
        (None, Some(configured)) => Some(configured.into_argv()?),
        (None, None) => None,
    };

    let mut labels = config.labels;
    labels.extend(arguments.image.labels.iter().cloned());
    let mut ports = config.ports;
    ports.extend(arguments.image.ports.iter().cloned());
    let mut cargo_args = config.cargo_args;
    cargo_args.extend(arguments.cargo_args.iter().cloned());

    // Explicit flags win over package.metadata.rib
    let transport = RegistryClientArgs {
        from_plain_http: arguments.client.from_plain_http || config.from_plain_http,
        from_skip_tls: arguments.client.from_skip_tls || config.from_skip_tls,
        to_plain_http: arguments.client.to_plain_http || config.to_plain_http,
        to_skip_tls: arguments.client.to_skip_tls || config.to_skip_tls,
        read_timeout: arguments.client.read_timeout.or(config.read_timeout),
        connect_timeout: arguments.client.connect_timeout.or(config.connect_timeout),
        max_attempts: arguments.client.max_attempts.or(config.max_attempts),
    };
    let cache = CacheArgs {
        cache: arguments.cache.cache || config.cache,
        cache_path: arguments.cache.cache_path.clone().or(config.cache_path),
    };

    let image_specification = ImageBuildSpec {
        from,
        targets,
        copies,
        jobs: arguments.jobs.or(config.jobs).unwrap_or_else(default_jobs),
        platform,
        entrypoint,
        cmd,
        labels,
        ports,
        workdir: arguments.image.workdir.clone().or(config.workdir),
        user: arguments.image.user.clone().or(config.user),
        keep_cmd: arguments.image.keep_cmd || config.keep_cmd,
        creation_time: arguments
            .creation_time
            .clone()
            .or(config.creation_time)
            .unwrap_or_else(|| "epoch".to_string()),
        transport: transport.into(),
        cache: cache.into(),
    };

    Ok(CargoBuildPlan {
        artifact,
        cargo_args,
        manifest_path: package.manifest_path.clone(),
        package_name: package.name.clone(),
        image: BuildRequest::try_from(image_specification)?,
    })
}

fn parse_config(package: &CargoPackage) -> Result<CargoRibConfig> {
    let value = package
        .metadata
        .get("rib")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    serde_json::from_value(value)
        .with_context(|| format!("parsing package.metadata.rib for {}", package.name))
}

fn resolve_path(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn default_destination(artifact: &Path) -> Result<String> {
    let name = artifact
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            anyhow!(
                "artifact path has no UTF-8 filename: {}",
                artifact.display()
            )
        })?;
    Ok(format!("/app/{name}"))
}

fn validate_artifact_destination(destination: &str) -> Result<()> {
    if !destination.starts_with('/') || destination.ends_with('/') {
        bail!("destination must be an absolute file path: {destination:?}");
    }
    Ok(())
}

fn parse_config_target(raw: &str, manifest_dir: &Path) -> Result<OutputTarget> {
    let target: OutputTarget = raw
        .parse()
        .with_context(|| format!("parsing package.metadata.rib.to entry {raw:?}"))?;
    Ok(match target {
        OutputTarget::OciArchive { path, tag } => OutputTarget::OciArchive {
            path: resolve_path(&path, manifest_dir),
            tag,
        },
        OutputTarget::DockerArchive { path, tag } => OutputTarget::DockerArchive {
            path: resolve_path(&path, manifest_dir),
            tag,
        },
        registry => registry,
    })
}

fn parse_copies_at(raw_copies: &[String], base: &Path) -> Result<Vec<CopyDirective>> {
    raw_copies
        .iter()
        .map(|raw| {
            let mut specification =
                CopySpec::parse(raw).with_context(|| format!("parsing COPY directive {raw:?}"))?;
            let source = Path::new(&specification.src);
            if source.is_relative() {
                specification.src = base.join(source).to_string_lossy().into_owned();
            }
            Ok(CopyDirective::new(raw.clone(), specification))
        })
        .collect()
}

async fn run_cargo_build(
    cargo: &OsString,
    plan: &CargoBuildPlan,
    progress: &Progress,
) -> Result<()> {
    let task = progress.spinner(format!("cargo build ({})", plan.package_name));
    let mut command = Command::new(cargo);
    command
        .arg("build")
        .arg("--manifest-path")
        .arg(&plan.manifest_path)
        .args(&plan.cargo_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let status = command.status().await.context("running cargo build")?;
    if !status.success() {
        bail!("cargo build failed with {status}");
    }
    task.done();
    Ok(())
}

fn validate_artifact(artifact: &Path) -> Result<()> {
    let metadata = std::fs::metadata(artifact)
        .with_context(|| format!("reading built artifact {}", artifact.display()))?;
    if !metadata.is_file() {
        bail!(
            "configured artifact is not a regular file: {}",
            artifact.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Read;

    use oci_spec::image::{
        ANNOTATION_BASE_IMAGE_DIGEST, ANNOTATION_BASE_IMAGE_NAME, ImageConfiguration, ImageIndex,
        ImageManifest,
    };

    use super::*;
    use crate::digest::blob_path;
    use crate::progress::ProgressMode;
    use tempfile::TempDir;

    fn archive_entry(path: &Path, name: &str) -> Vec<u8> {
        let mut archive = tar::Archive::new(File::open(path).unwrap());
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap().to_string_lossy() == name {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).unwrap();
                return bytes;
            }
        }
        panic!("archive contains no entry named {name}");
    }

    #[test]
    fn parses_cargo_arguments_after_separator() {
        let cli = CargoRibCli::try_parse_from([
            "cargo-rib",
            "build",
            "--artifact",
            "target/release/demo",
            "--platform",
            "linux/amd64",
            "--",
            "--release",
            "--locked",
            "--bin",
            "demo",
        ])
        .unwrap();
        let arguments = match cli.command {
            CargoRibCommand::Build(arguments) => *arguments,
        };
        assert_eq!(
            arguments.cargo_args,
            ["--release", "--locked", "--bin", "demo"]
        );
    }

    #[test]
    fn rejects_unknown_metadata_keys() {
        let package = CargoPackage {
            id: "demo 0.1.0".to_string(),
            name: "demo".to_string(),
            manifest_path: PathBuf::from("/tmp/demo/Cargo.toml"),
            metadata: serde_json::json!({"rib": {"artificat": "target/release/demo"}}),
        };
        let error = parse_config(&package).unwrap_err().to_string();
        assert!(error.contains("package.metadata.rib"));
    }

    #[test]
    fn configured_copy_sources_are_relative_to_manifest() {
        let directory = TempDir::new().unwrap();
        let copies = parse_copies_at(
            &[
                "config/default.toml:/app/config.toml".to_string(),
                "/opt/shared:/app/shared".to_string(),
            ],
            directory.path(),
        )
        .unwrap();
        assert_eq!(
            copies[0].specification().src,
            directory
                .path()
                .join("config/default.toml")
                .to_string_lossy()
        );
        assert_eq!(copies[1].specification().src, "/opt/shared");
    }

    #[test]
    fn selects_the_package_containing_the_current_directory() {
        let workspace = TempDir::new().unwrap();
        let first = workspace.path().join("first");
        let second = workspace.path().join("second");
        std::fs::create_dir_all(first.join("src")).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let metadata = CargoMetadata {
            packages: vec![
                CargoPackage {
                    id: "first".to_string(),
                    name: "first".to_string(),
                    manifest_path: first.join("Cargo.toml"),
                    metadata: serde_json::json!({}),
                },
                CargoPackage {
                    id: "second".to_string(),
                    name: "second".to_string(),
                    manifest_path: second.join("Cargo.toml"),
                    metadata: serde_json::json!({}),
                },
            ],
            workspace_default_members: Vec::new(),
        };
        let selected = select_package(&metadata, &first.join("src")).unwrap();
        assert_eq!(selected.name, "first");
    }

    #[test]
    fn cli_cmd_overrides_configured_cmd() {
        let project = TempDir::new().unwrap();
        let package = CargoPackage {
            id: "demo".to_string(),
            name: "demo".to_string(),
            manifest_path: project.path().join("Cargo.toml"),
            metadata: serde_json::json!({
                "rib": {
                    "artifact": "target/release/demo",
                    "platform": "linux/amd64",
                    "from": "scratch",
                    "to": ["oci-archive:dist/image.tar"],
                    "cmd": "config-cmd"
                }
            }),
        };
        let cli = CargoRibCli::try_parse_from(["cargo-rib", "build", "--cmd", "cli-cmd"]).unwrap();
        let arguments = match cli.command {
            CargoRibCommand::Build(arguments) => *arguments,
        };
        let plan = resolve_plan(&arguments, &package, project.path()).unwrap();
        assert_eq!(plan.image.cmd(), Some(&["cli-cmd".to_string()][..]));
    }

    #[test]
    fn configured_argv_accepts_arrays_and_quoted_lines() {
        let project = TempDir::new().unwrap();
        let package = CargoPackage {
            id: "demo".to_string(),
            name: "demo".to_string(),
            manifest_path: project.path().join("Cargo.toml"),
            metadata: serde_json::json!({
                "rib": {
                    "artifact": "target/release/demo",
                    "platform": "linux/amd64",
                    "from": "scratch",
                    "to": ["oci-archive:dist/image.tar"],
                    "entrypoint": ["/bin/sh", "-c", "echo hello world"],
                    "cmd": "--title 'two words'"
                }
            }),
        };
        let cli = CargoRibCli::try_parse_from(["cargo-rib", "build"]).unwrap();
        let arguments = match cli.command {
            CargoRibCommand::Build(arguments) => *arguments,
        };
        let plan = resolve_plan(&arguments, &package, project.path()).unwrap();

        assert_eq!(
            plan.image.entrypoint(),
            Some(
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "echo hello world".to_string()
                ][..]
            )
        );
        assert_eq!(
            plan.image.cmd(),
            Some(&["--title".to_string(), "two words".to_string()][..])
        );
    }

    #[test]
    fn validates_artifact_destination() {
        assert!(validate_artifact_destination("/usr/local/bin/demo").is_ok());
        assert!(validate_artifact_destination("usr/local/bin/demo").is_err());
        assert!(validate_artifact_destination("/usr/local/bin/").is_err());
    }

    #[test]
    fn configured_and_cli_copies_precede_the_artifact() {
        let project = TempDir::new().unwrap();
        let package = CargoPackage {
            id: "demo".to_string(),
            name: "demo".to_string(),
            manifest_path: project.path().join("Cargo.toml"),
            metadata: serde_json::json!({
                "rib": {
                    "artifact": "target/release/demo",
                    "platform": "linux/amd64",
                    "from": "scratch",
                    "to": ["oci-archive:dist/image.tar"],
                    "copies": ["config.toml:/app/config.toml"]
                }
            }),
        };
        let cli =
            CargoRibCli::try_parse_from(["cargo-rib", "build", "--copy", "assets:/app/assets/"])
                .unwrap();
        let arguments = match cli.command {
            CargoRibCommand::Build(arguments) => *arguments,
        };
        let plan = resolve_plan(&arguments, &package, project.path()).unwrap();
        let copies = plan.image.copies();

        assert_eq!(copies.len(), 3);
        assert_eq!(copies[0].description(), "config.toml:/app/config.toml");
        assert_eq!(copies[1].description(), "assets:/app/assets/");
        assert!(copies[2].description().starts_with("artifact "));
        assert_eq!(copies[2].specification().mode, Some(0o755));
        assert_eq!(copies[2].specification().dst, "/app/demo");
    }

    #[tokio::test]
    async fn cargo_plan_builds_the_explicit_configured_artifact() {
        let project = TempDir::new().unwrap();
        std::fs::create_dir(project.path().join("src")).unwrap();
        std::fs::write(
            project.path().join("Cargo.toml"),
            r#"
[package]
name = "cargo-rib-fixture"
version = "0.1.0"
edition = "2024"

[package.metadata.rib]
artifact = "target/debug/cargo-rib-fixture"
platform = "linux/amd64"
from = "scratch"
to = ["oci-archive:dist/image.tar"]
destination = "/usr/local/bin/fixture"
cargo-args = ["--offline"]
"#,
        )
        .unwrap();
        std::fs::write(
            project.path().join("src/main.rs"),
            "fn main() { println!(\"fixture\"); }\n",
        )
        .unwrap();

        let cargo = cargo_executable();
        let metadata = read_metadata(&cargo, project.path()).await.unwrap();
        let package = select_package(&metadata, project.path()).unwrap();
        let cli =
            CargoRibCli::try_parse_from(["cargo-rib", "build", "--progress", "quiet"]).unwrap();
        let arguments = match cli.command {
            CargoRibCommand::Build(arguments) => *arguments,
        };
        let plan = resolve_plan(&arguments, package, project.path()).unwrap();

        run_cargo_build(&cargo, &plan, &Progress::new(ProgressMode::Quiet))
            .await
            .unwrap();
        validate_artifact(&plan.artifact).unwrap();
        assert!(plan.artifact.ends_with("target/debug/cargo-rib-fixture"));

        std::fs::create_dir(project.path().join("dist")).unwrap();
        plan.image
            .run(Credentials::default(), Progress::new(ProgressMode::Quiet))
            .await
            .unwrap();
        let archive = project.path().join("dist/image.tar");
        assert!(archive.is_file());
        assert!(std::fs::metadata(&archive).unwrap().len() > 0);

        let index: ImageIndex =
            serde_json::from_slice(&archive_entry(&archive, "index.json")).unwrap();
        let manifest_descriptor = &index.manifests()[0];
        let manifest: ImageManifest = serde_json::from_slice(&archive_entry(
            &archive,
            &blob_path(manifest_descriptor.digest()),
        ))
        .unwrap();
        assert_eq!(manifest.layers().len(), 1);
        let annotations = manifest.annotations().as_ref().unwrap();
        assert!(!annotations.contains_key(ANNOTATION_BASE_IMAGE_NAME));
        assert!(!annotations.contains_key(ANNOTATION_BASE_IMAGE_DIGEST));

        let config: ImageConfiguration = serde_json::from_slice(&archive_entry(
            &archive,
            &blob_path(manifest.config().digest()),
        ))
        .unwrap();
        assert_eq!(config.rootfs().diff_ids().len(), 1);
        assert_eq!(config.history().as_ref().unwrap().len(), 1);
        assert_eq!(
            config.config().as_ref().unwrap().entrypoint().as_ref(),
            Some(&vec!["/usr/local/bin/fixture".to_string()])
        );
    }
}
