use crate::error::{ExecutionLedgerError, Result};
use crate::{ExecutionLedgerIntent, ExecutionLedgerIntentSource};
use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

/// Checks whether `value` is a 32-character lowercase hex string
/// (the shape of a W3C Trace Context trace-id).
pub fn looks_like_trace_id(value: &str) -> bool {
    value.len() == 32
        && value.as_bytes().iter().all(u8::is_ascii_hexdigit)
        && value == value.to_lowercase()
}

/// Builds a validated [`ExecutionLedgerIntent`] from the common fields
/// shared by the API trigger path and the CLI job-launch path.
///
/// The caller supplies `source` (Api, Manual, Scheduler, etc.) —
/// everything else is derived from the arguments.
pub fn build_intent(
    run_id: Uuid,
    job_id: &str,
    event: &str,
    source: ExecutionLedgerIntentSource,
    data: serde_json::Value,
    correlation_id: &str,
) -> Result<ExecutionLedgerIntent> {
    let trace_id = if looks_like_trace_id(correlation_id) {
        Some(correlation_id.to_string())
    } else {
        None
    };

    let intent = ExecutionLedgerIntent {
        id: run_id,
        source,
        event: event.to_string(),
        target: Some(job_id.to_string()),
        data: json!({
            "input": data,
            "correlationId": correlation_id
        }),
        timestamp: Utc::now(),
        trace_id,
        span_id: None,
        parent_span_id: None,
    };
    intent
        .validate()
        .map_err(|error| ExecutionLedgerError::InvalidConfiguration {
            name: format!("ExecutionLedgerIntent: {error}"),
        })?;
    Ok(intent)
}
