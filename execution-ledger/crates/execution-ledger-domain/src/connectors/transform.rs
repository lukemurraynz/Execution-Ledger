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
//! | `parse` | no | — | When `"xml"`, parses `input` (a string) into JSON before `map` runs |
//!
//! ## XML input (`"parse": "xml"`)
//!
//! A generic, schema-less XML→JSON bridge — not an XSD-aware mapper. Feeds
//! the *existing* `map`/`defaults`/`passthrough` engine below rather than
//! adding a second mapping language: once XML is parsed, dot-notation paths
//! address its fields exactly like they address JSON today.
//!
//! Conversion rules: an element's attributes become `@name` keys; repeated
//! sibling elements become an array; text content becomes the element's
//! value directly (or a `#text` key when the element also has attributes or
//! children); **the root element's own tag name is dropped** — its
//! attributes/children become the top-level object, so paths don't need a
//! leading root-tag segment.
//!
//! ```text
//! <ACEExtractv6>
//!   <Invoice id="1"><Total>99.50</Total></Invoice>
//!   <Invoice id="2"><Total>10.00</Total></Invoice>
//! </ACEExtractv6>
//! ```
//! parses to:
//! ```json
//! { "Invoice": [
//!   { "@id": "1", "Total": "99.50" },
//!   { "@id": "2", "Total": "10.00" }
//! ] }
//! ```
//! so `"Invoice.0.Total"` resolves to `"99.50"` via the same dot-notation
//! `map` used for JSON input.
//!
//! `input` must be a JSON string containing the raw XML text when `parse` is
//! set — this connector doesn't read `ctx.upstream_output` (see the
//! `consumes_upstream_output` known-gap note in `connectors/contracts.rs`),
//! so a prior step's decoded XML content must be placed into this step's
//! `input` explicitly.
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
//! - Returns `InvalidConfiguration` if `parse` is set to anything other than `"xml"`.
//! - Returns `InvalidConfiguration` if `parse: "xml"` is set and `input` is not a string.
//! - Returns `InvalidConfiguration` if the XML in `input` fails to parse.
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
use quick_xml::Reader;
use quick_xml::events::Event;
use serde_json::{Value, json};

use crate::connectors::helpers::step_config_object;
use crate::connectors::{ConnectorExecutionContext, ConnectorStep};

/// One open element while parsing: its accumulated attributes/children
/// (`map`), and any text content seen directly inside it (`text`).
struct XmlFrame {
    tag: String,
    map: serde_json::Map<String, Value>,
    text: String,
}

/// Inserts a closed child element's value under `key`. A second arrival for
/// the same key (repeated sibling elements) turns the entry into an array
/// instead of overwriting it.
fn insert_xml_child(map: &mut serde_json::Map<String, Value>, key: String, value: Value) {
    if let Some(existing) = map.get_mut(&key) {
        if let Value::Array(items) = existing {
            items.push(value);
        } else {
            let previous = std::mem::replace(existing, Value::Null);
            *existing = Value::Array(vec![previous, value]);
        }
    } else {
        map.insert(key, value);
    }
}

fn xml_attributes_to_map(start: &quick_xml::events::BytesStart) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    for attr in start.attributes().flatten() {
        let key = format!(
            "@{}",
            String::from_utf8_lossy(attr.key.local_name().as_ref())
        );
        let value = String::from_utf8_lossy(&attr.value).into_owned();
        map.insert(key, Value::String(value));
    }
    map
}

