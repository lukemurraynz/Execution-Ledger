use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

const DEFAULT_EVENT: &str = "target.invoked";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionLedgerIntentSource {
    Scheduler,
    Api,
    EventGrid,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionLedgerIntent {
    pub id: Uuid,
    pub source: ExecutionLedgerIntentSource,
    #[serde(default = "default_event")]
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default)]
    pub data: serde_json::Value,
    pub timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionLedgerMessage {
    pub version: u8,
    pub run_id: Uuid,
    pub unique_id: Uuid,
    pub job_id: String,
    #[serde(default)]
    pub data: serde_json::Value,
    pub meta: ExecutionLedgerMessageMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionLedgerMessageMeta {
    pub source: ExecutionLedgerIntentSource,
    pub timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EnvelopeValidationError {
    #[error("target is required unless source is event_grid")]
    MissingTarget,
    #[error("trace_id must be 32 lowercase hex characters")]
    InvalidTraceId,
    #[error("span_id must be 16 lowercase hex characters")]
    InvalidSpanId,
    #[error("parent_span_id must be 16 lowercase hex characters")]
    InvalidParentSpanId,
    #[error("message version must be 1")]
    InvalidMessageVersion,
    #[error("job_id must not be empty")]
    EmptyJobId,
}

impl ExecutionLedgerIntent {
    pub fn validate(&self) -> Result<(), EnvelopeValidationError> {
        if !matches!(self.source, ExecutionLedgerIntentSource::EventGrid)
            && self.target.as_deref().is_none_or(str::is_empty)
        {
            return Err(EnvelopeValidationError::MissingTarget);
        }
        validate_trace_like(
            self.trace_id.as_deref(),
            32,
            EnvelopeValidationError::InvalidTraceId,
        )?;
        validate_trace_like(
            self.span_id.as_deref(),
            16,
            EnvelopeValidationError::InvalidSpanId,
        )?;
        validate_trace_like(
            self.parent_span_id.as_deref(),
            16,
            EnvelopeValidationError::InvalidParentSpanId,
        )?;
        Ok(())
    }
}

impl ExecutionLedgerMessage {
    pub fn validate(&self) -> Result<(), EnvelopeValidationError> {
        if self.version != 1 {
            return Err(EnvelopeValidationError::InvalidMessageVersion);
        }
        if self.job_id.trim().is_empty() {
            return Err(EnvelopeValidationError::EmptyJobId);
        }
        validate_trace_like(
            self.meta.trace_id.as_deref(),
            32,
            EnvelopeValidationError::InvalidTraceId,
        )?;
        validate_trace_like(
            self.meta.span_id.as_deref(),
            16,
            EnvelopeValidationError::InvalidSpanId,
        )?;
        validate_trace_like(
            self.meta.parent_span_id.as_deref(),
            16,
            EnvelopeValidationError::InvalidParentSpanId,
        )?;
        Ok(())
    }
}

fn default_event() -> String {
    DEFAULT_EVENT.to_string()
}

fn validate_trace_like(
    value: Option<&str>,
    width: usize,
    error: EnvelopeValidationError,
) -> Result<(), EnvelopeValidationError> {
    if let Some(raw) = value
        && (raw.len() != width
            || !raw.as_bytes().iter().all(u8::is_ascii_hexdigit)
            || raw != raw.to_lowercase())
    {
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_intent(
        source: ExecutionLedgerIntentSource,
        target: Option<&str>,
    ) -> ExecutionLedgerIntent {
        ExecutionLedgerIntent {
            id: Uuid::nil(),
            source,
            event: DEFAULT_EVENT.to_string(),
            target: target.map(ToString::to_string),
            data: serde_json::json!({}),
            timestamp: Utc::now(),
            trace_id: None,
            span_id: None,
            parent_span_id: None,
        }
    }

    #[test]
    fn event_grid_allows_missing_target() {
        let intent = sample_intent(ExecutionLedgerIntentSource::EventGrid, None);
        assert_eq!(intent.validate(), Ok(()));
    }

    #[test]
    fn non_event_grid_requires_target() {
        let intent = sample_intent(ExecutionLedgerIntentSource::Api, None);
        assert_eq!(
            intent.validate(),
            Err(EnvelopeValidationError::MissingTarget)
        );
    }

    #[test]
    fn invalid_trace_id_is_rejected() {
        let mut intent = sample_intent(ExecutionLedgerIntentSource::Api, Some("platform.health"));
        intent.trace_id = Some("ABC".to_string());
        assert_eq!(
            intent.validate(),
            Err(EnvelopeValidationError::InvalidTraceId)
        );
    }

    #[test]
    fn message_version_must_be_one() {
        let msg = ExecutionLedgerMessage {
            version: 2,
            run_id: Uuid::nil(),
            unique_id: Uuid::max(),
            job_id: "platform.health".to_string(),
            data: serde_json::json!({}),
            meta: ExecutionLedgerMessageMeta {
                source: ExecutionLedgerIntentSource::Api,
                timestamp: Utc::now(),
                trace_id: None,
                span_id: None,
                parent_span_id: None,
            },
        };
        assert_eq!(
            msg.validate(),
            Err(EnvelopeValidationError::InvalidMessageVersion)
        );
    }
}
