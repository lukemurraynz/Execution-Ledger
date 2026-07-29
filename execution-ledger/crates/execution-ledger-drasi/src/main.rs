//! Drasi WAL reader: streams PostgreSQL change events and pushes
//! completed/failed job-run events to the configured delivery endpoint.
//!
//! Split from `execution-ledger-worker` so the WAL reader runs as a
//! singleton replica while intent processing scales independently.
//!
//! Delivery: prefers Event Grid custom topic (managed identity auth) over
//! raw webhook URL. Falls back to webhook if Event Grid is not configured.
//!
//! Configuration is loaded from `config/drasi.yaml` at startup. Override
//! the path with `EXECUTION_LEDGER_DRASI_CONFIG_PATH`. Falls back to
//! embedded defaults if no config file is found.

mod config;

use azure_core::credentials::TokenCredential;
use azure_identity::ManagedIdentityCredential;
use config::DrasiConfig;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_http::HttpReactionBuilder;
use drasi_source_postgres::PostgresReplicationSource;
use execution_ledger_core::{
    ExecutionLedgerConfig, db,
    observability::{log_counter, log_gauge},
};
use serde_json::json;
use url::Url;

const EVENTGRID_SCOPE: &str = "https://eventgrid.azure.net/.default";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    execution_ledger_core::observability::init_runtime_telemetry("execution-ledger-drasi");

    let runtime_config = ExecutionLedgerConfig::from_env_with_vault().await?;
    let drasi_config = DrasiConfig::load_or_default()?;

    // Prefer Event Grid (managed identity) over raw webhook URL
    let (delivery_url, delivery_auth_token, delivery_mode) =
        resolve_delivery_target(&runtime_config).await?;

    let pool = db::connect_with_max_connections(
        &runtime_config.database_url,
        runtime_config.pg_max_connections,
    )
    .await?;
    db::bootstrap_runtime_tables(&pool).await?;
    db::ensure_default_platform_tenant(&pool).await?;
    db::assert_logical_replication_enabled(&pool).await?;
    ensure_publication_exists(&pool, &drasi_config).await?;

    let pg_conn = PostgresConnectionParts::parse(&runtime_config.database_url)?;

    let source = PostgresReplicationSource::builder(&drasi_config.source_id)
        .with_host(pg_conn.host)
        .with_port(pg_conn.port)
        .with_database(pg_conn.database)
        .with_user(pg_conn.user)
        .with_password(pg_conn.password)
        .with_tables(drasi_config.source_tables.clone())
        .with_slot_name(drasi_config.source_slot_name.clone())
        .with_publication_name(drasi_config.source_publication_name.clone())
        .with_ssl_mode(drasi_config.ssl_mode())
        .build()?;

    let mut drasi_builder = DrasiLib::builder()
        .with_id("execution-ledger-drasi")
        .with_source(source);

    // Add all queries from config
    for query in &drasi_config.queries {
        let query_config = Query::cypher(&query.id)
            .query(&query.cypher_query)
            .from_source(&query.source)
            .build();
        drasi_builder = drasi_builder.with_query(query_config);

        tracing::info!(
            query_id = %query.id,
            source = %query.source,
            "drasi: registering continuous query"
        );
    }

    // Add all reactions from config
    for reaction_cfg in &drasi_config.reactions {
        let base_url = reaction_cfg.base_url.as_deref().unwrap_or(&delivery_url);

        // Skip reactions whose base_url is an unresolved env-var placeholder
        if base_url.starts_with("${") && base_url.ends_with('}') {
            tracing::warn!(
                reaction_id = %reaction_cfg.id,
                base_url,
                "drasi: skipping reaction with unresolved base_url env var"
            );
            continue;
        }

        let mut rxn = HttpReactionBuilder::new(&reaction_cfg.id)
            .with_query(&reaction_cfg.query)
            .with_base_url(base_url)
            .with_timeout_ms(reaction_cfg.timeout_ms);

        // Per-reaction token takes priority over global delivery token
        if let Some(ref token) = reaction_cfg.token {
            rxn = rxn.with_token(token.clone());
        } else if let Some(ref token) = delivery_auth_token {
            rxn = rxn.with_token(token.clone());
        }

        drasi_builder = drasi_builder.with_reaction(rxn.build()?);

        tracing::info!(
            reaction_id = %reaction_cfg.id,
            query = %reaction_cfg.query,
            base_url,
            has_token = reaction_cfg.token.is_some() || delivery_auth_token.is_some(),
            "drasi: registering reaction"
        );
    }

    let drasi = drasi_builder.build().await?;

    tracing::info!(
        delivery_url,
        delivery_mode,
        tables = ?drasi_config.source_tables,
        slot_name = %drasi_config.source_slot_name,
        num_queries = drasi_config.queries.len(),
        num_reactions = drasi_config.reactions.len(),
        "execution-ledger-drasi: running (WAL -> {delivery_mode})"
    );

    log_counter(
        "drasi.startup",
        1,
        &json!({
            "status": "started",
            "slot_name": drasi_config.source_slot_name,
            "delivery_mode": delivery_mode,
        }),
    );
    log_gauge(
        "drasi.active_instances",
        1.0,
        &json!({ "slot_name": drasi_config.source_slot_name }),
    );

    spawn_heartbeat_writer(pool.clone());

    tokio::signal::ctrl_c().await?;
    drasi.stop().await?;
    tracing::info!("execution-ledger-drasi: stopped");

    Ok(())
}

