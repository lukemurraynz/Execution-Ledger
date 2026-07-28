//! Environment-variable configuration, mirroring `docs/environment-variables.md`.
//!
//! Kept intentionally small: one flat struct read once at process start.
//! Add fields here as each runtime crate needs them — do not pre-build a
//! generic config framework before a second real consumer exists.

use crate::error::{ExecutionLedgerError, Result};
use crate::vault::{EnvVault, resolve_secret_markers};
use azure_core::credentials::TokenCredential;
use azure_identity::ManagedIdentityCredential;
use reqwest::Url;
use std::collections::HashMap;

const APPCONFIG_SCOPE: &str = "https://azconfig.io/.default";
const KEYVAULT_SCOPE: &str = "https://vault.azure.net/.default";
/// [VERIFIED] Azure App Configuration data-plane key-value API version.
/// `EvidenceType` = Docs
/// `WhereToCheck` = <https://learn.microsoft.com/en-us/azure/azure-app-configuration/rest-api-versioning>
/// Date = 2026-07-27
const APPCONFIG_API_VERSION: &str = "1.0";
/// [VERIFIED] Azure Key Vault secret REST API — real published version.
/// `EvidenceType` = Docs
/// `WhereToCheck` = <https://learn.microsoft.com/en-us/rest/api/keyvault/secrets/get-secret?view=rest-keyvault-secrets-7.4>
/// Date = 2026-07-26
const KEYVAULT_API_VERSION: &str = "7.4";
const KEYVAULT_REF_CONTENT_TYPE: &str =
    "application/vnd.microsoft.appconfig.keyvaultref+json;charset=utf-8";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueEngine {
    Local,
    ServiceBus,
}

#[derive(Debug, Clone)]
pub struct ExecutionLedgerConfig {
    pub database_url: String,
    pub environment: String,
    /// Expected `aud` claim on inbound Entra ID access tokens (the API
    /// app's Application ID URI or client ID). Required by `execution-ledger-api`
    /// only — `None` there is a startup-time configuration error, not a
    /// missing-auth bypass; worker/scheduler don't authenticate callers so
    /// they leave it unset.
    pub entra_audience: Option<String>,
    /// Shared secret APIM injects as `x-execution-ledger-gateway-secret` on every
    /// request it forwards. Optional: unset in local/dev where there's no
    /// APIM in front. When set, `execution-ledger-api` rejects any request missing or
    /// mismatching it, so the pod can't be reached by skipping the gateway
    /// even if network policy ever allows a direct path to it. APIM itself
    /// still owns subscription keys, rate limiting, and routing — this is
    /// only "did this request come through APIM at all".
    pub apim_gateway_secret: Option<String>,
    pub mcp_resource_url: Option<String>,
    pub mcp_authorization_servers: Option<String>,
    pub event_grid_topic_endpoint: Option<String>,
    pub worker_webhook_url: Option<String>,
    pub queue_engine: QueueEngine,
    pub servicebus_namespace: Option<String>,
    pub servicebus_queue_name: Option<String>,
    /// Maximum concurrent intent processing tasks per worker replica.
    /// Controls how many external API calls can run in parallel within a
    /// single replica. Defaults to 5. Set lower if external APIs have
    /// rate limits; set higher if they're fast and you need more throughput
    /// per replica.
    pub worker_max_concurrent_intents: usize,
    /// Maximum `PostgreSQL` connections for this service's pool.
    /// Defaults to sqlx default (10) when unset.
    pub pg_max_connections: Option<u32>,
}

impl ExecutionLedgerConfig {
    /// Loads configuration from process environment variables.
    ///
    /// # Errors
    /// Returns [`ExecutionLedgerError::InvalidConfiguration`] when a required variable
    /// is missing.
    pub fn from_env() -> Result<Self> {
        Self::from_map(|key| std::env::var(key).ok())
    }

