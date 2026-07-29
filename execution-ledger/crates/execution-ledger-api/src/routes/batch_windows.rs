//! Batch-window internal handlers: arrival ingest and Drasi-reactive release.
//!
//! Both endpoints use gateway-secret Bearer auth for internal service calls
//! (the `batch_window` connector adding an item; Drasi's `due-batch-windows`
//! reaction releasing a closed window) — same trust model as
//! `routes::schedules::internal_schedules_due`.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use uuid::Uuid;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub(crate) struct AddItemRequest {
    #[serde(rename = "windowKey")]
    window_key: String,
    #[serde(rename = "jobId")]
    job_id: String,
    #[serde(rename = "windowMinutes")]
    window_minutes: i32,
    #[serde(default, rename = "expectedCount")]
    expected_count: Option<i32>,
    #[serde(default)]
    item: serde_json::Value,
}

/// Called by the `batch_window` connector's `add-item` action. Upserts the
/// arrival into the open window for `windowKey`.
pub(crate) async fn internal_add_item(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AddItemRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if body.window_key.trim().is_empty() || body.job_id.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if body.window_minutes <= 0 {
        return Err(StatusCode::BAD_REQUEST);
    }

    let row = execution_ledger_core::db::batch_window_add_item(
        &state.pool,
        &execution_ledger_core::db::BatchWindowAddItemSpec {
            window_key: &body.window_key,
            job_id: &body.job_id,
            tenant_id: None,
            window_minutes: body.window_minutes,
            expected_count: body.expected_count,
            item: &body.item,
        },
    )
    .await
    .map_err(|error| {
        tracing::error!(%error, window_key = %body.window_key, "batch window add-item failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(serde_json::json!({
        "id": row.id,
        "windowKey": row.window_key,
        "jobId": row.job_id,
        "itemCount": row.item_count,
        "expectedCount": row.expected_count,
        "windowEndsAt": row.window_ends_at,
        "status": row.status,
    })))
}

/// Receives due-window change events from the Drasi `due-batch-windows`
/// continuous query. Marks each window `released` (idempotent — a second
/// delivery of the same event finds the row already released and is a
/// no-op) and triggers the window's `jobId` with the collected `items`.
///
/// Handles both Drasi change-event envelopes (`{ changes: [{ data: {...} }] }`)
/// and raw window objects, matching `internal_schedules_due`.
pub(crate) async fn internal_batch_windows_due(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let entries = extract_window_entries(&payload);

    let mut released = 0_i32;
    for entry in entries {
        let Some(id) = entry
            .get("id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            continue;
        };

        let Some(release) = execution_ledger_core::db::batch_window_release(&state.pool, id)
            .await
            .map_err(|error| {
                tracing::error!(%error, window_id = %id, "batch window release failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })?
        else {
            // Already released by a prior delivery of this same event — skip.
            continue;
        };

        let correlation_id = Uuid::new_v4().to_string();
        let intent = crate::routes::jobs::build_job_intent(
            Uuid::new_v4(),
            &release.job_id,
            "target.invoked",
            serde_json::json!({
                "batchWindowId": release.id,
                "windowKey": release.window_key,
                "items": release.items,
            }),
            &correlation_id,
        )?;

        crate::routes::jobs::enqueue_job_intent(&state, &intent).await?;
        released += 1;
    }

    Ok(Json(serde_json::json!({
        "accepted": true,
        "released": released,
    })))
}

fn extract_window_entries(payload: &serde_json::Value) -> Vec<&serde_json::Value> {
    if let Some(changes) = payload.get("changes").and_then(|v| v.as_array()) {
        return changes.iter().filter_map(|c| c.get("data")).collect();
    }
    if let Some(arr) = payload.as_array() {
        return arr.iter().collect();
    }
    vec![payload]
}
