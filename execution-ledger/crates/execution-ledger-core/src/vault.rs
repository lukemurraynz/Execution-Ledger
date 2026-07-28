use std::collections::HashMap;

use crate::error::{ExecutionLedgerError, Result};

/// Small vault adapter used by runtime bootstrap and tests.
pub trait VaultStore {
    fn get(&self, secret_name: &str) -> Result<Option<String>>;
}

/// Environment-backed vault for local/dev.
///
/// Reads JSON from `EXECUTION_LEDGER_VAULT_SECRETS_JSON`, e.g.
/// `{ "db-url": "...", "api-secret": "..." }`.
pub struct EnvVault {
    secrets: HashMap<String, String>,
}

impl EnvVault {
    pub fn from_env() -> Result<Self> {
        let legacy_raw = std::env::var("EXECUTION_LEDGER_VAULT_SECRETS_JSON").unwrap_or_default();
        let key_vault_raw =
            std::env::var("EXECUTION_LEDGER_KEY_VAULT_SECRETS_JSON").unwrap_or_default();

        let mut secrets = HashMap::new();
        if !legacy_raw.trim().is_empty() {
            let map =
                serde_json::from_str::<HashMap<String, String>>(&legacy_raw).map_err(|source| {
                    ExecutionLedgerError::DependencyFailed {
                        dependency: "serde_json",
                        source: Box::new(source),
                    }
                })?;
            secrets.extend(map);
        }
        if !key_vault_raw.trim().is_empty() {
            let map = serde_json::from_str::<HashMap<String, String>>(&key_vault_raw).map_err(
                |source| ExecutionLedgerError::DependencyFailed {
                    dependency: "serde_json",
                    source: Box::new(source),
                },
            )?;
            secrets.extend(map);
        }
        Ok(Self { secrets })
    }
}

impl VaultStore for EnvVault {
    fn get(&self, secret_name: &str) -> Result<Option<String>> {
        Ok(self.secrets.get(secret_name).cloned())
    }
}

/// Replaces `<secret:name>` markers in a key/value map using the provided vault.
#[allow(clippy::implicit_hasher)] // callers always use default hasher
pub fn resolve_secret_markers(
    values: &std::collections::HashMap<String, String>,
    vault: &dyn VaultStore,
) -> Result<std::collections::HashMap<String, String>> {
    let mut resolved = std::collections::HashMap::with_capacity(values.len());
    for (key, value) in values {
        if let Some(secret_name) = secret_marker_name(value) {
            let secret =
                vault
                    .get(secret_name)?
                    .ok_or(ExecutionLedgerError::InvalidConfiguration {
                        name: "secret marker resolution".to_string(),
                    })?;
            resolved.insert(key.clone(), secret);
        } else {
            resolved.insert(key.clone(), value.clone());
        }
    }
    Ok(resolved)
}

fn secret_marker_name(value: &str) -> Option<&str> {
    value
        .strip_prefix("<secret:")
        .and_then(|v| v.strip_suffix('>'))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestVault {
        secrets: HashMap<String, String>,
    }

    impl VaultStore for TestVault {
        fn get(&self, secret_name: &str) -> Result<Option<String>> {
            Ok(self.secrets.get(secret_name).cloned())
        }
    }

    #[test]
    fn resolves_secret_markers() -> Result<()> {
        let values = HashMap::from([
            ("A".to_string(), "<secret:alpha>".to_string()),
            ("B".to_string(), "plain".to_string()),
        ]);
        let vault = TestVault {
            secrets: HashMap::from([("alpha".to_string(), "resolved".to_string())]),
        };

        let resolved = resolve_secret_markers(&values, &vault)?;

        assert_eq!(resolved.get("A"), Some(&"resolved".to_string()));
        assert_eq!(resolved.get("B"), Some(&"plain".to_string()));
        Ok(())
    }
}
