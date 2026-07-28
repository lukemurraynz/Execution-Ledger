//! Schema validation pipeline — validates step input and output against
//! JSON Schema definitions before and after connector execution.
//!
//! Validation runs in `execute_step()` at the connector dispatch boundary,
//! so every connector gets schema enforcement for free.
//!
//! # Schema compilation
//!
//! Schemas are compiled from their `serde_json::Value` representation on
//! each validation call. For production workloads with hot paths and large
//! schemas, add a compiled-schema cache keyed by `(step.id, direction)`.
//!
//! # Error output
//!
//! Validation errors include the JSON path to the failing value and the
//! schema constraint that was violated, so operators can pinpoint the
//! exact field without manually comparing payloads.

use execution_ledger_core::ExecutionLedgerError;
use serde_json::Value;

/// Validate `instance` against the JSON Schema in `schema_value`.
///
/// Returns `Ok(())` if the instance is valid, or a
/// `SchemaValidation` error with human-readable diagnostics.
pub(crate) fn validate(
    step_id: &str,
    direction: &'static str,
    schema_value: &Value,
    instance: &Value,
) -> Result<(), ExecutionLedgerError> {
    let compiled = jsonschema::validator_for(schema_value).map_err(|err| {
        ExecutionLedgerError::InvalidConfiguration {
            name: format!("{step_id}.{direction}Schema: {err}"),
        }
    })?;

    let errors: Vec<String> = compiled
        .iter_errors(instance)
        .map(|err| format!("{}: {err}", err.instance_path()))
        .collect();

    if errors.is_empty() {
        Ok(())
    } else {
        Err(ExecutionLedgerError::SchemaValidation {
            step_id: step_id.to_string(),
            direction,
            errors: errors.join("; "),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_instance_passes() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "age": { "type": "integer" }
            },
            "required": ["name"]
        });
        let instance = json!({"name": "Widget", "age": 42});
        assert!(validate("test", "input", &schema, &instance).is_ok());
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn missing_required_field_fails_with_path() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"]
        });
        let instance = json!({});
        let result = validate("order.transform", "input", &schema, &instance);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("order.transform"));
        assert!(err.contains("input"));
        assert!(err.contains("name"));
    }

    #[test]
    fn wrong_type_fails() {
        let schema = json!({"type": "integer"});
        let instance = json!("not-a-number");
        assert!(validate("step1", "output", &schema, &instance).is_err());
    }

    #[test]
    fn invalid_schema_reports_configuration_error() {
        // Use a schema with a self-referencing $ref loop that cannot
        // resolve — this fails at compile time rather than at validation.
        let schema = json!({"$ref": "#/nonexistent/path"});
        let instance = json!(42);
        let result = validate("s", "input", &schema, &instance);
        assert!(result.is_err());
    }

    #[test]
    fn empty_schema_accepts_everything() {
        let schema = json!({});
        let instance = json!({"anything": "goes"});
        assert!(validate("s", "input", &schema, &instance).is_ok());
    }
}
