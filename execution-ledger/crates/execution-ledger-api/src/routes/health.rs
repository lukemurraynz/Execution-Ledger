//! Platform health and GC handlers.
//!
//! `/health` is deliberately unauthenticated — orchestrator liveness probes
//! do not carry bearer tokens.
//! `/platform/gc` requires `platform:write` scope (enforced by router layer).

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use execution_ledger_core::observability;
use execution_ledger_domain::platform::HealthReport;
use execution_ledger_domain::platform::health_report;
use serde::Serialize;

use crate::AppState;

/// Real database health check — mirrors `health.route.ts`'s dependency
/// probe rather than returning a static "ok".
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

    Ok(Json(health_report(&state.environment)))
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
