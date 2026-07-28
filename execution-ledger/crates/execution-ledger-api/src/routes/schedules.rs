//! Schedule management handlers: list, register, enable, disable,
//! and internal Drasi-reactive due-schedule enqueue.
//!
//! User-facing handlers require `platform:write` scope (enforced by router
//! layer). The internal endpoint uses gateway-secret Bearer auth for
//! Drasi-to-API calls within the Container Apps Environment.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use uuid::Uuid;

use crate::{AppState, ListRunsQuery};

// ── Request / response types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct RegisterScheduleRequest {
    #[serde(rename = "schedulerId")]
    scheduler_id: String,
    #[serde(rename = "jobId")]
    job_id: String,
    #[serde(rename = "cronExpression")]
    cron_expression: String,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default, rename = "nextRun")]
    next_run: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ScheduleIdPath {
    id: String,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub(crate) async fn list_schedules(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListRunsQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let rows = execution_ledger_core::db::list_schedules(&state.pool, limit)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to list schedules");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(serde_json::json!({ "schedules": rows })))
}

pub(crate) async fn register_schedule(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RegisterScheduleRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let timezone = body.timezone.as_deref().unwrap_or("UTC");
    let next_run = match body.next_run.as_deref() {
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .map_err(|_| StatusCode::BAD_REQUEST)
            .unwrap_or_else(|_| {
                tracing::warn!("invalid next_run format: {s}, defaulting to now");
                chrono::Utc::now()
            }),
        None => chrono::Utc::now(),
    };
    let version = execution_ledger_core::db::scheduler_release_upsert(
        &state.pool,
        &execution_ledger_core::db::ScheduleReleaseSpec {
            scheduler_id: &body.scheduler_id,
            job_id: &body.job_id,
            cron_expression: &body.cron_expression,
            timezone,
            next_run,
        },
    )
    .await
    .map_err(|error| {
        tracing::error!(%error, scheduler_id = %body.scheduler_id, "failed to register schedule");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(serde_json::json!({
        "scheduler_id": body.scheduler_id,
        "version": version,
    })))
}

pub(crate) async fn enable_schedule(
    State(state): State<Arc<AppState>>,
    Path(path): Path<ScheduleIdPath>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let changed = execution_ledger_core::db::scheduler_set_status(&state.pool, &path.id, "ACTIVE")
        .await
        .map_err(|error| {
            tracing::error!(%error, scheduler_id = %path.id, "failed to enable schedule");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if !changed {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(
        serde_json::json!({ "scheduler_id": path.id, "status": "ACTIVE" }),
    ))
}

pub(crate) async fn disable_schedule(
    State(state): State<Arc<AppState>>,
    Path(path): Path<ScheduleIdPath>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let changed =
        execution_ledger_core::db::scheduler_set_status(&state.pool, &path.id, "DISABLED")
            .await
            .map_err(|error| {
                tracing::error!(%error, scheduler_id = %path.id, "failed to disable schedule");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
    if !changed {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(
        serde_json::json!({ "scheduler_id": path.id, "status": "DISABLED" }),
    ))
}

// ── Internal: Drasi-reactive due-schedule enqueue ──────────────────────────

/// Receives due-schedule change events from the Drasi `due-schedules`
/// continuous query and enqueues intents. This is a secondary catch-up
/// path — the scheduler's 60-second tick is the primary enqueue mechanism.
///
/// Handles both Drasi change-event envelopes (`{ changes: [{ data: {...} }] }`)
/// and raw schedule objects.
pub(crate) async fn internal_schedules_due(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let schedule_entries: Vec<&serde_json::Value> = extract_schedule_entries(&payload);

    let mut enqueued = 0_i32;
    for entry in schedule_entries {
        let Some(scheduler_id) = entry.get("scheduler_id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(job_id) = entry.get("job_id").and_then(|v| v.as_str()) else {
            continue;
        };
        if scheduler_id.is_empty() || job_id.is_empty() {
            continue;
        }

        let _tenant_id = entry
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok());

        let correlation_id = Uuid::new_v4().to_string();
        let intent = crate::routes::jobs::build_job_intent(
            Uuid::new_v4(),
            job_id,
            "target.invoked",
            serde_json::json!({
                "scheduledFor": entry.get("next_run").unwrap_or(&serde_json::Value::Null),
                "schedulerId": scheduler_id,
                "parameters": entry.get("parameters").unwrap_or(&serde_json::Value::Null),
            }),
            &correlation_id,
        )?;

        crate::routes::jobs::enqueue_job_intent(&state, &intent).await?;
        enqueued += 1;
    }

    Ok(Json(serde_json::json!({
        "accepted": true,
        "enqueued": enqueued,
    })))
}

/// Extracts schedule entries from either a Drasi change-event envelope
/// (`{ changes: [...] }`) or a bare schedule object/array.
fn extract_schedule_entries(payload: &serde_json::Value) -> Vec<&serde_json::Value> {
    if let Some(changes) = payload.get("changes").and_then(|v| v.as_array()) {
        return changes
            .iter()
            .filter_map(|c| c.get("data"))
            .collect();
    }
    if let Some(arr) = payload.as_array() {
        return arr.iter().collect();
    }
    vec![payload]
}
