use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use oci_client::Reference;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OutputTarget {
    OciArchive { path: PathBuf, tag: Option<String> },
    DockerArchive { path: PathBuf, tag: Option<String> },
    Registry { reference: Reference },
}

impl OutputTarget {
    pub fn is_archive(&self) -> bool {
        matches!(self, Self::OciArchive { .. } | Self::DockerArchive { .. })
    }
}

impl FromStr for OutputTarget {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (kind, location) = value.split_once(':').ok_or_else(|| {
            anyhow::anyhow!("invalid output target {value:?}: expected <kind>:<location>[@<tag>]")
        })?;
        if location.is_empty() {
            bail!("output target has an empty location: {value:?}");
        }

        match kind {
            "registry" => Ok(Self::Registry {
                reference: location
                    .parse()
                    .with_context(|| format!("parsing registry output reference {location:?}"))?,
            }),
            "oci-archive" | "docker-archive" => {
                let (path, tag) = match location.rsplit_once('@') {
                    Some((path, tag)) if !tag.is_empty() => {
                        (path.to_string(), Some(tag.to_string()))
                    }
                    _ => (location.to_string(), None),
                };
                if path.is_empty() {
                    bail!("output target has an empty archive path: {value:?}");
                }
                let path = PathBuf::from(path);
                if kind == "oci-archive" {
                    Ok(Self::OciArchive { path, tag })
                } else {
                    Ok(Self::DockerArchive { path, tag })
                }
            }
            other => bail!(
                "unknown output target kind {other:?}; expected oci-archive, docker-archive, or registry"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_archive_targets_and_tags() {
        assert_eq!(
            "oci-archive:out.tar@v1".parse::<OutputTarget>().unwrap(),
            OutputTarget::OciArchive {
                path: PathBuf::from("out.tar"),
                tag: Some("v1".to_string()),
            }
        );
        assert_eq!(
            "docker-archive:dir@name/image.tar@latest"
                .parse::<OutputTarget>()
                .unwrap(),
            OutputTarget::DockerArchive {
                path: PathBuf::from("dir@name/image.tar"),
                tag: Some("latest".to_string()),
            }
        );
    }

    #[test]
    fn parses_registry_reference() {
        assert_eq!(
            "registry:ghcr.io/acme/demo:1.0.0"
                .parse::<OutputTarget>()
                .unwrap(),
            OutputTarget::Registry {
                reference: "ghcr.io/acme/demo:1.0.0".parse().unwrap(),
            }
        );
    }

    #[test]
    fn rejects_invalid_targets_without_cli_specific_errors() {
        let error = "./bare.tar"
            .parse::<OutputTarget>()
            .unwrap_err()
            .to_string();
        assert!(error.contains("output target"));
        assert!(!error.contains("--to"));
        assert!("oci-archive:".parse::<OutputTarget>().is_err());
        assert!("oci-archive:@v1".parse::<OutputTarget>().is_err());
        assert!("unknown:path".parse::<OutputTarget>().is_err());
    }
}
