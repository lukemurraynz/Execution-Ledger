//! Platform recovery handlers: run lease recovery, schedule lease recovery,
//! local poison queue drain, Service Bus poison queue drain.
//!
//! All handlers require `platform:write` scope (enforced by router layer).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use execution_ledger_core::{QueueEngine, observability};
use serde::Serialize;

use crate::{AppState, ListRunsQuery};

// ── Response types ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunLeaseRecoveryResponse {
    requeued_pending: u64,
    failed_terminal: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScheduleLeaseRecoveryResponse {
    released: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PoisonDrainResponse {
    drained: u64,
    engine: &'static str,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub(crate) async fn platform_recover_run_leases(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListRunsQuery>,
) -> Result<Json<RunLeaseRecoveryResponse>, StatusCode> {
    let limit = query.limit.unwrap_or(500).clamp(1, 5000);
    let summary = execution_ledger_core::db::recover_expired_run_leases(&state.pool, limit)
        .await
        .map_err(|error| {
            tracing::error!(%error, limit, "run lease recovery failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    observability::emit_runtime_event(
        "platform.recovery.runs.completed",
        &serde_json::json!({
            "limit": limit,
            "requeuedPending": summary.requeued_pending,
            "failedTerminal": summary.failed_terminal
        }),
    );
    observability::log_counter(
        "execution_ledger_platform_recovery_runs_total",
        1,
        &serde_json::json!({"outcome":"completed"}),
    );

    Ok(Json(RunLeaseRecoveryResponse {
        requeued_pending: summary.requeued_pending,
        failed_terminal: summary.failed_terminal,
    }))
}

pub(crate) async fn platform_recover_schedule_leases(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListRunsQuery>,
) -> Result<Json<ScheduleLeaseRecoveryResponse>, StatusCode> {
    let limit = query.limit.unwrap_or(200).clamp(1, 5000);
    let released = execution_ledger_core::db::recover_expired_schedule_leases(&state.pool, limit)
        .await
        .map_err(|error| {
            tracing::error!(%error, limit, "schedule lease recovery failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    observability::emit_runtime_event(
        "platform.recovery.schedules.completed",
        &serde_json::json!({
            "limit": limit,
            "released": released
        }),
    );
    observability::log_counter(
        "execution_ledger_platform_recovery_schedules_total",
        1,
        &serde_json::json!({"outcome":"completed"}),
    );

    Ok(Json(ScheduleLeaseRecoveryResponse { released }))
}

pub(crate) async fn platform_drain_local_poison_queue(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListRunsQuery>,
) -> Result<Json<PoisonDrainResponse>, StatusCode> {
    let limit = query.limit.unwrap_or(500).clamp(1, 5000);
    let drained = execution_ledger_core::db::drain_local_dead_letter_queue(&state.pool, limit)
        .await
        .map_err(|error| {
            tracing::error!(%error, limit, "local poison queue drain failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    observability::emit_runtime_event(
        "platform.recovery.local_poison.completed",
        &serde_json::json!({
            "limit": limit,
            "drained": drained
        }),
    );
    observability::log_counter(
        "execution_ledger_platform_recovery_local_poison_total",
        1,
        &serde_json::json!({"outcome":"completed"}),
    );

    Ok(Json(PoisonDrainResponse {
        drained,
        engine: "local",
    }))
}

pub(crate) async fn platform_drain_servicebus_poison_queue(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListRunsQuery>,
) -> Result<Json<PoisonDrainResponse>, StatusCode> {
    if !matches!(state.queue_engine, QueueEngine::ServiceBus) {
        return Err(StatusCode::CONFLICT);
    }

    #[allow(clippy::cast_sign_loss)] // clamped to 1..5000, always non-negative
    let limit = query.limit.unwrap_or(200).clamp(1, 5000) as usize;
    let Some(client) = state.servicebus_client.as_ref() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };

    let drained = client
        .drain_dead_letter_messages(limit)
        .await
        .map_err(|error| {
            tracing::error!(%error, limit, "service bus poison queue drain failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    observability::emit_runtime_event(
        "platform.recovery.servicebus_poison.completed",
        &serde_json::json!({
            "limit": limit,
            "drained": drained
        }),
    );
    observability::log_counter(
        "execution_ledger_platform_recovery_servicebus_poison_total",
        1,
        &serde_json::json!({"outcome":"completed"}),
    );

    Ok(Json(PoisonDrainResponse {
        drained,
        engine: "servicebus",
    }))
}
