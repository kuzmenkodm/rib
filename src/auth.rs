mod dockerconfig;
mod explicit;

use anyhow::Result;
pub use dockerconfig::DockerConfig;
use secrecy::SecretString;
use std::path::Path;

#[derive(Clone)]
pub enum Credential {
    Basic {
        username: String,
        password: SecretString,
        #[allow(unused)]
        source: CredentialSource,
    },
    IdentityToken {
        identity_token: SecretString,
        #[allow(unused)]
        source: CredentialSource,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum CredentialSource {
    Explicit,
    DockerAuths,
}

impl std::fmt::Display for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Explicit => "explicit (--credential)",
            Self::DockerAuths => "docker config: auths",
        })
    }
}

pub trait CredentialStore {
    fn get_credential(&self, registry: &str) -> Option<Credential>;
}

#[derive(Clone, Default)]
pub struct Credentials {
    explicit: explicit::ExplicitCreds,
    docker_config: Option<DockerConfig>,
}

impl Credentials {
    /// Load explicit flags + docker config. A missing config file is not an error
    pub fn load(explicit_flags: &[String], docker_config_path: Option<&Path>) -> Result<Self> {
        let explicit = explicit::ExplicitCreds::from_args(explicit_flags)?;
        let docker_config = dockerconfig::DockerConfig::from_path(docker_config_path)?;
        Ok(Self {
            explicit,
            docker_config,
        })
    }

    /// Resolve credentials for the given registry hostname. None = anonymous
    pub fn resolve(&self, registry: &str) -> Option<Credential> {
        let key = normalize_registry(registry);

        if let Some(c) = self.explicit.get_credential(&key) {
            return Some(c);
        }

        if let Some(cfg) = self.docker_config.as_ref()
            && let Some(c) = cfg.get_credential(&key)
        {
            return Some(c);
        }

        None
    }
}

/// Strip scheme and path; map docker.io aliases to the canonical v2 host
pub(crate) fn normalize_registry(input: &str) -> String {
    let trimmed = input
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let host = trimmed.split('/').next().unwrap_or(trimmed);
    match host {
        "docker.io" | "index.docker.io" | "registry-1.docker.io" => {
            "registry-1.docker.io".to_string()
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_docker_aliases() {
        for s in [
            "docker.io",
            "index.docker.io",
            "registry-1.docker.io",
            "https://index.docker.io/v1/",
            "https://docker.io/",
        ] {
            assert_eq!(
                normalize_registry(s),
                "registry-1.docker.io",
                "for input {s}"
            );
        }
    }

    #[test]
    fn normalize_other_hosts_keeps_host() {
        assert_eq!(normalize_registry("ghcr.io"), "ghcr.io");
        assert_eq!(normalize_registry("https://ghcr.io/"), "ghcr.io");
        assert_eq!(normalize_registry("gcr.io/foo/bar"), "gcr.io");
        assert_eq!(
            normalize_registry("123.dkr.ecr.us-east-1.amazonaws.com"),
            "123.dkr.ecr.us-east-1.amazonaws.com"
        );
    }

    #[test]
    fn missing_credentials_returns_none() {
        let store = Credentials::default();
        assert!(store.resolve("ghcr.io").is_none());
    }

    #[test]
    fn explicit_overrides_docker_config() {
        let cfg = serde_json::from_str::<DockerConfig>(
            r#"{ "auths": { "ghcr.io": { "auth": "YWxpY2U6czNjcmV0" } } }"#,
        )
        .unwrap();
        let store = Credentials {
            explicit: explicit::ExplicitCreds::from_args(&["ghcr.io=bob:override".to_string()])
                .unwrap(),
            docker_config: Some(cfg),
        };
        let c = store.resolve("ghcr.io").unwrap();
        match c {
            Credential::Basic {
                username,
                password: _,
                source,
            } => {
                assert_eq!(username, "bob");
                assert!(matches!(source, CredentialSource::Explicit));
            }
            _ => panic!("unexpected credential variant"),
        }
    }
}
