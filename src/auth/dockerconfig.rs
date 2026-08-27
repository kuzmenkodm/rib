use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
#[cfg(test)]
use secrecy::ExposeSecret;
use secrecy::SecretString;
use serde::{Deserialize, Deserializer};
use tracing::debug;

use super::{Credential, CredentialSource, CredentialStore, normalize_registry};

#[derive(Clone, Deserialize, Default)]
pub struct DockerConfig {
    #[serde(rename = "auths", default, deserialize_with = "deserialize_auths")]
    auth_data: HashMap<String, AuthEntry>,
}

fn deserialize_auths<'a, D>(deserializer: D) -> Result<HashMap<String, AuthEntry>, D::Error>
where
    D: Deserializer<'a>,
{
    let raw = HashMap::<String, AuthEntry>::deserialize(deserializer)?;

    let normalized = raw
        .into_iter()
        .map(|(k, v)| (normalize_registry(&k), v))
        .collect();

    Ok(normalized)
}

impl DockerConfig {
    pub fn from_path(explicit: Option<&Path>) -> Result<Option<Self>> {
        let path = explicit
            .map(Path::to_path_buf)
            .unwrap_or_else(Self::dockerconfig_default_path);
        if !path.exists() {
            if explicit.is_some() {
                bail!("explicit Docker config does not exist: {}", path.display());
            }
            debug!(path = %path.display(), "no docker config present");
            return Ok(None);
        }
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let dockerconfig: DockerConfig = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        debug!(path = %path.display(), "loaded docker config");
        Ok(Some(dockerconfig))
    }

    fn dockerconfig_default_path() -> PathBuf {
        if let Ok(dir) = std::env::var("DOCKER_CONFIG") {
            return PathBuf::from(dir).join("config.json");
        }
        if let Some(home) = dirs::home_dir() {
            return home.join(".docker").join("config.json");
        }
        PathBuf::from(".docker/config.json")
    }
}

impl CredentialStore for DockerConfig {
    fn get_credential(&self, key: &str) -> Option<Credential> {
        if let Some(entry) = self.auth_data.get(key)
            && let Some(credential) = entry.to_credential()
        {
            return Some(credential);
        }
        None
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct AuthEntry {
    #[serde(rename = "auth", default)]
    base64_auth: Option<String>,
    #[serde(rename = "identitytoken", default)]
    identity_token: Option<String>,
}

impl AuthEntry {
    fn to_credential(&self) -> Option<Credential> {
        if let Some(base64_string) = &self.base64_auth
            && let Some((username, password)) = Self::decode_basic_auth(base64_string)
        {
            return Some(Credential::Basic {
                username,
                password: SecretString::from(password),
                source: CredentialSource::DockerAuths,
            });
        }
        // Some entries hold only an identity token
        if let Some(token) = &self.identity_token {
            return Some(Credential::IdentityToken {
                identity_token: SecretString::from(token.clone()),
                source: CredentialSource::DockerAuths,
            });
        }
        None
    }

    fn decode_basic_auth(b64_string: &str) -> Option<(String, String)> {
        let decoded_bytes = B64.decode(b64_string.trim()).ok()?;
        let decoded_string = std::str::from_utf8(&decoded_bytes).ok()?;
        let (username, password) = decoded_string.split_once(':')?;
        Some((username.to_string(), password.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn auths_base64_decoded() {
        let cfg: DockerConfig =
            serde_json::from_str(r#"{ "auths": { "ghcr.io": { "auth": "YWxpY2U6czNjcmV0" } } }"#)
                .unwrap();
        let entry = cfg.auth_data.get("ghcr.io").unwrap();
        let c = entry.to_credential().unwrap();
        match c {
            Credential::Basic {
                username,
                password,
                source,
            } => {
                assert_eq!(username, "alice");
                assert_eq!(password.expose_secret(), "s3cret");
                assert!(matches!(source, CredentialSource::DockerAuths))
            }

            _ => panic!("unexpected credential variant"),
        }
    }

    #[test]
    fn auths_identity_token_only() {
        let entry = AuthEntry {
            base64_auth: None,
            identity_token: Some("eyJhbGc...".into()),
        };
        let c = entry.to_credential().unwrap();

        match c {
            Credential::IdentityToken {
                identity_token,
                source,
            } => {
                assert_eq!(identity_token.expose_secret(), "eyJhbGc...");
                assert!(matches!(source, CredentialSource::DockerAuths))
            }

            _ => panic!("unexpected credential variant"),
        }
    }

    #[test]
    fn auths_docker_normolized() {
        let cfg: DockerConfig = serde_json::from_str(
            r#"{ "auths": { "https://index.docker.io/v1/": { "auth": "YWxpY2U6czNjcmV0" } } }"#,
        )
        .unwrap();
        let entry = cfg.auth_data.get("registry-1.docker.io").unwrap();
        let c = entry.to_credential().unwrap();
        match c {
            Credential::Basic {
                username,
                password,
                source,
            } => {
                assert_eq!(username, "alice");
                assert_eq!(password.expose_secret(), "s3cret");
                assert!(matches!(source, CredentialSource::DockerAuths))
            }

            _ => panic!("unexpected credential variant"),
        }
    }

    #[test]
    fn missing_explicit_config_is_an_error() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("missing-config.json");
        let error = DockerConfig::from_path(Some(&path))
            .err()
            .expect("an explicit missing path must fail");
        assert!(error.to_string().contains("explicit Docker config"));
        assert!(error.to_string().contains("missing-config.json"));
    }
}
