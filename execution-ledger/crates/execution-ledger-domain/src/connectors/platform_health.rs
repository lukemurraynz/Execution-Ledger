//! Platform health connector.
//!
//! Supports two actions:
//!
//! - **`health-check`** (default when `action` is omitted): validates
//!   platform connectivity and readiness by delegating to
//!   `platform::health_report`. Requires no external configuration and
//!   always succeeds — useful as a gate step before committing to
//!   downstream work.
//! - **`drasi-heartbeat-check`**: the backstop for silent Drasi degradation.
//!   Drasi can't monitor its own absence (if it's down, nothing runs to
//!   detect that), so this action runs on the ordinary scheduler+worker
//!   path — independent of Drasi entirely — and fires an alert if it is.
//!   Calls the platform's own `/health` endpoint (which reports
//!   `status: "degraded"` when the `drasi:heartbeat` KV entry
//!   `execution-ledger-drasi` writes every ~60s has gone missing or
//!   expired) and, if degraded, sends a notification by composing the
//!   `ms_graph_notify` connector in-process rather than duplicating its
//!   Graph API logic.
//!
//! ## Configuration (`drasi-heartbeat-check`)
//!
//! | Key | Source | Required | Default | Notes |
//! |-----|--------|----------|---------|-------|
//! | `healthUrl` | config or env | yes | — | Platform `/health` URL to poll |
//! | `alert` | config | yes | — | `ms_graph_notify` config object, used only if degraded |
//!
//! ## Fail-closed behavior
//!
//! - Returns `InvalidConfiguration` if `healthUrl` or `alert` is missing —
//!   validated eagerly, before checking health, so a misconfigured alert
//!   path is caught in testing rather than discovered silently during a
//!   real Drasi outage.
//! - Returns `DependencyFailed` if the `/health` call fails, or if sending
//!   the alert (when degraded) fails.

use std::sync::LazyLock;
use std::time::Duration;

use execution_ledger_core::ExecutionLedgerError;
use serde_json::{Value, json};

use crate::connectors::helpers::{step_config_object, step_config_string};
use crate::connectors::{ConnectorExecutionContext, ConnectorKind, ConnectorStep};
use crate::platform;

#[allow(clippy::expect_used)]
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest::Client::builder().build() should not fail with valid config")
});

