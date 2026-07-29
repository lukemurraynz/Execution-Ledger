//! Platform health and GC handlers.
//!
//! `/health` is deliberately unauthenticated — orchestrator liveness probes
//! do not carry bearer tokens.
//! `/platform/gc` requires `platform:write` scope (enforced by router layer).

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use execution_ledger_core::observability;
use execution_ledger_domain::platform::HealthReport;
use execution_ledger_domain::platform::{degraded_health_report, health_report};
use serde::Serialize;

use crate::AppState;

const DRASI_HEARTBEAT_KEY: &str = "drasi:heartbeat";

/// Real database health check — mirrors `health.route.ts`'s dependency
/// probe rather than returning a static "ok". Also reports `degraded`
/// (still HTTP 200 — the API itself is serving fine) when Drasi's
/// `drasi:heartbeat` KV entry has expired, so a fully-down Drasi process is
/// visible here instead of failing silently. The TTL on that entry (see
/// `execution-ledger-drasi::spawn_heartbeat_writer`) is the staleness
/// signal — `kv_get` returning `None` already means "expired or never
/// written," no timestamp comparison needed on this side.
pub(crate) async fn health(
    State(state): State<Arc<AppState>>,
) -> Result<Json<HealthReport>, StatusCode> {
    sqlx::query("SELECT 1")
        .execute(&state.pool)
        .await
        .map_err(|error| {
            tracing::error!(%error, "health check: database unreachable");
            StatusCode::SERVICE_UNAVAILABLE
        })?;

    let drasi_heartbeat = execution_ledger_core::db::kv_get(&state.pool, DRASI_HEARTBEAT_KEY)
        .await
        .unwrap_or(None);

    let report = if drasi_heartbeat.is_some() {
        health_report(&state.environment)
    } else {
        let mut services = HashMap::new();
        services.insert("drasi".to_string(), "stale".to_string());
        degraded_health_report(
            &state.environment,
            services,
            vec!["drasi: heartbeat missing or expired (no update in ~3m)".to_string()],
        )
    };

    Ok(Json(report))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlatformGcResponse {
    pub terminal_runs_deleted: u64,
    pub stale_schedules_deleted: u64,
    pub expired_kv_deleted: u64,
    pub offload_files_deleted: u64,
}

pub(crate) async fn platform_gc(
    State(state): State<Arc<AppState>>,
) -> Result<Json<PlatformGcResponse>, StatusCode> {
    let summary = execution_ledger_core::db::gc_cleanup_batch(&state.pool, 500)
        .await
        .map_err(|error| {
            tracing::error!(%error, "platform gc failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    observability::emit_runtime_event(
        "platform.gc.completed",
        &serde_json::json!({
            "terminalRunsDeleted": summary.terminal_runs_deleted,
            "staleSchedulesDeleted": summary.stale_schedules_deleted,
            "expiredKvDeleted": summary.expired_kv_deleted,
            "offloadFilesDeleted": summary.offload_files_deleted
        }),
    );
    observability::log_counter(
        "execution_ledger_platform_gc_total",
        1,
        &serde_json::json!({"outcome":"completed"}),
    );
    #[allow(clippy::cast_precision_loss)] // u64 count → f64 for metrics; lossless up to 2^53
    observability::log_histogram(
        "execution_ledger_platform_gc_terminal_runs_deleted",
        summary.terminal_runs_deleted as f64,
        &serde_json::json!({}),
    );

    Ok(Json(PlatformGcResponse {
        terminal_runs_deleted: summary.terminal_runs_deleted,
        stale_schedules_deleted: summary.stale_schedules_deleted,
        expired_kv_deleted: summary.expired_kv_deleted,
        offload_files_deleted: summary.offload_files_deleted,
    }))
}
