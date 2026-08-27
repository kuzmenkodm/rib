use super::{Credential, CredentialSource, CredentialStore, normalize_registry};
use anyhow::{Result, anyhow};
#[cfg(test)]
use secrecy::ExposeSecret;
use secrecy::SecretString;
use std::collections::HashMap;

#[derive(Clone)]
struct Entry {
    username: String,
    password: SecretString,
}

#[derive(Clone, Default)]
pub struct ExplicitCreds {
    creds_by_host: HashMap<String, Entry>,
}

impl ExplicitCreds {
    pub fn from_args(cli_args: &[String]) -> Result<Self> {
        let mut creds_by_host = HashMap::new();
        for arg in cli_args {
            let (reg, userpass) = arg.split_once('=').ok_or_else(|| {
                anyhow!(
                    "bad --credential, expected <registry>=<user>:<pass>. Error splitting by `=`"
                )
            })?;
            let (user, pass) = userpass.split_once(':').ok_or_else(|| {
                anyhow!(
                    "bad --credential, expected <registry>=<user>:<pass>. Error splitting by `:`"
                )
            })?;
            creds_by_host.insert(
                normalize_registry(reg),
                Entry {
                    username: user.to_string(),
                    password: SecretString::from(pass.to_string()),
                },
            );
        }
        Ok(ExplicitCreds { creds_by_host })
    }
}

impl CredentialStore for ExplicitCreds {
    fn get_credential(&self, host: &str) -> Option<Credential> {
        let entry = self.creds_by_host.get(host)?;
        Some(Credential::Basic {
            username: entry.username.clone(),
            password: entry.password.clone(),
            source: CredentialSource::Explicit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_form() {
        let store = ExplicitCreds::from_args(&["ghcr.io=alice:s3cret".to_string()]).unwrap();
        let c = store.get_credential("ghcr.io").unwrap();
        match c {
            Credential::Basic {
                username,
                password,
                source,
            } => {
                assert_eq!(username, "alice");
                assert_eq!(password.expose_secret(), "s3cret");
                assert!(matches!(source, CredentialSource::Explicit))
            }

            _ => panic!("unexpected credential variant"),
        }
    }
    #[test]
    fn password_can_contain_colons() {
        let store =
            ExplicitCreds::from_args(&["ghcr.io=alice:pass:with:colons".to_string()]).unwrap();
        let c = store.get_credential("ghcr.io").unwrap();
        match c {
            Credential::Basic {
                username: _,
                password,
                source,
            } => {
                assert_eq!(password.expose_secret(), "pass:with:colons");
                assert!(matches!(source, CredentialSource::Explicit))
            }

            _ => panic!("unexpected credential variant"),
        }
    }

    #[test]
    fn rejects_bad_format() {
        assert!(ExplicitCreds::from_args(&["no-equals-sign".into()]).is_err());
        assert!(ExplicitCreds::from_args(&["host=no-colon".into()]).is_err());
    }

    #[test]
    fn normalizes_dockerhub_aliases() {
        let store = ExplicitCreds::from_args(&["docker.io=alice:s3cret".to_string()]).unwrap();
        assert!(store.get_credential("registry-1.docker.io").is_some());
    }
}
