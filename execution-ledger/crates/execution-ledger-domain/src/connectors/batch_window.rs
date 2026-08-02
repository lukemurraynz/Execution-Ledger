//! Batch-window aggregation connector.
//!
//! Correlates arrivals sharing a `windowKey` (e.g.
//! `"farmlands:branch-014"`) into one open window, then a Drasi continuous
//! query (`due-batch-windows` in `drasi.yaml`) fires the moment the window
//! closes — either the configured time elapses, or `expectedCount` arrivals
//! have landed, whichever comes first — and triggers `jobId` with the
//! collected items as its payload.
//!
//! This connector only implements the arrival side (`add-item`). Release is
//! not a connector action: it's the internal
//! `POST /platform/internal/batch-windows/due` handler
//! (`execution-ledger-api/src/routes/batch_windows.rs`), invoked reactively
//! by the Drasi reaction — a job author never calls it directly.
//!
//! Unlike every other connector, this one doesn't talk to an *external*
//! system: it calls Execution Ledger's own internal API, the same way
//! Drasi's `due-schedules` reaction already treats the platform's API as
//! just another HTTP endpoint. This keeps the connector-step execution
//! context (`ConnectorExecutionContext`) free of a database pool — the
//! actual Postgres work happens behind the API, authenticated the same way
//! Drasi already authenticates to it.
//!
//! ## Configuration
//!
//! | Key | Source | Required | Default | Notes |
//! |-----|--------|----------|---------|-------|
//! | `windowKey` | config or input | yes | — | Correlation key; concurrent arrivals with the same key share one window |
//! | `jobId` | config | yes | — | Job triggered with the collected items when the window closes |
//! | `windowMinutes` | config | yes | — | Window duration from first arrival |
//! | `expectedCount` | config | no | — | Release early once this many items have arrived |
//! | `apiUrl` | config or env | no | `EXECUTION_LEDGER_API_INTERNAL_BATCH_WINDOWS_ADD_ITEM_URL` | Internal API endpoint |
//!
//! ## Fail-closed behavior
//!
//! - Returns `InvalidConfiguration` if `windowKey`, `jobId`, `windowMinutes`,
//!   or the internal API URL is missing.
//! - Returns `DependencyFailed` if the internal API call fails or returns a
//!   non-2xx status.

use std::sync::LazyLock;
use std::time::Duration;

use execution_ledger_core::ExecutionLedgerError;
use serde_json::{Value, json};

use crate::connectors::helpers::{input_string, step_config_string, step_config_u64};
use crate::connectors::{ConnectorExecutionContext, ConnectorStep};

const ADD_ITEM_URL_ENV: &str = "EXECUTION_LEDGER_API_INTERNAL_BATCH_WINDOWS_ADD_ITEM_URL";
const GATEWAY_SECRET_ENV: &str = "EXECUTION_LEDGER_APIM_GATEWAY_SECRET";

#[allow(clippy::expect_used)]
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest::Client::builder().build() should not fail with valid config")
});

pub(crate) async fn add_item(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let window_key = step_config_string(step, "windowKey")
        .or_else(|| input_string(step, "windowKey"))
        .ok_or(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.batch_window.windowKey".to_string(),
        })?;
    let job_id = step_config_string(step, "jobId").ok_or(
        ExecutionLedgerError::InvalidConfiguration {
            name: "connector.batch_window.jobId".to_string(),
        },
    )?;
    let window_minutes =
        step_config_u64(step, "windowMinutes").ok_or(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.batch_window.windowMinutes".to_string(),
        })?;
    let expected_count = step_config_u64(step, "expectedCount");
    let api_url = step_config_string(step, "apiUrl")
        .or_else(|| std::env::var(ADD_ITEM_URL_ENV).ok())
        .ok_or(ExecutionLedgerError::InvalidConfiguration {
            name: format!("connector.batch_window.apiUrl (or {ADD_ITEM_URL_ENV})"),
        })?;
    let gateway_secret = std::env::var(GATEWAY_SECRET_ENV).ok();

    let body = json!({
        "windowKey": window_key,
        "jobId": job_id,
        "windowMinutes": window_minutes,
        "expectedCount": expected_count,
        "item": step.input,
        "correlationId": ctx.correlation_id,
    });

    let mut request = HTTP_CLIENT.post(&api_url).json(&body);
    if let Some(secret) = gateway_secret {
        request = request.bearer_auth(secret);
    }

    let response = request
        .send()
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "batch_window",
            source: Box::new(source),
        })?;

    let status = response.status();
    let response_body: Value = response.json().await.unwrap_or(Value::Null);

    if !status.is_success() {
        return Err(ExecutionLedgerError::DependencyFailed {
            dependency: "batch_window",
            source: Box::new(std::io::Error::other(format!(
                "batch window add-item failed: {status} {response_body}"
            ))),
        });
    }

    Ok(json!({
        "stepId": step.id,
        "connector": "batch_window",
        "action": "add-item",
        "windowKey": window_key,
        "correlationId": ctx.correlation_id,
        "event": ctx.event,
        "window": response_body,
    }))
}