/// Writes a `drasi:heartbeat` KV entry every 60s with a 180s TTL (3x the
/// interval) — Drasi can't monitor its own absence, so this is read by
/// `execution-ledger-api`'s `/health` (which reports `status: "degraded"`
/// once the entry has expired) and by the independent
/// `platform_health.drasi-heartbeat-check` connector action, which runs on
/// the ordinary scheduler+worker path and alerts even when this process is
/// fully down. The TTL expiry itself *is* the staleness signal — no
/// timestamp comparison needed on the reading side, `kv_get` already
/// filters out expired rows.
fn spawn_heartbeat_writer(pool: sqlx::PgPool) {
    const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
    const HEARTBEAT_TTL_SECONDS: i64 = 180;
    const HEARTBEAT_KEY: &str = "drasi:heartbeat";

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
        loop {
            ticker.tick().await;
            if let Err(error) = execution_ledger_core::db::kv_set(
                &pool,
                HEARTBEAT_KEY,
                &json!({ "source": "execution-ledger-drasi" }),
                None,
                Some(HEARTBEAT_TTL_SECONDS),
            )
            .await
            {
                tracing::warn!(%error, "drasi: heartbeat write failed");
            }
        }
    });
}

/// Resolve the delivery target: Event Grid (MI auth) or raw webhook.
///
/// Returns (delivery_url, optional_bearer_token, mode_label).
async fn resolve_delivery_target(
    config: &ExecutionLedgerConfig,
) -> anyhow::Result<(String, Option<String>, &'static str)> {
    if let Some(ref eg_endpoint) = config.event_grid_topic_endpoint {
        let credential = ManagedIdentityCredential::new(None)?;
        let token = credential
            .get_token(&[EVENTGRID_SCOPE], None)
            .await
            .map_err(
                |source| execution_ledger_core::ExecutionLedgerError::DependencyFailed {
                    dependency: "azure_identity (eventgrid token)",
                    source: Box::new(source),
                },
            )?;
        let access_token = token.token.secret().to_string();
        if access_token.trim().is_empty() {
            anyhow::bail!("managed identity returned empty Event Grid access token");
        }
        tracing::info!("drasi: using Event Grid topic with managed identity auth");
        return Ok((
            eg_endpoint.clone(),
            Some(format!("Bearer {access_token}")),
            "eventgrid",
        ));
    }

    let webhook_url = config.worker_webhook_url.as_deref().ok_or(
        execution_ledger_core::ExecutionLedgerError::InvalidConfiguration {
            name: "EXECUTION_LEDGER_WORKER_WEBHOOK_URL (or EXECUTION_LEDGER_EVENT_GRID_TOPIC_ENDPOINT)".to_string(),
        },
    )?;
    tracing::info!("drasi: using raw webhook URL (Event Grid not configured)");
    Ok((webhook_url.to_string(), None, "webhook"))
}

async fn ensure_publication_exists(
    pool: &sqlx::PgPool,
    config: &DrasiConfig,
) -> execution_ledger_core::error::Result<()> {
    let exists: (bool,) =
        sqlx::query_as("SELECT EXISTS (SELECT 1 FROM pg_publication WHERE pubname = $1)")
            .bind(&config.source_publication_name)
            .fetch_one(pool)
            .await
            .map_err(
                |source| execution_ledger_core::ExecutionLedgerError::DependencyFailed {
                    dependency: "postgres",
                    source: Box::new(source),
                },
            )?;

    if !exists.0 {
        let table_list = config.source_tables.join(", ");
        let sql = format!(
            "CREATE PUBLICATION {} FOR TABLE {}",
            config.source_publication_name, table_list
        );
        tracing::info!(
            publication = %config.source_publication_name,
            tables = %table_list,
            "creating publication for drasi source"
        );
        // Table names come from our config file, not HTTP input — audited.
        // sqlx 0.9 gates dynamic SQL behind AssertSqlSafe to prevent
        // injection. Table names here are from our own config, not user input.
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(pool)
            .await
            .map_err(
                |source| execution_ledger_core::ExecutionLedgerError::DependencyFailed {
                    dependency: "postgres",
                    source: Box::new(source),
                },
            )?;
    }

    Ok(())
}

struct PostgresConnectionParts {
    host: String,
    port: u16,
    database: String,
    user: String,
    password: String,
}

impl PostgresConnectionParts {
    fn parse(database_url: &str) -> execution_ledger_core::error::Result<Self> {
        let url = Url::parse(database_url).map_err(|_| {
            execution_ledger_core::ExecutionLedgerError::InvalidConfiguration {
                name: "EXECUTION_LEDGER_DATABASE_URL".to_string(),
            }
        })?;

        let host = url
            .host_str()
            .ok_or(
                execution_ledger_core::ExecutionLedgerError::InvalidConfiguration {
                    name: "EXECUTION_LEDGER_DATABASE_URL".to_string(),
                },
            )?
            .to_string();
        let database = url.path().trim_start_matches('/').to_string();

        Ok(Self {
            host,
            port: url.port().unwrap_or(5432),
            database,
            user: url.username().to_string(),
            password: url.password().unwrap_or_default().to_string(),
        })
    }
}
