//! Shared helpers for reading configuration and input values from connector steps.
//!
//! These functions are used by all connector implementations to extract
//! typed values from the `ConnectorStep` config and input JSON objects.

use crate::connectors::ConnectorStep;
use execution_ledger_core::ExecutionLedgerError;
use reqwest::Method;
use serde_json::{Value, json};

/// Read `subscriptionId` from step config, falling back to `AZURE_SUBSCRIPTION_ID` env var.
pub(crate) fn subscription_id(step: &ConnectorStep) -> Result<String, ExecutionLedgerError> {
    step_config_string(step, "subscriptionId")
        .or_else(|| std::env::var("AZURE_SUBSCRIPTION_ID").ok())
        .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
            name: "connector.subscriptionId".to_string(),
        })
}

/// Read `resourceGroup` from step config, falling back to `AZURE_RESOURCE_GROUP` env var.
pub(crate) fn resource_group(step: &ConnectorStep) -> Result<String, ExecutionLedgerError> {
    step_config_string(step, "resourceGroup")
        .or_else(|| std::env::var("AZURE_RESOURCE_GROUP").ok())
        .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
            name: "connector.resourceGroup".to_string(),
        })
}

/// Read a string value from step config, falling back to step input.
pub(crate) fn step_config_string(step: &ConnectorStep, key: &str) -> Option<String> {
    step.config
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

/// Read a u64 value from step config.
pub(crate) fn step_config_u64(step: &ConnectorStep, key: &str) -> Option<u64> {
    step.config.get(key).and_then(Value::as_u64)
}

/// Read a JSON object from step config.
pub(crate) fn step_config_object<'a>(
    step: &'a ConnectorStep,
    key: &str,
) -> Option<&'a serde_json::Map<String, Value>> {
    step.config.get(key).and_then(Value::as_object)
}

/// Read a string value from step input.
pub(crate) fn input_string(step: &ConnectorStep, key: &str) -> Option<String> {
    step.input
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

/// Read a u64 value from step input.
pub(crate) fn input_u64(step: &ConnectorStep, key: &str) -> Option<u64> {
    step.input.get(key).and_then(Value::as_u64)
}

/// Parse an HTTP method from an optional string value.
pub(crate) fn method_from(raw: Option<String>) -> Option<Method> {
    raw.and_then(|value| Method::from_bytes(value.as_bytes()).ok())
}

/// Extract the request body from step input.
///
/// Prefers `input.body` if present, otherwise uses the entire input object.
/// Returns `json!({})` if input is null.
pub(crate) fn request_body(step: &ConnectorStep) -> Value {
    if let Some(body) = step.input.get("body") {
        return body.clone();
    }
    if matches!(step.input, Value::Null) {
        return json!({});
    }
    step.input.clone()
}

/// Parse a string as JSON, falling back to a plain string value.
pub(crate) fn parse_json_or_text(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_step(config: Value, input: Value) -> ConnectorStep {
        ConnectorStep {
            id: "test".to_string(),
            connector: crate::connectors::ConnectorKind::Rest,
            action: None,
            external_id: None,
            input,
            config,
            input_schema: None,
            output_schema: None,
        }
    }

    #[test]
    fn subscription_id_from_config() -> Result<(), ExecutionLedgerError> {
        let step = test_step(json!({"subscriptionId": "sub-123"}), json!({}));
        assert_eq!(subscription_id(&step)?, "sub-123");
        Ok(())
    }

    #[test]
    fn subscription_id_missing_is_error() {
        let step = test_step(json!({}), json!({}));
        // Only errors if env var is also unset — skip in CI
        if std::env::var("AZURE_SUBSCRIPTION_ID").is_err() {
            assert!(subscription_id(&step).is_err());
        }
    }

    #[test]
    fn resource_group_from_config() -> Result<(), ExecutionLedgerError> {
        let step = test_step(json!({"resourceGroup": "rg-west"}), json!({}));
        assert_eq!(resource_group(&step)?, "rg-west");
        Ok(())
    }

    #[test]
    fn step_config_string_reads_key() {
        let step = test_step(json!({"serverName": "prod-db"}), json!({}));
        assert_eq!(
            step_config_string(&step, "serverName").as_deref(),
            Some("prod-db")
        );
    }

    #[test]
    fn step_config_string_missing_is_none() {
        let step = test_step(json!({}), json!({}));
        assert!(step_config_string(&step, "nope").is_none());
    }

    #[test]
    fn step_config_object_reads_nested() {
        let step = test_step(
            json!({"targetSku": {"name": "S1", "tier": "Standard"}}),
            json!({}),
        );
        let obj = step_config_object(&step, "targetSku");
        assert_eq!(
            obj.and_then(|o| o.get("name")).and_then(Value::as_str),
            Some("S1")
        );
    }

    #[test]
    fn input_string_falls_back_to_input() {
        let step = test_step(json!({}), json!({"vmName": "web-01"}));
        // step_config_string won't find it in config, but callers
        // use input_string as a fallback
        assert_eq!(input_string(&step, "vmName").as_deref(), Some("web-01"));
    }

    #[test]
    fn request_body_returns_input_by_default() {
        let step = test_step(json!({}), json!({"key": "value"}));
        assert_eq!(request_body(&step), json!({"key": "value"}));
    }

    #[test]
    fn request_body_extracts_body_field() {
        let step = test_step(
            json!({}),
            json!({"body": {"payload": true}, "headers": {"X-Id": "1"}}),
        );
        assert_eq!(request_body(&step), json!({"payload": true}));
    }

    #[test]
    fn request_body_null_input_returns_empty_object() {
        let step = test_step(json!({}), Value::Null);
        assert_eq!(request_body(&step), json!({}));
    }
}