    /// Loads configuration from env and resolves `<secret:...>` markers using
    /// the environment-backed vault adapter.
    pub async fn from_env_with_vault() -> Result<Self> {
        let keys = [
            "EXECUTION_LEDGER_DATABASE_URL",
            "EXECUTION_LEDGER_ENV",
            "EXECUTION_LEDGER_ENTRA_AUDIENCE",
            "EXECUTION_LEDGER_APIM_GATEWAY_SECRET",
            "EXECUTION_LEDGER_MCP_RESOURCE_URL",
            "EXECUTION_LEDGER_MCP_AUTHORIZATION_SERVERS",
            "EXECUTION_LEDGER_WORKER_WEBHOOK_URL",
            "EXECUTION_LEDGER_QUEUE_ENGINE",
            "EXECUTION_LEDGER_SERVICEBUS_CONNECTION_STRING",
            "EXECUTION_LEDGER_SERVICEBUS_NAMESPACE",
            "EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME",
            "EXECUTION_LEDGER_WORKER_MAX_CONCURRENT_INTENTS",
            "EXECUTION_LEDGER_PG_MAX_CONNECTIONS",
        ];
        let mut values = load_app_config_map().await?;
        for key in keys {
            if let Ok(value) = std::env::var(key) {
                values.insert(key.to_string(), value);
            }
        }
        let environment = values
            .get("EXECUTION_LEDGER_ENV")
            .map_or("local", String::as_str);
        if is_production_env(environment) {
            reject_production_local_secret_sources()?;
        }
        let vault = EnvVault::from_env()?;
        let resolved = resolve_secret_markers(&values, &vault)?;
        Self::from_map(|key| resolved.get(key).cloned())
    }

