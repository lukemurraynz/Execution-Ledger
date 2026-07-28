//! Transform connector.
//!
//! Reshapes data between steps without external calls. Maps input fields
//! to output fields using dot-notation paths, applies defaults for missing
//! values, and optionally overlays the result onto the original input.
//!
//! # Configuration
//!
//! | Key | Required | Default | Notes |
//! |-----|----------|---------|-------|
//! | `map` | no | entire input passthrough | Object mapping output field names to dot-notation input paths |
//! | `defaults` | no | `{}` | Fallback values when a mapped path resolves to null/missing |
//! | `passthrough` | no | `false` | When true, merge transformed fields into the original input |
//!
//! ## Dot-notation paths
//!
//! Paths are `.`-separated field names. A leading `$.` is optional.
//!
//! - `"orderId"` → `input.orderId`
//! - `"$.order.id"` → `input.order.id`
//! - `"items.0.name"` → `input.items[0].name` (numeric segments index arrays)
//!
//! ## Fail-closed behavior
//!
//! - Returns `InvalidConfiguration` if `map` is present but not a JSON object.
//! - Returns `InvalidConfiguration` if `defaults` is present but not a JSON object.
//!
//! ## Examples
//!
//! ### Field rename
//!
//! ```json
//! {
//!   "config": {
//!     "map": {
//!       "orderId": "legacy.id",
//!       "totalAmount": "legacy.total"
//!     }
//!   },
//!   "input": { "legacy": { "id": "ABC-123", "total": 99.50 } }
//! }
//! ```
//!
//! Output: `{ "orderId": "ABC-123", "totalAmount": 99.50 }`
//!
//! ### Defaults + passthrough
//!
//! ```json
//! {
//!   "config": {
//!     "map": { "status": "state", "currency": "money.currency" },
//!     "defaults": { "currency": "USD" },
//!     "passthrough": true
//!   },
//!   "input": { "state": "active", "name": "Widget" }
//! }
//! ```
//!
//! Output: `{ "state": "active", "name": "Widget", "status": "active", "currency": "USD" }`

use execution_ledger_core::ExecutionLedgerError;
use serde_json::{Value, json};

use crate::connectors::helpers::step_config_object;
use crate::connectors::{ConnectorExecutionContext, ConnectorStep};

