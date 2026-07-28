//! Drasi configuration — loaded from YAML files at startup, with defaults
//! matching the current production configuration. Operators can override
//! any value by setting `EXECUTION_LEDGER_DRASI_CONFIG_PATH`.

use serde::Deserialize;

// ── Defaults ──────────────────────────────────────────────────────────────

const DEFAULT_CONFIG_PATH: &str = "config/drasi.yaml";
const CONFIG_PATH_ENV: &str = "EXECUTION_LEDGER_DRASI_CONFIG_PATH";

const DEFAULT_CYPHER_QUERY: &str = "MATCH (r:EXECUTION_LEDGER_jobs_runs) \
     WHERE r.status = 'completed' OR r.status = 'failed' \
     RETURN r.id, r.job_id, r.status, r.completed_at";

// ── YAML schema (Drasi-native) ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DrasiConfigFile {
    #[serde(default)]
    pub sources: Vec<SourceEntry>,
    #[serde(default)]
    pub queries: Vec<QueryEntry>,
    #[serde(default)]
    pub reactions: Vec<ReactionEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceEntry {
    pub id: String,
    #[serde(default)]
    pub tables: Vec<String>,
    pub slot_name: String,
    pub publication_name: String,
    #[serde(default = "default_ssl_mode")]
    pub ssl_mode: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryEntry {
    pub id: String,
    pub source: String,
    #[serde(default)]
    pub cypher_file: Option<String>,
    #[serde(default)]
    pub cypher: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReactionEntry {
    pub id: String,
    pub query: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
}

fn default_ssl_mode() -> String {
    "require".to_string()
}

fn default_timeout_ms() -> u64 {
    5000
}

fn resolve_env(value: &str) -> String {
    if value.starts_with("${") && value.ends_with('}') {
        let var_name = &value[2..value.len() - 1];
        std::env::var(var_name).unwrap_or_else(|_| value.to_string())
    } else {
        value.to_string()
    }
}

// ── Resolved config ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DrasiConfig {
    pub source_id: String,
    pub source_tables: Vec<String>,
    pub source_slot_name: String,
    pub source_publication_name: String,
    pub source_ssl_mode: String,
    pub queries: Vec<ResolvedQuery>,
    pub reactions: Vec<ResolvedReaction>,
}

#[derive(Debug, Clone)]
pub struct ResolvedQuery {
    pub id: String,
    pub source: String,
    pub cypher_query: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedReaction {
    pub id: String,
    pub query: String,
    pub timeout_ms: u64,
    pub base_url: Option<String>,
    pub token: Option<String>,
}

impl DrasiConfig {
    /// Load config from YAML file. Falls back to embedded defaults if the
    /// file-path env var is not set or the file cannot be read.
    ///
    /// # Errors
    ///
    /// Returns an error only if the file is found but contains invalid YAML.
    /// Missing-file cases fall back to defaults silently.
    pub fn load_or_default() -> anyhow::Result<Self> {
        let config_path = std::env::var(CONFIG_PATH_ENV)
            .ok()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string());

        match std::fs::read_to_string(&config_path) {
            Ok(raw) => Self::from_yaml(&raw, &config_path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(
                    path = %config_path,
                    "drasi config file not found; using embedded defaults"
                );
                Ok(Self::defaults())
            }
            Err(e) => {
                tracing::warn!(
                    path = %config_path,
                    error = %e,
                    "failed to read drasi config file; using embedded defaults"
                );
                Ok(Self::defaults())
            }
        }
    }

    fn from_yaml(raw: &str, config_path: &str) -> anyhow::Result<Self> {
        let file: DrasiConfigFile = serde_yaml::from_str(raw)?;

        if file.sources.is_empty() && file.queries.is_empty() && file.reactions.is_empty() {
            tracing::info!("drasi config is empty; using embedded defaults");
            return Ok(Self::defaults());
        }

        let source = file.sources.first();
        let config_dir = std::path::Path::new(config_path)
            .parent()
            .unwrap_or(std::path::Path::new("."));

        let queries: Vec<ResolvedQuery> = file
            .queries
            .iter()
            .map(|q| {
                let cypher_query = if let Some(ref inline) = q.cypher {
                    inline.trim().to_string()
                } else if let Some(ref cypher_file) = q.cypher_file {
                    let cypher_path = config_dir.join(cypher_file.as_str());
                    match std::fs::read_to_string(&cypher_path) {
                        Ok(content) => content.trim().to_string(),
                        Err(e) => {
                            tracing::warn!(
                                path = %cypher_path.display(),
                                error = %e,
                                "failed to read cypher query file; using embedded default"
                            );
                            DEFAULT_CYPHER_QUERY.to_string()
                        }
                    }
                } else {
                    tracing::warn!(
                        "query '{}' has neither cypher nor cypher_file; using embedded default",
                        q.id
                    );
                    DEFAULT_CYPHER_QUERY.to_string()
                };
                ResolvedQuery {
                    id: q.id.clone(),
                    source: q.source.clone(),
                    cypher_query,
                }
            })
            .collect();

        let reactions: Vec<ResolvedReaction> = file
            .reactions
            .iter()
            .map(|r| ResolvedReaction {
                id: r.id.clone(),
                query: r.query.clone(),
                timeout_ms: r.timeout_ms,
                base_url: r.base_url.as_deref().map(resolve_env),
                token: r.token.as_deref().map(resolve_env),
            })
            .collect();

        Ok(Self {
            source_id: source
                .map_or_else(|| "execution-ledger-job-runs".to_string(), |s| s.id.clone()),
            source_tables: source.map_or_else(
                || vec!["EXECUTION_LEDGER_jobs_runs".to_string()],
                |s| {
                    if s.tables.is_empty() {
                        vec!["EXECUTION_LEDGER_jobs_runs".to_string()]
                    } else {
                        s.tables.clone()
                    }
                },
            ),
            source_slot_name: source.map_or_else(
                || "execution_ledger_worker_slot".to_string(),
                |s| s.slot_name.clone(),
            ),
            source_publication_name: source.map_or_else(
                || "execution_ledger_worker_publication".to_string(),
                |s| s.publication_name.clone(),
            ),
            source_ssl_mode: source.map_or_else(|| "require".to_string(), |s| s.ssl_mode.clone()),
            queries,
            reactions,
        })
    }

    fn defaults() -> Self {
        Self {
            source_id: "execution-ledger-job-runs".to_string(),
            source_tables: vec!["EXECUTION_LEDGER_jobs_runs".to_string()],
            source_slot_name: "execution_ledger_worker_slot".to_string(),
            source_publication_name: "execution_ledger_worker_publication".to_string(),
            source_ssl_mode: "require".to_string(),
            queries: vec![ResolvedQuery {
                id: "completed-job-runs".to_string(),
                source: "execution-ledger-job-runs".to_string(),
                cypher_query: DEFAULT_CYPHER_QUERY.to_string(),
            }],
            reactions: vec![ResolvedReaction {
                id: "job-run-webhook".to_string(),
                query: "completed-job-runs".to_string(),
                timeout_ms: 5000,
                base_url: None,
                token: None,
            }],
        }
    }

    pub fn ssl_mode(&self) -> drasi_source_postgres::SslMode {
        match self.source_ssl_mode.as_str() {
            "disable" => drasi_source_postgres::SslMode::Disable,
            "prefer" => drasi_source_postgres::SslMode::Prefer,
            _ => drasi_source_postgres::SslMode::Require,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── defaults() ─────────────────────────────────────────────────────────

    #[test]
    fn defaults_sets_expected_source_id() {
        let config = DrasiConfig::defaults();
        assert_eq!(config.source_id, "execution-ledger-job-runs");
    }

    #[test]
    fn defaults_sets_expected_tables() {
        let config = DrasiConfig::defaults();
        assert_eq!(config.source_tables, vec!["EXECUTION_LEDGER_jobs_runs"]);
    }

    #[test]
    fn defaults_sets_expected_slot_name() {
        let config = DrasiConfig::defaults();
        assert_eq!(config.source_slot_name, "execution_ledger_worker_slot");
    }

    #[test]
    fn defaults_sets_expected_publication_name() {
        let config = DrasiConfig::defaults();
        assert_eq!(
            config.source_publication_name,
            "execution_ledger_worker_publication"
        );
    }

    #[test]
    fn defaults_sets_require_ssl_mode() {
        let config = DrasiConfig::defaults();
        assert_eq!(config.source_ssl_mode, "require");
    }

    #[test]
    fn defaults_produces_one_query() {
        let config = DrasiConfig::defaults();
        assert_eq!(config.queries.len(), 1);
        assert_eq!(config.queries[0].id, "completed-job-runs");
    }

    #[test]
    fn defaults_produces_one_reaction() {
        let config = DrasiConfig::defaults();
        assert_eq!(config.reactions.len(), 1);
        assert_eq!(config.reactions[0].id, "job-run-webhook");
    }

    // ── ssl_mode() ─────────────────────────────────────────────────────────

    #[test]
    fn ssl_mode_disable_maps_correctly() {
        let mut config = DrasiConfig::defaults();
        config.source_ssl_mode = "disable".to_string();
        assert!(matches!(
            config.ssl_mode(),
            drasi_source_postgres::SslMode::Disable
        ));
    }

    #[test]
    fn ssl_mode_prefer_maps_correctly() {
        let mut config = DrasiConfig::defaults();
        config.source_ssl_mode = "prefer".to_string();
        assert!(matches!(
            config.ssl_mode(),
            drasi_source_postgres::SslMode::Prefer
        ));
    }

    #[test]
    fn ssl_mode_require_maps_correctly() {
        let config = DrasiConfig::defaults();
        assert!(matches!(
            config.ssl_mode(),
            drasi_source_postgres::SslMode::Require
        ));
    }

    #[test]
    fn ssl_mode_unknown_falls_back_to_require() {
        let mut config = DrasiConfig::defaults();
        config.source_ssl_mode = "verify-full".to_string();
        // Unknown values default to Require — fail-safe behavior
        assert!(matches!(
            config.ssl_mode(),
            drasi_source_postgres::SslMode::Require
        ));
    }

    // ── from_yaml() — valid configs ────────────────────────────────────────

    #[test]
    fn from_yaml_parses_complete_config() {
        let yaml = r#"
sources:
  - id: my-source
    tables:
      - EXECUTION_LEDGER_jobs_runs
      - EXECUTION_LEDGER_steps_runs
    slot_name: custom_slot
    publication_name: custom_pub
    ssl_mode: disable
queries:
  - id: my-query
    source: my-source
    cypher: "MATCH (r) RETURN r.id"
reactions:
  - id: my-reaction
    query: my-query
    base_url: https://hooks.example.com/events
    token: "${EVENT_WEBHOOK_TOKEN}"
"#;
        let config = DrasiConfig::from_yaml(yaml, "test/config.yaml").unwrap();

        assert_eq!(config.source_id, "my-source");
        assert_eq!(
            config.source_tables,
            vec!["EXECUTION_LEDGER_jobs_runs", "EXECUTION_LEDGER_steps_runs"]
        );
        assert_eq!(config.source_slot_name, "custom_slot");
        assert_eq!(config.source_publication_name, "custom_pub");
        assert_eq!(config.source_ssl_mode, "disable");
        assert_eq!(config.queries.len(), 1);
        assert_eq!(config.queries[0].id, "my-query");
        assert_eq!(config.queries[0].cypher_query, "MATCH (r) RETURN r.id");
        assert_eq!(config.reactions.len(), 1);
        assert_eq!(config.reactions[0].id, "my-reaction");
        assert_eq!(
            config.reactions[0].base_url,
            Some("https://hooks.example.com/events".to_string())
        );
    }

    #[test]
    fn from_yaml_parses_minimal_config() {
        // Only required fields — `tables`, `ssl_mode` use defaults
        let yaml = r#"
sources:
  - id: bare-source
    slot_name: bare_slot
    publication_name: bare_pub
"#;
        let config = DrasiConfig::from_yaml(yaml, "test/config.yaml").unwrap();

        assert_eq!(config.source_id, "bare-source");
        assert_eq!(config.source_tables, vec!["EXECUTION_LEDGER_jobs_runs"]);
        assert_eq!(config.source_ssl_mode, "require");
    }

    #[test]
    fn from_yaml_empty_file_falls_back_to_defaults() {
        let yaml = "";
        // Empty YAML file → serde returns None/empty vecs → falls back to defaults
        let config = DrasiConfig::from_yaml(yaml, "test/config.yaml").unwrap();

        assert_eq!(config.source_id, "execution-ledger-job-runs");
        assert_eq!(config.source_tables, vec!["EXECUTION_LEDGER_jobs_runs"]);
        assert_eq!(config.source_slot_name, "execution_ledger_worker_slot");
    }

    // ── from_yaml() — invalid configs ──────────────────────────────────────

    #[test]
    fn from_yaml_rejects_invalid_yaml() {
        let yaml = "sources: [unclosed";
        let result = DrasiConfig::from_yaml(yaml, "test/config.yaml");
        assert!(result.is_err());
    }

    #[test]
    fn from_yaml_rejects_unknown_fields() {
        // `deny_unknown_fields` on DrasiConfigFile — typos should fail
        let yaml = r#"
sources:
  - id: src
    slot_name: slot
    publication_name: pub
    not_a_real_field: "typo"
"#;
        let result = DrasiConfig::from_yaml(yaml, "test/config.yaml");
        assert!(result.is_err());
    }

    // ── resolve_env() ──────────────────────────────────────────────────────

    #[test]
    fn resolve_env_passes_through_non_variable_strings() {
        let result = resolve_env("plain-string");
        assert_eq!(result, "plain-string");
    }

    #[test]
    fn resolve_env_returns_original_on_missing_variable() {
        let result = resolve_env("${NONEXISTENT_VAR_FOR_TEST}");
        // Missing env var → returns original `${...}` string (fail-safe: doesn't panic)
        assert_eq!(result, "${NONEXISTENT_VAR_FOR_TEST}");
    }

    #[test]
    fn resolve_env_handles_partial_variable_syntax() {
        // Not ${...} pattern → treated as literal
        let result = resolve_env("$NOT_A_VAR");
        assert_eq!(result, "$NOT_A_VAR");
    }

    // ── default_ssl_mode() ─────────────────────────────────────────────────

    #[test]
    fn default_ssl_mode_is_require() {
        assert_eq!(default_ssl_mode(), "require");
    }

    // ── default_timeout_ms() ───────────────────────────────────────────────

    #[test]
    fn default_timeout_ms_is_5000() {
        assert_eq!(default_timeout_ms(), 5000);
    }
}