    /// Loads configuration from an arbitrary lookup function.
    ///
    /// Exists so tests can supply a fake environment instead of mutating the
    /// real process environment (which is global and test-order-sensitive).
    pub fn from_map(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let database_url = lookup("EXECUTION_LEDGER_DATABASE_URL").ok_or(
            ExecutionLedgerError::InvalidConfiguration {
                name: "EXECUTION_LEDGER_DATABASE_URL".to_string(),
            },
        )?;
        let environment = lookup("EXECUTION_LEDGER_ENV").unwrap_or_else(|| "local".to_string());
        let entra_audience = lookup("EXECUTION_LEDGER_ENTRA_AUDIENCE");
        let apim_gateway_secret = lookup("EXECUTION_LEDGER_APIM_GATEWAY_SECRET");
        let mcp_resource_url = lookup("EXECUTION_LEDGER_MCP_RESOURCE_URL");
        let mcp_authorization_servers = lookup("EXECUTION_LEDGER_MCP_AUTHORIZATION_SERVERS");
        let worker_webhook_url = lookup("EXECUTION_LEDGER_WORKER_WEBHOOK_URL");
        let event_grid_topic_endpoint = lookup("EXECUTION_LEDGER_EVENT_GRID_TOPIC_ENDPOINT");
        let queue_engine = parse_queue_engine(lookup("EXECUTION_LEDGER_QUEUE_ENGINE").as_deref())?;
        let servicebus_namespace = lookup("EXECUTION_LEDGER_SERVICEBUS_NAMESPACE");
        let servicebus_queue_name = lookup("EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME");
        let worker_max_concurrent_intents =
            lookup("EXECUTION_LEDGER_WORKER_MAX_CONCURRENT_INTENTS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(5);
        let pg_max_connections =
            lookup("EXECUTION_LEDGER_PG_MAX_CONNECTIONS").and_then(|v| v.parse().ok());

        if environment != "local" && !matches!(queue_engine, QueueEngine::ServiceBus) {
            return Err(ExecutionLedgerError::InvalidConfiguration {
                name: "EXECUTION_LEDGER_QUEUE_ENGINE (must be servicebus outside local)"
                    .to_string(),
            });
        }

        if is_production_env(&environment) {
            if contains_localhost(&database_url) {
                return Err(ExecutionLedgerError::InvalidConfiguration {
                    name: "EXECUTION_LEDGER_DATABASE_URL (must not use localhost in production)"
                        .to_string(),
                });
            }
            if let Some(url) = worker_webhook_url.as_deref()
                && contains_localhost(url)
            {
                return Err(ExecutionLedgerError::InvalidConfiguration {
                    name:
                        "EXECUTION_LEDGER_WORKER_WEBHOOK_URL (must not use localhost in production)"
                            .to_string(),
                });
            }
        }

        if matches!(queue_engine, QueueEngine::ServiceBus) {
            if servicebus_queue_name.as_deref().is_none_or(str::is_empty) {
                return Err(ExecutionLedgerError::InvalidConfiguration {
                    name: "EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME".to_string(),
                });
            }

            if servicebus_namespace.as_deref().is_none_or(str::is_empty) {
                return Err(ExecutionLedgerError::InvalidConfiguration {
                    name: "EXECUTION_LEDGER_SERVICEBUS_NAMESPACE".to_string(),
                });
            }
            if lookup("EXECUTION_LEDGER_SERVICEBUS_CONNECTION_STRING")
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            {
                return Err(ExecutionLedgerError::InvalidConfiguration {
                    name: "EXECUTION_LEDGER_SERVICEBUS_CONNECTION_STRING (not allowed; managed identity only)"
                        .to_string(),
                });
            }
        }

        if environment == "local" {
            reject_local_secret_marker("EXECUTION_LEDGER_DATABASE_URL", &database_url)?;
            if let Some(value) = &entra_audience {
                reject_local_secret_marker("EXECUTION_LEDGER_ENTRA_AUDIENCE", value)?;
            }
            if let Some(value) = &apim_gateway_secret {
                reject_local_secret_marker("EXECUTION_LEDGER_APIM_GATEWAY_SECRET", value)?;
            }
            if let Some(value) = &mcp_resource_url {
                reject_local_secret_marker("EXECUTION_LEDGER_MCP_RESOURCE_URL", value)?;
            }
            if let Some(value) = &mcp_authorization_servers {
                reject_local_secret_marker("EXECUTION_LEDGER_MCP_AUTHORIZATION_SERVERS", value)?;
            }
            if let Some(value) = &worker_webhook_url {
                reject_local_secret_marker("EXECUTION_LEDGER_WORKER_WEBHOOK_URL", value)?;
            }
            if let Some(value) = &servicebus_queue_name {
                reject_local_secret_marker("EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME", value)?;
            }
            if let Some(value) = &servicebus_namespace {
                reject_local_secret_marker("EXECUTION_LEDGER_SERVICEBUS_NAMESPACE", value)?;
            }
        }

        Ok(Self {
            database_url,
            environment,
            entra_audience,
            apim_gateway_secret,
            mcp_resource_url,
            mcp_authorization_servers,
            event_grid_topic_endpoint,
            worker_webhook_url,
            queue_engine,
            servicebus_namespace,
            servicebus_queue_name,
            worker_max_concurrent_intents,
            pg_max_connections,
        })
    }
}

fn parse_queue_engine(raw: Option<&str>) -> Result<QueueEngine> {
    match raw {
        None | Some("" | "local") => Ok(QueueEngine::Local),
        Some("servicebus") => Ok(QueueEngine::ServiceBus),
        Some(_) => Err(ExecutionLedgerError::InvalidConfiguration {
            name: "EXECUTION_LEDGER_QUEUE_ENGINE".to_string(),
        }),
    }
}

async fn load_app_config_map() -> Result<HashMap<String, String>> {
    let environment = std::env::var("EXECUTION_LEDGER_ENV").unwrap_or_else(|_| "local".to_string());
    let endpoint = std::env::var("EXECUTION_LEDGER_APP_CONFIG_ENDPOINT").unwrap_or_default();
    if endpoint.trim().is_empty() {
        if environment == "local" {
            return load_local_app_config_json();
        }
        return Err(ExecutionLedgerError::InvalidConfiguration {
            name: "EXECUTION_LEDGER_APP_CONFIG_ENDPOINT".to_string(),
        });
    }
    let label = std::env::var("EXECUTION_LEDGER_APP_CONFIG_LABEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(environment);
    load_remote_app_config_map(endpoint.trim(), &label).await
}

fn load_local_app_config_json() -> Result<HashMap<String, String>> {
    let raw = std::env::var("EXECUTION_LEDGER_APP_CONFIG_JSON").unwrap_or_default();
    if raw.trim().is_empty() {
        return Ok(HashMap::new());
    }
    serde_json::from_str::<HashMap<String, String>>(&raw).map_err(|source| {
        ExecutionLedgerError::DependencyFailed {
            dependency: "serde_json",
            source: Box::new(source),
        }
    })
}

async fn load_remote_app_config_map(
    endpoint: &str,
    label: &str,
) -> Result<HashMap<String, String>> {
    #[derive(serde::Deserialize)]
    struct AppConfigList {
        items: Vec<AppConfigItem>,
    }

    #[derive(serde::Deserialize)]
    struct AppConfigItem {
        key: String,
        value: String,
        #[serde(default)]
        content_type: Option<String>,
    }

    let mut url = Url::parse(endpoint).map_err(|_| ExecutionLedgerError::InvalidConfiguration {
        name: "EXECUTION_LEDGER_APP_CONFIG_ENDPOINT".to_string(),
    })?;
    url.set_path("/kv");
    url.query_pairs_mut()
        .append_pair("key", "EXECUTION_LEDGER_*")
        .append_pair("label", label)
        .append_pair("api-version", APPCONFIG_API_VERSION);

    let credential = ManagedIdentityCredential::new(None).map_err(|source| {
        ExecutionLedgerError::DependencyFailed {
            dependency: "azure_identity",
            source: Box::new(source),
        }
    })?;
    let token = managed_identity_access_token(&credential, APPCONFIG_SCOPE).await?;
    let client = reqwest::Client::new();
    let response = client
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "reqwest",
            source: Box::new(source),
        })?;