#[allow(clippy::unused_async)] // satisfies async connector interface; dispatched via .await in mod.rs
pub(crate) async fn execute(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    Ok(json!({
        "stepId": step.id,
        "connector": "platform_health",
        "action": "health-check",
        "result": platform::health_report(ctx.environment),
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Polls the platform `/health` endpoint and, if it reports
/// `status: "degraded"`, sends an alert via `ms_graph_notify` — the
/// independent backstop described in the module doc.
pub(crate) async fn drasi_heartbeat_check(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let health_url = step_config_string(step, "healthUrl")
        .or_else(|| std::env::var("EXECUTION_LEDGER_API_HEALTH_URL").ok())
        .ok_or(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.platform_health.healthUrl (or EXECUTION_LEDGER_API_HEALTH_URL)"
                .to_string(),
        })?;
    // Validated eagerly (before the health call) so a broken alert config
    // is caught before it's ever actually needed — see module fail-closed doc.
    let alert_config = step_config_object(step, "alert").cloned().ok_or(
        ExecutionLedgerError::InvalidConfiguration {
            name: "connector.platform_health.alert".to_string(),
        },
    )?;

    let response = HTTP_CLIENT
        .get(&health_url)
        .send()
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "platform_health",
            source: Box::new(source),
        })?;
    let body: Value = response.json().await.unwrap_or(Value::Null);
    let degraded = body.get("status").and_then(Value::as_str) == Some("degraded");

    if !degraded {
        return Ok(json!({
            "stepId": step.id,
            "connector": "platform_health",
            "action": "drasi-heartbeat-check",
            "degraded": false,
            "correlationId": ctx.correlation_id,
            "event": ctx.event
        }));
    }

    let alert_step = ConnectorStep {
        id: format!("{}-alert", step.id),
        connector: ConnectorKind::MsGraphNotify,
        action: None,
        external_id: None,
        input: Value::Null,
        config: Value::Object(alert_config),
        input_schema: None,
        output_schema: None,
    };
    let alert_result = crate::connectors::ms_graph_notify::execute(ctx, &alert_step).await?;

    Ok(json!({
        "stepId": step.id,
        "connector": "platform_health",
        "action": "drasi-heartbeat-check",
        "degraded": true,
        "degradedReasons": body.get("degradedReasons").cloned().unwrap_or(Value::Null),
        "alertSent": alert_result,
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// `health-check` has no side effects to undo. `drasi-heartbeat-check` may
/// have already sent a real notification (when degraded) — not safely
/// retractable, same reasoning as `ms_graph_notify::compensate`.
#[allow(clippy::unused_async)] // satisfies async connector interface; dispatched via .await in mod.rs
pub(crate) async fn compensate(
    _ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
    original_output: &Value,
) -> Result<Value, ExecutionLedgerError> {
    let action = original_output
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("health-check");
    let alert_may_have_sent =
        action == "drasi-heartbeat-check" && original_output.get("alertSent").is_some();
    let reason = if alert_may_have_sent {
        "an alert notification may have already been sent; not safely retractable"
    } else {
        "health checks have no side effects to undo"
    };
    Ok(json!({
        "stepId": step.id,
        "connector": "platform_health",
        "compensated": true,
        "action": "noop",
        "reason": reason
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn test_step(action: Option<&str>, config: Value) -> ConnectorStep {
        ConnectorStep {
            id: "health-step".to_string(),
            connector: ConnectorKind::PlatformHealth,
            action: action.map(str::to_string),
            external_id: None,
            input: Value::Null,
            config,
            input_schema: None,
            output_schema: None,
        }
    }

    #[tokio::test]
    async fn drasi_heartbeat_check_missing_health_url_is_invalid_configuration() {
        let ctx = test_ctx();
        let step = test_step(
            Some("drasi-heartbeat-check"),
            json!({"alert": {"target": "outlook_send_mail"}}),
        );
        let result = drasi_heartbeat_check(&ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            message.contains("healthUrl"),
            "expected healthUrl error, got: {message:?}"
        );
    }

    #[tokio::test]
    async fn drasi_heartbeat_check_missing_alert_config_is_invalid_configuration() {
        let ctx = test_ctx();
        let step = test_step(
            Some("drasi-heartbeat-check"),
            json!({"healthUrl": "http://127.0.0.1:0/health"}),
        );
        let result = drasi_heartbeat_check(&ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            message.contains("alert"),
            "expected alert config error, got: {message:?}"
        );
    }

    #[tokio::test]
    async fn compensate_health_check_reason_mentions_no_side_effects() {
        let ctx = test_ctx();
        let step = test_step(None, json!({}));
        let original_output = json!({"action": "health-check"});
        let result = compensate(&ctx, &step, &original_output).await.unwrap_or_else(
            |e| json!({"compensateError": e.to_string()}),
        );
        assert_eq!(
            result.get("reason").and_then(Value::as_str),
            Some("health checks have no side effects to undo")
        );
    }

    #[tokio::test]
    async fn compensate_drasi_heartbeat_check_with_alert_sent_is_not_retractable() {
        let ctx = test_ctx();
        let step = test_step(Some("drasi-heartbeat-check"), json!({}));
        let original_output = json!({
            "action": "drasi-heartbeat-check",
            "degraded": true,
            "alertSent": {"status": "sent"}
        });
        let result = compensate(&ctx, &step, &original_output).await.unwrap_or_else(
            |e| json!({"compensateError": e.to_string()}),
        );
        assert_eq!(
            result.get("reason").and_then(Value::as_str),
            Some("an alert notification may have already been sent; not safely retractable")
        );
    }

    #[tokio::test]
    async fn compensate_drasi_heartbeat_check_without_alert_sent_has_no_side_effects_reason() {
        let ctx = test_ctx();
        let step = test_step(Some("drasi-heartbeat-check"), json!({}));
        let original_output = json!({"action": "drasi-heartbeat-check", "degraded": false});
        let result = compensate(&ctx, &step, &original_output).await.unwrap_or_else(
            |e| json!({"compensateError": e.to_string()}),
        );
        assert_eq!(
            result.get("reason").and_then(Value::as_str),
            Some("health checks have no side effects to undo")
        );
    }
}
