//! Job-run handlers: trigger, list, get, steps, replay, skip, compensate.
//!
//! All handlers are tenant-scoped — every query includes `tenant_id` from the
//! validated bearer token so one tenant can never read or mutate another
//! tenant's data.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::Utc;
use execution_ledger_core::{
    AuthenticatedTenant, ExecutionLedgerIntent, ExecutionLedgerIntentSource, QueueEngine,
    build_intent, queue,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::{AppState, ListRunsQuery};

const INTENT_QUEUE_NAME: &str = "execution-ledger-intents";

// ── Request / response types ─────────────────────────────────────────────────

#[derive(Debug, Serialize, sqlx::FromRow)]
pub(crate) struct JobRun {
    pub id: Uuid,
    pub job_id: String,
    pub status: String,
    pub correlation_id: String,
    pub current_attempt: i32,
    pub max_attempts: i32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TriggerJobRequest {
    job_id: String,
    #[serde(default)]
    data: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TriggerJobResponse {
    accepted: bool,
    intent_id: Uuid,
    run_id: Uuid,
    job_id: String,
    status: String,
    correlation_id: String,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobRunListItem {
    id: Uuid,
    job_id: String,
    status: String,
    correlation_id: String,
    created_at: chrono::DateTime<Utc>,
    updated_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobRunStep {
    step_id: String,
    external_id: Option<String>,
    step_idempotency_key: Option<String>,
    status: String,
    attempt: i32,
    idx: i32,
    output: Option<serde_json::Value>,
    error: Option<serde_json::Value>,
    created_at: chrono::DateTime<Utc>,
    updated_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReplayRunRequest {
    #[serde(default)]
    step_ids: Vec<String>,
    /// Required to replay a step whose connector/action
    /// `execution_ledger_domain::connectors::contracts::replay_safety`
    /// classifies as `NotSafeWithoutGuard` (e.g. `ms_graph_notify`, or
    /// `rest`/`erp` where this platform can't know whether the target
    /// treats a repeat call as idempotent) — replaying such a step repeats
    /// a real external action, not just an in-platform retry. Ignored when
    /// no targeted step needs it.
    #[serde(default)]
    force: bool,
}

/// Returned when one or more explicitly targeted replay steps are not safe
/// to replay without confirmation — see `ReplayRunRequest::force`.
pub(crate) enum ReplayError {
    NotFound,
    Internal,
    UnsafeStepsBlocked(Vec<execution_ledger_domain::connectors::contracts::UnsafeReplayStep>),
}

impl axum::response::IntoResponse for ReplayError {
    fn into_response(self) -> axum::response::Response {
        match self {
            ReplayError::NotFound => StatusCode::NOT_FOUND.into_response(),
            ReplayError::Internal => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            ReplayError::UnsafeStepsBlocked(steps) => (
                StatusCode::CONFLICT,
                axum::Json(json!({
                    "error": "replay_blocked_unsafe_steps",
                    "message": "One or more targeted steps repeat a real external action on replay rather than an in-platform retry. Set \"force\": true in the request body to proceed anyway.",
                    "unsafeSteps": steps,
                })),
            )
                .into_response(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompensateRunRequest {
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    data: serde_json::Value,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// Real lookup against `EXECUTION_LEDGER_jobs_runs`, mirroring `jobs.route.ts`'s
/// job-run-by-id endpoint. Scoped by the caller's `tenant_id` from the
/// validated token — never by anything the caller supplies — so one tenant
/// can never read another tenant's job run, even by guessing its id.
pub(crate) async fn get_job_run(
    State(state): State<Arc<AppState>>,
    Extension(authenticated): Extension<AuthenticatedTenant>,
    Path(id): Path<Uuid>,
) -> Result<Json<JobRun>, StatusCode> {
    let job_run = sqlx::query_as::<_, JobRun>(
        "SELECT id, job_id, status, correlation_id, current_attempt, max_attempts \
         FROM EXECUTION_LEDGER_jobs_runs WHERE id = $1 AND tenant_id = $2",
    )
    .bind(id)
    .bind(authenticated.tenant_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|error| {
        tracing::error!(%error, "job run lookup failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(job_run))
}

pub(crate) async fn trigger_job(
    State(state): State<Arc<AppState>>,
    Extension(authenticated): Extension<AuthenticatedTenant>,
    headers: HeaderMap,
    Json(payload): Json<TriggerJobRequest>,
) -> Result<Json<TriggerJobResponse>, StatusCode> {
    if payload.job_id.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let correlation_id = parse_correlation_id(&headers)?;
    let run_id = idempotency_key.unwrap_or_else(|| {
        let material = format!(
            "{}:{}",
            authenticated.tenant_id,
            Utc::now().timestamp_millis()
        );
        Uuid::new_v5(&Uuid::NAMESPACE_OID, material.as_bytes())
    });
    let effective_correlation_id = correlation_id.unwrap_or_else(|| run_id.to_string());

    let inserted = sqlx::query_as::<_, (Uuid, Uuid, String, String, String)>(
        "WITH dedup AS (
            SELECT $1::uuid AS idem
         )
         INSERT INTO EXECUTION_LEDGER_jobs_runs (
            id, idempotency_key, correlation_id, job_id, status, input_data, current_attempt, max_attempts, tenant_id
         )
         SELECT d.idem, d.idem, $2, $3, 'pending', $4::jsonb, 1, 4, $5
         FROM dedup d
         ON CONFLICT (idempotency_key) DO NOTHING
         RETURNING id, idempotency_key, job_id, status, correlation_id",
    )
    .bind(run_id)
    .bind(&effective_correlation_id)
    .bind(&payload.job_id)
    .bind(&payload.data)
    .bind(authenticated.tenant_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|error| {
        tracing::error!(%error, "job trigger insert failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if let Some((run_id, intent_id, job_id, status, correlation_id)) = inserted {
        let intent = build_job_intent(
            run_id,
            &job_id,
            "target.invoked",
            payload.data.clone(),
            &correlation_id,
        )?;
        if let Err(status_code) = enqueue_job_intent(&state, &intent).await {
            if let Err(update_err) = sqlx::query(
                "UPDATE EXECUTION_LEDGER_jobs_runs
                 SET status = 'failed',
                     completed_at = now(),
                     updated_at = now(),
                    lease_owner = NULL,
                    lease_expires_at = NULL,
                    last_error = jsonb_build_object('message', 'intent enqueue failed')
                 WHERE id = $1",
            )
            .bind(run_id)
            .execute(&state.pool)
            .await
            {
                tracing::error!(
                    %run_id,
                    %update_err,
                    "trigger_job: failed to mark run as failed after enqueue error — run orphaned"
                );
            }
            return Err(status_code);
        }

        return Ok(Json(TriggerJobResponse {
            accepted: true,
            intent_id,
            run_id,
            job_id,
            status,
            correlation_id,
        }));
    }

    let Some(idempotency_key) = idempotency_key else {
        return Err(StatusCode::CONFLICT);
    };

    let existing = sqlx::query_as::<_, (Uuid, Uuid, String, String, String)>(
        "SELECT id, idempotency_key, job_id, status
         FROM EXECUTION_LEDGER_jobs_runs
         WHERE idempotency_key = $1::uuid
           AND tenant_id = $2",
    )
    .bind(idempotency_key)
    .bind(authenticated.tenant_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|error| {
        tracing::error!(%error, "job trigger dedup lookup failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if let Some((run_id, intent_id, job_id, status, correlation_id)) = existing {
        return Ok(Json(TriggerJobResponse {
            accepted: false,
            intent_id,
            run_id,
            job_id,
            status,
            correlation_id,
        }));
    }

    Err(StatusCode::CONFLICT)
}

pub(crate) async fn list_job_runs(
    State(state): State<Arc<AppState>>,
    Extension(authenticated): Extension<AuthenticatedTenant>,
    Query(query): Query<ListRunsQuery>,
) -> Result<Json<Vec<JobRunListItem>>, StatusCode> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let runs = sqlx::query_as::<_, JobRunListItem>(
        "SELECT id, job_id, status, correlation_id, created_at, updated_at
         FROM EXECUTION_LEDGER_jobs_runs
         WHERE tenant_id = $1
         ORDER BY created_at DESC
         LIMIT $2",
    )
    .bind(authenticated.tenant_id)
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(|error| {
        tracing::error!(%error, "job runs list failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(runs))
}

pub(crate) async fn list_job_run_steps(
    State(state): State<Arc<AppState>>,
    Extension(authenticated): Extension<AuthenticatedTenant>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<JobRunStep>>, StatusCode> {
    let steps = sqlx::query_as::<_, JobRunStep>(
        "SELECT s.step_id, s.external_id, s.step_idempotency_key, s.status, s.attempt, s.idx, s.output, s.error, s.created_at, s.updated_at
         FROM EXECUTION_LEDGER_steps_runs s
         JOIN EXECUTION_LEDGER_jobs_runs r ON r.id = s.job_run_id
         WHERE s.job_run_id = $1 AND r.tenant_id = $2
         ORDER BY s.idx, s.created_at",
    )
    .bind(id)
    .bind(authenticated.tenant_id)
    .fetch_all(&state.pool)
    .await
    .map_err(|error| {
        tracing::error!(%error, "job run steps lookup failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(steps))
}

pub(crate) async fn replay_job_run(
    State(state): State<Arc<AppState>>,
    Extension(authenticated): Extension<AuthenticatedTenant>,
    Path(id): Path<Uuid>,
    Json(payload): Json<ReplayRunRequest>,
) -> Result<Json<TriggerJobResponse>, ReplayError> {
    // Safety gate runs before any mutation: a blocked replay leaves the run
    // completely untouched. Only targeted steps (`step_ids` non-empty) are
    // checked — an empty `step_ids` resumes/retries incomplete steps rather
    // than force-replaying already-completed ones, so there's no prior
    // external action being repeated in that case.
    if !payload.step_ids.is_empty() && !payload.force {
        let input_data = sqlx::query_as::<_, (serde_json::Value,)>(
            "SELECT input_data FROM EXECUTION_LEDGER_jobs_runs WHERE id = $1 AND tenant_id = $2",
        )
        .bind(id)
        .bind(authenticated.tenant_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|error| {
            tracing::error!(%error, "job replay safety lookup failed");
            ReplayError::Internal
        })?
        .ok_or(ReplayError::NotFound)?
        .0;

        let unsafe_steps = execution_ledger_domain::connectors::contracts::unsafe_replay_steps(
            &input_data,
            &payload.step_ids,
        );
        if !unsafe_steps.is_empty() {
            return Err(ReplayError::UnsafeStepsBlocked(unsafe_steps));
        }
    }

    let row = sqlx::query_as::<_, (Uuid, String, String)>(
        "UPDATE EXECUTION_LEDGER_jobs_runs
         SET status = 'pending',
             completed_at = NULL,
             updated_at = now(),
             lease_owner = NULL,
             lease_expires_at = NULL,
             last_error = NULL
         WHERE id = $1 AND tenant_id = $2
         RETURNING id, job_id, correlation_id",
    )
    .bind(id)
    .bind(authenticated.tenant_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|error| {
        tracing::error!(%error, "job replay update failed");
        ReplayError::Internal
    })?
    .ok_or(ReplayError::NotFound)?;

    let intent = build_job_intent(
        row.0,
        &row.1,
        "target.replay",
        json!({ "replay": { "stepIds": payload.step_ids } }),
        &row.2,
    )
    .map_err(|_| ReplayError::Internal)?;
    enqueue_job_intent(&state, &intent)
        .await
        .map_err(|_| ReplayError::Internal)?;

    Ok(Json(TriggerJobResponse {
        accepted: true,
        intent_id: row.0,
        run_id: row.0,
        job_id: row.1,
        status: "pending".to_string(),
        correlation_id: row.2,
    }))
}

pub(crate) async fn skip_job_run(
    State(state): State<Arc<AppState>>,
    Extension(authenticated): Extension<AuthenticatedTenant>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    let updated = sqlx::query(
        "UPDATE EXECUTION_LEDGER_jobs_runs
         SET status = 'cancelled',
             completed_at = now(),
             updated_at = now(),
             lease_owner = NULL,
             lease_expires_at = NULL,
             state_data = jsonb_set(COALESCE(state_data, '{}'::jsonb), '{operator,action}', '\"skip\"'::jsonb, true)
         WHERE id = $1 AND tenant_id = $2 AND status IN ('pending', 'running')",
    )
    .bind(id)
    .bind(authenticated.tenant_id)
    .execute(&state.pool)
    .await
    .map_err(|error| {
        tracing::error!(%error, "job skip failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if updated.rows_affected() == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn compensate_job_run(
    State(state): State<Arc<AppState>>,
    Extension(authenticated): Extension<AuthenticatedTenant>,
    Path(id): Path<Uuid>,
    Json(payload): Json<CompensateRunRequest>,
) -> Result<Json<TriggerJobResponse>, StatusCode> {
    let row = sqlx::query_as::<_, (Uuid, String, String)>(
        "UPDATE EXECUTION_LEDGER_jobs_runs
         SET status = 'pending',
             completed_at = NULL,
             updated_at = now(),
             lease_owner = NULL,
             lease_expires_at = NULL,
             last_error = NULL
         WHERE id = $1 AND tenant_id = $2
         RETURNING id, job_id, correlation_id",
    )
    .bind(id)
    .bind(authenticated.tenant_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|error| {
        tracing::error!(%error, "job compensate update failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .ok_or(StatusCode::NOT_FOUND)?;

    let intent = build_job_intent(
        row.0,
        &row.1,
        "target.compensate",
        json!({
            "compensate": {
                "reason": payload.reason,
                "data": payload.data
            }
        }),
        &row.2,
    )?;
    enqueue_job_intent(&state, &intent).await?;

    Ok(Json(TriggerJobResponse {
        accepted: true,
        intent_id: row.0,
        run_id: row.0,
        job_id: row.1,
        status: "pending".to_string(),
        correlation_id: row.2,
    }))
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn parse_correlation_id(headers: &HeaderMap) -> Result<Option<String>, StatusCode> {
    let raw = headers
        .get("x-correlation-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .map(ToString::to_string);

    if let Some(value) = &raw
        && (value.is_empty() || value.len() > 128)
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    Ok(raw)
}

pub(crate) fn build_job_intent(
    run_id: Uuid,
    job_id: &str,
    event: &str,
    data: serde_json::Value,
    correlation_id: &str,
) -> Result<ExecutionLedgerIntent, StatusCode> {
    build_intent(
        run_id,
        job_id,
        event,
        ExecutionLedgerIntentSource::Api,
        data,
        correlation_id,
    )
    .map_err(|_| StatusCode::BAD_REQUEST)
}

pub(crate) async fn enqueue_job_intent(
    state: &AppState,
    intent: &ExecutionLedgerIntent,
) -> Result<(), StatusCode> {
    let enqueue_result = match state.queue_engine {
        QueueEngine::Local => queue::enqueue_local_intent(&state.pool, INTENT_QUEUE_NAME, intent)
            .await
            .map(|_| ()),
        QueueEngine::ServiceBus => {
            let Some(client) = state.servicebus_client.as_ref() else {
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            };
            client.enqueue_intent(intent).await.map(|_| ())
        }
    };

    enqueue_result.map_err(|error| {
        tracing::error!(%error, "job intent enqueue failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}
