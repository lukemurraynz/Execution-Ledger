//! Platform health connector.
//!
//! Validates platform connectivity and readiness by delegating to
//! `platform::health_report`. This connector requires no external
//! configuration and always succeeds — it's useful as a gate step
//! before committing to downstream work.

use execution_ledger_core::ExecutionLedgerError;
use serde_json::{Value, json};

use crate::connectors::{ConnectorExecutionContext, ConnectorStep};
use crate::platform;

#[allow(clippy::unused_async)] // satisfies async connector interface; dispatched via .await in mod.rs
pub(crate) async fn execute(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    Ok(json!({
        "stepId": step.id,
        "connector": "platform_health",
        "result": platform::health_report(ctx.environment),
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Compensation is a no-op for `platform_health`: health checks have no
/// side effects to undo.
#[allow(clippy::unused_async)] // satisfies async connector interface; dispatched via .await in mod.rs
pub(crate) async fn compensate(
    _ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
    _original_output: &Value,
) -> Result<Value, ExecutionLedgerError> {
    Ok(json!({
        "stepId": step.id,
        "connector": "platform_health",
        "compensated": true,
        "action": "noop"
    }))
}