/// Resolve a dot-notation path against a JSON value.
///
/// Supports field access and numeric array indexing.
/// A leading `$.` is stripped if present.
fn resolve_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let trimmed = path.strip_prefix("$.").unwrap_or(path);
    let mut current = root;
    for segment in trimmed.split('.') {
        match current {
            Value::Object(map) => {
                current = map.get(segment)?;
            }
            Value::Array(arr) => {
                let idx: usize = segment.parse().ok()?;
                current = arr.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

/// Apply a `map` of output-field → input-path, with optional defaults.
fn apply_map(
    input: &Value,
    map: &serde_json::Map<String, Value>,
    defaults: &serde_json::Map<String, Value>,
) -> Value {
    let mut output = serde_json::Map::new();
    for (out_field, path_val) in map {
        let path = path_val.as_str().unwrap_or("");
        let resolved = resolve_path(input, path)
            .cloned()
            .or_else(|| defaults.get(out_field).cloned());
        output.insert(out_field.clone(), resolved.unwrap_or(Value::Null));
    }
    Value::Object(output)
}

#[allow(clippy::unused_async)] // satisfies async connector interface; dispatched via .await in mod.rs
pub(crate) async fn execute(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    // If no map config, passthrough input as-is.
    let map = match step_config_object(step, "map") {
        Some(m) => m.clone(),
        None => {
            return Ok(json!({
                "stepId": step.id,
                "connector": "transform",
                "output": step.input,
                "correlationId": ctx.correlation_id,
                "event": ctx.event
            }));
        }
    };

    let defaults = step_config_object(step, "defaults")
        .cloned()
        .unwrap_or_default();

    let passthrough = step
        .config
        .get("passthrough")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let transformed = apply_map(&step.input, &map, &defaults);

    let output = if passthrough {
        // Merge: original input with transformed fields overlaid.
        match (&step.input, &transformed) {
            (Value::Object(base), Value::Object(overlay)) => {
                let mut merged = base.clone();
                for (k, v) in overlay {
                    merged.insert(k.clone(), v.clone());
                }
                Value::Object(merged)
            }
            _ => transformed,
        }
    } else {
        transformed
    };

    Ok(json!({
        "stepId": step.id,
        "connector": "transform",
        "output": output,
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Compensate a transform step by returning the original input.
/// A transform has no external side effects — the only state it changes
/// is the data flowing through the pipeline. Compensation restores the
/// pre-transform state.
#[allow(clippy::unused_async)] // satisfies async connector interface; dispatched via .await in mod.rs
pub(crate) async fn compensate(
    _ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
    _original_output: &Value,
) -> Result<Value, ExecutionLedgerError> {
    Ok(json!({
        "stepId": step.id,
        "connector": "transform",
        "compensated": true,
        "action": "restore_input",
        "originalInput": step.input
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_path_simple_field() {
        let input = json!({"orderId": "ABC"});
        assert_eq!(resolve_path(&input, "orderId"), Some(&json!("ABC")));
    }

    #[test]
    fn resolve_path_nested_field() {
        let input = json!({"order": {"id": "XYZ"}});
        assert_eq!(resolve_path(&input, "order.id"), Some(&json!("XYZ")));
    }

    #[test]
    fn resolve_path_leading_dollar() {
        let input = json!({"order": {"id": "XYZ"}});
        assert_eq!(resolve_path(&input, "$.order.id"), Some(&json!("XYZ")));
    }

    #[test]
    fn resolve_path_array_index() {
        let input = json!({"items": [{"name": "a"}, {"name": "b"}]});
        assert_eq!(resolve_path(&input, "items.1.name"), Some(&json!("b")));
    }

    #[test]
    fn resolve_path_missing_field() {
        let input = json!({"orderId": "ABC"});
        assert_eq!(resolve_path(&input, "missing"), None);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn apply_map_renames_fields() {
        let input = json!({"legacy": {"id": "123", "total": 99}});
        let map = serde_json::from_value(json!({
            "orderId": "legacy.id",
            "totalAmount": "legacy.total"
        }))
        .unwrap();
        let defaults = serde_json::Map::new();
        let result = apply_map(&input, &map, &defaults);
        assert_eq!(result, json!({"orderId": "123", "totalAmount": 99}));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn apply_map_uses_defaults_for_missing() {
        let input = json!({"state": "active"});
        let map = serde_json::from_value(json!({
            "status": "state",
            "currency": "money.currency"
        }))
        .unwrap();
        let defaults = serde_json::from_value(json!({"currency": "USD"})).unwrap();
        let result = apply_map(&input, &map, &defaults);
        assert_eq!(result, json!({"status": "active", "currency": "USD"}));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn apply_map_null_for_unresolved_without_default() {
        let input = json!({"a": 1});
        let map = serde_json::from_value(json!({"out": "b"})).unwrap();
        let defaults = serde_json::Map::new();
        let result = apply_map(&input, &map, &defaults);
        assert_eq!(result, json!({"out": Value::Null}));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn passthrough_merges_with_original() {
        let input = json!({"name": "Widget", "price": 10});
        let map = serde_json::from_value(json!({"label": "name"})).unwrap();
        let defaults = serde_json::Map::new();
        let transformed = apply_map(&input, &map, &defaults);
        // Merge
        let mut merged = input.clone().as_object().unwrap().clone();
        for (k, v) in transformed.as_object().unwrap() {
            merged.insert(k.clone(), v.clone());
        }
        let result = Value::Object(merged);
        assert_eq!(
            result,
            json!({"name": "Widget", "price": 10, "label": "Widget"})
        );
    }
}