    let status = response.status();
    if !status.is_success() {
        let body = match response.text().await {
            Ok(text) => text,
            Err(e) => {
                tracing::warn!(%e, %status, "could not read App Configuration error response body");
                format!("<unreadable: {e}>")
            }
        };
        return Err(ExecutionLedgerError::DependencyFailed {
            dependency: "app_configuration",
            source: Box::new(std::io::Error::other(format!(
                "list key-values failed with status {status}: {body}"
            ))),
        });
    }

    let payload = response.json::<AppConfigList>().await.map_err(|source| {
        ExecutionLedgerError::DependencyFailed {
            dependency: "reqwest",
            source: Box::new(source),
        }
    })?;
    let mut values = HashMap::with_capacity(payload.items.len());
    for item in payload.items {
        if keyvault_ref_content_type(item.content_type.as_deref()) {
            let secret_uri = parse_keyvault_ref_uri(&item.value)?;
            let secret = fetch_keyvault_secret(&client, &credential, &secret_uri).await?;
            values.insert(item.key, secret);
        } else {
            values.insert(item.key, item.value);
        }
    }
    Ok(values)
}

fn keyvault_ref_content_type(content_type: Option<&str>) -> bool {
    content_type
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .is_some_and(|value| value == KEYVAULT_REF_CONTENT_TYPE)
}

fn parse_keyvault_ref_uri(raw: &str) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct KeyVaultReference {
        uri: String,
    }

    let parsed = serde_json::from_str::<KeyVaultReference>(raw).map_err(|source| {
        ExecutionLedgerError::DependencyFailed {
            dependency: "serde_json",
            source: Box::new(source),
        }
    })?;
    if parsed.uri.trim().is_empty() {
        return Err(ExecutionLedgerError::InvalidConfiguration {
            name: "app_configuration:keyvault-reference-uri".to_string(),
        });
    }
    Ok(parsed.uri)
}

async fn fetch_keyvault_secret(
    client: &reqwest::Client,
    credential: &ManagedIdentityCredential,
    secret_uri: &str,
) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct KeyVaultSecretResponse {
        value: String,
    }

    let mut url =
        Url::parse(secret_uri).map_err(|_| ExecutionLedgerError::InvalidConfiguration {
            name: "app_configuration:keyvault-reference-uri".to_string(),
        })?;
    url.query_pairs_mut()
        .append_pair("api-version", KEYVAULT_API_VERSION);
    let token = managed_identity_access_token(credential, KEYVAULT_SCOPE).await?;
    let response = client
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "reqwest",
            source: Box::new(source),
        })?;

    let status = response.status();
    if !status.is_success() {
        let body = match response.text().await {
            Ok(text) => text,
            Err(e) => {
                tracing::warn!(%e, %status, "could not read Key Vault error response body");
                format!("<unreadable: {e}>")
            }
        };
        return Err(ExecutionLedgerError::DependencyFailed {
            dependency: "key_vault",
            source: Box::new(std::io::Error::other(format!(
                "resolve key vault secret failed with status {status}: {body}"
            ))),
        });
    }
    let payload = response
        .json::<KeyVaultSecretResponse>()
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "reqwest",
            source: Box::new(source),
        })?;
    if payload.value.trim().is_empty() {
        return Err(ExecutionLedgerError::InvalidConfiguration {
            name: "app_configuration:keyvault-reference-value".to_string(),
        });
    }
    Ok(payload.value)
}