/// The item this step contributed already lives in the batch window row
/// (owned by the internal API, not this connector) — there is nothing to
/// remove from an aggregate collection another branch's arrival may already
/// share, so compensation is a no-op like every other "already sent"
/// connector (`ms_graph_notify`, `sftp` upload).
#[allow(clippy::unused_async)] // satisfies async connector interface; dispatched via .await in mod.rs
pub(crate) async fn compensate(
    _ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
    _original_output: &Value,
) -> Result<Value, ExecutionLedgerError> {
    Ok(json!({
        "stepId": step.id,
        "connector": "batch_window",
        "compensated": true,
        "action": "noop",
        "reason": "item already merged into a shared window; not safely removable"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::ConnectorKind;
    use uuid::Uuid;

    fn test_ctx() -> ConnectorExecutionContext<'static> {
        ConnectorExecutionContext {
            run_id: Uuid::new_v4(),
            correlation_id: "test-correlation",
            job_id: "test-job",
            event: "test",
            environment: "test",
            upstream_output: None,
        }
    }

    fn test_step(config: Value) -> ConnectorStep {
        ConnectorStep {
            id: "batch-window-step".to_string(),
            connector: ConnectorKind::BatchWindow,
            action: Some("add-item".to_string()),
            external_id: None,
            input: json!({"file": "branch-014.inv"}),
            config,
            input_schema: None,
            output_schema: None,
        }
    }

    #[tokio::test]
    async fn missing_window_key_is_invalid_configuration() {
        let ctx = test_ctx();
        let step = test_step(json!({
            "jobId": "farmlands.invoice.aggregate",
            "windowMinutes": 60,
            "apiUrl": "http://127.0.0.1:0/add-item"
        }));
        let result = add_item(&ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            message.contains("windowKey"),
            "expected windowKey error, got: {message:?}"
        );
    }

    #[tokio::test]
    async fn missing_job_id_is_invalid_configuration() {
        let ctx = test_ctx();
        let step = test_step(json!({
            "windowKey": "farmlands:branch-014",
            "windowMinutes": 60,
            "apiUrl": "http://127.0.0.1:0/add-item"
        }));
        let result = add_item(&ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            message.contains("jobId"),
            "expected jobId error, got: {message:?}"
        );
    }

    #[tokio::test]
    async fn missing_window_minutes_is_invalid_configuration() {
        let ctx = test_ctx();
        let step = test_step(json!({
            "windowKey": "farmlands:branch-014",
            "jobId": "farmlands.invoice.aggregate",
            "apiUrl": "http://127.0.0.1:0/add-item"
        }));
        let result = add_item(&ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            message.contains("windowMinutes"),
            "expected windowMinutes error, got: {message:?}"
        );
    }

    #[tokio::test]
    async fn missing_api_url_is_invalid_configuration() {
        // SAFETY (test-only): no other test in this process depends on
        // ADD_ITEM_URL_ENV being set, and tests within a module run
        // single-threaded per binary unless explicitly parallelized with
        // shared env state — this mirrors the same env-var-fallback testing
        // approach other connectors use.
        // Only assert when the env var truly isn't set, to avoid flaking in
        // an environment where it happens to be configured.
        if std::env::var(ADD_ITEM_URL_ENV).is_err() {
            let ctx = test_ctx();
            let step = test_step(json!({
                "windowKey": "farmlands:branch-014",
                "jobId": "farmlands.invoice.aggregate",
                "windowMinutes": 60
            }));
            let result = add_item(&ctx, &step).await;
            let message = result.map_or_else(|e| e.to_string(), |_| String::new());
            assert!(
                message.contains("apiUrl"),
                "expected apiUrl error, got: {message:?}"
            );
        }
    }

    #[tokio::test]
    async fn compensate_is_always_noop() {
        let ctx = test_ctx();
        let step = test_step(json!({}));
        let result = compensate(&ctx, &step, &json!({})).await;
        let value = result.unwrap_or_else(|e| json!({"compensateError": e.to_string()}));
        assert_eq!(
            value.get("action").and_then(Value::as_str),
            Some("noop")
        );
    }
}