/// Parses `xml` into a generic JSON value — see the module doc's "XML input"
/// section for the exact conversion rules and the root-tag-name-dropped
/// convention.
fn parse_xml_to_json(xml: &str) -> Result<Value, ExecutionLedgerError> {
    let xml_error = |detail: String| ExecutionLedgerError::InvalidConfiguration {
        name: format!("connector.transform.input (invalid xml: {detail})"),
    };

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut stack: Vec<XmlFrame> = Vec::new();
    let mut root: Option<Value> = None;

    loop {
        match reader.read_event() {
            Err(source) => return Err(xml_error(source.to_string())),
            Ok(Event::Eof) => break,
            Ok(Event::Start(start)) => {
                let tag = String::from_utf8_lossy(start.local_name().as_ref()).into_owned();
                stack.push(XmlFrame {
                    tag,
                    map: xml_attributes_to_map(&start),
                    text: String::new(),
                });
            }
            Ok(Event::Empty(start)) => {
                let tag = String::from_utf8_lossy(start.local_name().as_ref()).into_owned();
                let map = xml_attributes_to_map(&start);
                let value = if map.is_empty() {
                    Value::String(String::new())
                } else {
                    Value::Object(map)
                };
                close_xml_element(&mut stack, &mut root, tag, value);
            }
            Ok(Event::Text(text)) => {
                let decoded = String::from_utf8_lossy(text.as_ref()).into_owned();
                if let Some(frame) = stack.last_mut() {
                    frame.text.push_str(decoded.trim());
                }
            }
            Ok(Event::End(_)) => {
                let Some(frame) = stack.pop() else {
                    return Err(xml_error("unmatched closing tag".to_string()));
                };
                let trimmed_text = frame.text.trim().to_string();
                let value = if frame.map.is_empty() {
                    Value::String(trimmed_text)
                } else {
                    let mut map = frame.map;
                    if !trimmed_text.is_empty() {
                        map.insert("#text".to_string(), Value::String(trimmed_text));
                    }
                    Value::Object(map)
                };
                close_xml_element(&mut stack, &mut root, frame.tag, value);
            }
            _ => {}
        }
    }

    root.ok_or_else(|| xml_error("empty document".to_string()))
}

/// Closes an element: merges it into its parent's accumulating map, or — if
/// there is no parent — sets it as the parsed document's root value, per the
/// root-tag-name-dropped convention documented on the module.
fn close_xml_element(stack: &mut [XmlFrame], root: &mut Option<Value>, tag: String, value: Value) {
    if let Some(parent) = stack.last_mut() {
        insert_xml_child(&mut parent.map, tag, value);
    } else {
        *root = Some(value);
    }
}

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

/// Resolves the value `apply_map`/passthrough operate on: `step.input`
/// as-is, or — when `parse: "xml"` is configured — `step.input` parsed from
/// XML text into JSON first. See the module doc's "XML input" section.
fn resolve_source_input(step: &ConnectorStep) -> Result<Value, ExecutionLedgerError> {
    match step.config.get("parse").and_then(Value::as_str) {
        Some("xml") => {
            let xml_text = step.input.as_str().ok_or_else(|| {
                ExecutionLedgerError::InvalidConfiguration {
                    name: "connector.transform.input (must be a string when parse=\"xml\")"
                        .to_string(),
                }
            })?;
            parse_xml_to_json(xml_text)
        }
        Some(other) => Err(ExecutionLedgerError::InvalidConfiguration {
            name: format!("connector.transform.parse ('{other}' is not supported; use \"xml\")"),
        }),
        None => Ok(step.input.clone()),
    }
}