async fn managed_identity_access_token(
    credential: &ManagedIdentityCredential,
    scope: &str,
) -> Result<String> {
    let token = credential
        .get_token(&[scope], None)
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "azure_identity",
            source: Box::new(source),
        })?;
    let access_token = token.token.secret().trim();
    if access_token.is_empty() {
        return Err(ExecutionLedgerError::DependencyFailed {
            dependency: "azure_identity",
            source: Box::new(std::io::Error::other(
                "managed identity token request returned an empty access token",
            )),
        });
    }
    Ok(access_token.to_string())
}

fn reject_local_secret_marker(name: &'static str, value: &str) -> Result<()> {
    if value.starts_with("<secret:") && value.ends_with('>') {
        return Err(ExecutionLedgerError::InvalidConfiguration {
            name: name.to_string(),
        });
    }
    Ok(())
}

fn is_production_env(environment: &str) -> bool {
    matches!(environment, "prod" | "prd" | "production")
}

fn contains_localhost(value: &str) -> bool {
    let lowered = value.to_ascii_lowercase();
    lowered.contains("localhost") || lowered.contains("127.0.0.1")
}

fn reject_production_local_secret_sources() -> Result<()> {
    let legacy = std::env::var("EXECUTION_LEDGER_VAULT_SECRETS_JSON").unwrap_or_default();
    if !legacy.trim().is_empty() {
        return Err(ExecutionLedgerError::InvalidConfiguration {
            name: "EXECUTION_LEDGER_VAULT_SECRETS_JSON (not allowed in production)".to_string(),
        });
    }

    let key_vault_map =
        std::env::var("EXECUTION_LEDGER_KEY_VAULT_SECRETS_JSON").unwrap_or_default();
    if !key_vault_map.trim().is_empty() {
        return Err(ExecutionLedgerError::InvalidConfiguration {
            name: "EXECUTION_LEDGER_KEY_VAULT_SECRETS_JSON (not allowed in production)".to_string(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn map_lookup(values: HashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> {
        move |key| values.get(key).map(ToString::to_string)
    }

    #[test]
    fn rejects_missing_database_url() {
        let result = ExecutionLedgerConfig::from_map(map_lookup(HashMap::new()));

        assert!(matches!(
            result,
            Err(ExecutionLedgerError::InvalidConfiguration { ref name })
                if name == "EXECUTION_LEDGER_DATABASE_URL"
        ));
    }

    #[test]
    fn defaults_environment_to_local_when_unset() -> Result<()> {
        let values = HashMap::from([(
            "EXECUTION_LEDGER_DATABASE_URL",
            "postgres://localhost/execution_ledger",
        )]);

        let config = ExecutionLedgerConfig::from_map(map_lookup(values))?;

        assert_eq!(config.environment, "local");
        Ok(())
    }

    #[test]
    fn rejects_secret_markers_in_local_environment() {
        let values = HashMap::from([("EXECUTION_LEDGER_DATABASE_URL", "<secret:db-url>")]);

        let result = ExecutionLedgerConfig::from_map(map_lookup(values));

        assert!(matches!(
            result,
            Err(ExecutionLedgerError::InvalidConfiguration { ref name })
                if name == "EXECUTION_LEDGER_DATABASE_URL"
        ));
    }

    #[test]
    fn rejects_servicebus_mode_without_connection_settings() {
        let values = HashMap::from([
            (
                "EXECUTION_LEDGER_DATABASE_URL",
                "postgres://localhost/execution_ledger",
            ),
            ("EXECUTION_LEDGER_QUEUE_ENGINE", "servicebus"),
            (
                "EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME",
                "execution-ledger-intents",
            ),
        ]);

        let result = ExecutionLedgerConfig::from_map(map_lookup(values));

        assert!(matches!(
            result,
            Err(ExecutionLedgerError::InvalidConfiguration { ref name })
                if name == "EXECUTION_LEDGER_SERVICEBUS_NAMESPACE"
        ));
    }

    #[test]
    fn rejects_local_queue_engine_outside_local_environment() {
        let values = HashMap::from([
            (
                "EXECUTION_LEDGER_DATABASE_URL",
                "postgres://localhost/execution_ledger",
            ),
            ("EXECUTION_LEDGER_ENV", "prd"),
            ("EXECUTION_LEDGER_QUEUE_ENGINE", "local"),
        ]);

        let result = ExecutionLedgerConfig::from_map(map_lookup(values));

        assert!(matches!(
            result,
            Err(ExecutionLedgerError::InvalidConfiguration { ref name })
                if name == "EXECUTION_LEDGER_QUEUE_ENGINE (must be servicebus outside local)"
        ));
    }

    #[test]
    fn rejects_servicebus_mode_without_namespace_outside_local() {
        let values = HashMap::from([
            (
                "EXECUTION_LEDGER_DATABASE_URL",
                "postgres://db.internal/execution_ledger",
            ),
            ("EXECUTION_LEDGER_ENV", "prd"),
            ("EXECUTION_LEDGER_QUEUE_ENGINE", "servicebus"),
            (
                "EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME",
                "execution-ledger-intents",
            ),
        ]);

        let result = ExecutionLedgerConfig::from_map(map_lookup(values));

        assert!(matches!(
            result,
            Err(ExecutionLedgerError::InvalidConfiguration { ref name })
                if name == "EXECUTION_LEDGER_SERVICEBUS_NAMESPACE"
        ));
    }

    #[test]
    fn rejects_servicebus_connection_string_when_managed_identity_only() {
        let values = HashMap::from([
            (
                "EXECUTION_LEDGER_DATABASE_URL",
                "postgres://db.internal/execution_ledger",
            ),
            ("EXECUTION_LEDGER_ENV", "prd"),
            ("EXECUTION_LEDGER_QUEUE_ENGINE", "servicebus"),
            (
                "EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME",
                "execution-ledger-intents",
            ),
            (
                "EXECUTION_LEDGER_SERVICEBUS_NAMESPACE",
                "test.servicebus.windows.net",
            ),
            (
                "EXECUTION_LEDGER_SERVICEBUS_CONNECTION_STRING",
                "Endpoint=sb://atlas.servicebus.windows.net/;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=x",
            ),
        ]);

        let result = ExecutionLedgerConfig::from_map(map_lookup(values));

        assert!(matches!(
            result,
            Err(ExecutionLedgerError::InvalidConfiguration { ref name })
                if name == "EXECUTION_LEDGER_SERVICEBUS_CONNECTION_STRING (not allowed; managed identity only)"
        ));
    }

    #[test]
    fn rejects_localhost_database_url_in_production() {
        let values = HashMap::from([
            (
                "EXECUTION_LEDGER_DATABASE_URL",
                "postgres://localhost/execution_ledger",
            ),
            ("EXECUTION_LEDGER_ENV", "production"),
            ("EXECUTION_LEDGER_QUEUE_ENGINE", "servicebus"),
            (
                "EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME",
                "execution-ledger-intents",
            ),
            (
                "EXECUTION_LEDGER_SERVICEBUS_NAMESPACE",
                "test.servicebus.windows.net",
            ),
        ]);

        let result = ExecutionLedgerConfig::from_map(map_lookup(values));

        assert!(matches!(
            result,
            Err(ExecutionLedgerError::InvalidConfiguration { ref name })
                if name == "EXECUTION_LEDGER_DATABASE_URL (must not use localhost in production)"
        ));
    }

    #[test]
    fn keyvault_ref_content_type_match_is_case_insensitive() {
        assert!(keyvault_ref_content_type(Some(
            "Application/Vnd.Microsoft.AppConfig.KeyVaultRef+Json;Charset=UTF-8"
        )));
    }
}