#[allow(clippy::unused_async)] // satisfies async connector interface; dispatched via .await in mod.rs
pub(crate) async fn execute(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let source_input = resolve_source_input(step)?;

    // If no map config, passthrough the (possibly XML-parsed) input as-is.
    let map = match step_config_object(step, "map") {
        Some(m) => m.clone(),
        None => {
            return Ok(json!({
                "stepId": step.id,
                "connector": "transform",
                "output": source_input,
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

    let transformed = apply_map(&source_input, &map, &defaults);

    let output = if passthrough {
        // Merge: original (parsed) input with transformed fields overlaid.
        match (&source_input, &transformed) {
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

    // ── parse_xml_to_json ────────────────────────────────────────────────

    #[test]
    #[allow(clippy::unwrap_used)]
    fn parse_xml_root_tag_name_is_dropped() {
        let result = parse_xml_to_json("<Root><Name>Widget</Name></Root>").unwrap();
        assert_eq!(result, json!({"Name": "Widget"}));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn parse_xml_element_text_becomes_string_value() {
        let result = parse_xml_to_json("<Root><Total>99.50</Total></Root>").unwrap();
        assert_eq!(result, json!({"Total": "99.50"}));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn parse_xml_attributes_become_at_prefixed_keys() {
        let result = parse_xml_to_json(r#"<Root><Invoice id="1" branch="014"/></Root>"#).unwrap();
        assert_eq!(result, json!({"Invoice": {"@id": "1", "@branch": "014"}}));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn parse_xml_repeated_siblings_become_array() {
        let xml = r#"<Root>
            <Invoice id="1"><Total>99.50</Total></Invoice>
            <Invoice id="2"><Total>10.00</Total></Invoice>
        </Root>"#;
        let result = parse_xml_to_json(xml).unwrap();
        assert_eq!(
            result,
            json!({"Invoice": [
                {"@id": "1", "Total": "99.50"},
                {"@id": "2", "Total": "10.00"}
            ]})
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn parse_xml_mixed_content_uses_text_key() {
        let result = parse_xml_to_json(r#"<Root><Note lang="en">hello</Note></Root>"#).unwrap();
        assert_eq!(result, json!({"Note": {"@lang": "en", "#text": "hello"}}));
    }

    #[test]
    fn parse_xml_invalid_xml_is_invalid_configuration() {
        let result = parse_xml_to_json("<Root><Unclosed></Root>");
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            message.contains("invalid xml"),
            "expected xml parse error, got: {message:?}"
        );
    }

    // ── execute() with parse: "xml" — the end-to-end path a job author uses ──

    fn test_ctx() -> ConnectorExecutionContext<'static> {
        ConnectorExecutionContext {
            run_id: uuid::Uuid::new_v4(),
            correlation_id: "test-correlation",
            job_id: "test-job",
            event: "test",
            environment: "test",
            upstream_output: None,
        }
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn execute_parses_xml_then_applies_dot_notation_map() {
        let ctx = test_ctx();
        let step = ConnectorStep {
            id: "xml-transform".to_string(),
            connector: crate::connectors::ConnectorKind::Transform,
            action: None,
            external_id: None,
            input: Value::String(
                r#"<ACEExtractv6><Invoice id="1"><Total>99.50</Total></Invoice></ACEExtractv6>"#
                    .to_string(),
            ),
            config: json!({
                "parse": "xml",
                "map": {"invoiceId": "Invoice.@id", "total": "Invoice.Total"}
            }),
            input_schema: None,
            output_schema: None,
        };
        let result = execute(&ctx, &step).await.unwrap();
        assert_eq!(
            result.get("output"),
            Some(&json!({"invoiceId": "1", "total": "99.50"}))
        );
    }

    #[tokio::test]
    async fn execute_xml_parse_rejects_non_string_input() {
        let ctx = test_ctx();
        let step = ConnectorStep {
            id: "xml-transform".to_string(),
            connector: crate::connectors::ConnectorKind::Transform,
            action: None,
            external_id: None,
            input: json!({"not": "a string"}),
            config: json!({"parse": "xml"}),
            input_schema: None,
            output_schema: None,
        };
        let result = execute(&ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            message.contains("must be a string"),
            "expected string-input error, got: {message:?}"
        );
    }

    #[tokio::test]
    async fn execute_rejects_unsupported_parse_value() {
        let ctx = test_ctx();
        let step = ConnectorStep {
            id: "xml-transform".to_string(),
            connector: crate::connectors::ConnectorKind::Transform,
            action: None,
            external_id: None,
            input: Value::String("irrelevant".to_string()),
            config: json!({"parse": "csv"}),
            input_schema: None,
            output_schema: None,
        };
        let result = execute(&ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            message.contains("not supported"),
            "expected unsupported-parse error, got: {message:?}"
        );
    }
}
