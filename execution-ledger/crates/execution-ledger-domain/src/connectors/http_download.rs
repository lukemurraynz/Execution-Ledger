//! HTTP download/upload connector.
//!
//! Two actions, both filling the same gap: the `rest`/`erp` connector only
//! sends/receives parsed JSON/text bodies — it cannot carry binary payloads
//! to or from `azure_blob`/`blob_archive`.
//!
//! - **`download`** (default when `action` is omitted): downloads bytes from
//!   an arbitrary URL (e.g. a time-limited SAS URL returned by an upstream
//!   `rest`/`erp` call) and returns them base64-encoded in the step output,
//!   in the same `{ "content": "<base64>", "contentEncoding": "base64" }`
//!   shape the `sftp` connector's `download` action produces. Chain it as
//!   `rest` (get a downloadable URL) → `http_download` → `azure_blob`
//!   (upload) or `blob_archive` (unzip).
//! - **`upload`**: PUTs bytes to an arbitrary URL (e.g. a writable SAS URL).
//!   Sends `x-ms-blob-type: BlockBlob` by default because Azure Blob SAS URL
//!   uploads reject a plain PUT with `MissingRequiredHeader` otherwise;
//!   override or clear it via `blobTypeHeader` if the target isn't Azure Blob.
//!   Chain it as
//!   `blob_archive` (zip) → `http_download` (`upload`).
//!
//! ## Configuration
//!
//! | Key | Action | Source | Required | Default | Notes |
//! |-----|--------|--------|----------|---------|-------|
//! | `url` | both | config, input, or upstream output | yes | — | URL to download from / upload to |
//! | `timeoutMs` | both | config or input | no | `30000` | Clamped to 120s max, matching the `rest` connector |
//! | `blobTypeHeader` | upload | config | no | `BlockBlob` | Value sent as `x-ms-blob-type`; set to an empty string to omit the header entirely |
//!
//! ## Upstream-output fallback
//!
//! `transform` reshapes a step's own static `input`, not the previous
//! step's output — it cannot itself lift a nested field like `response.value`
//! from a preceding `rest`/`erp` call into this step's input. So this
//! connector checks the upstream output directly, in order:
//! 1. A top-level `url` field (e.g. from a custom connector's output).
//! 2. `response.value` — the shape `rest`/`erp` steps produce, where the
//!    external API's own response body sits at `response` and the field of
//!    interest (typically a download URL returned by an upstream API call) is
//!    `value`.
//!
//! This lets `erp` (get a downloadable URL) → `http_download` chain directly
//! without an intermediate step.
//!
//! ## Fail-closed behavior
//!
//! - Returns `InvalidConfiguration` if `url` is missing.
//! - Returns `InvalidConfiguration` if `upload` has no bytes to send (see
//!   the payload resolution order on [`upload`]).
//! - Returns `DependencyFailed` if the request fails, times out, or returns
//!   a non-2xx status.

use std::sync::LazyLock;
use std::time::Duration;

use base64::Engine;
use execution_ledger_core::ExecutionLedgerError;
use serde_json::{Value, json};

use crate::connectors::helpers::{input_string, input_u64, step_config_string, step_config_u64};
use crate::connectors::{ConnectorExecutionContext, ConnectorStep};

/// Matches the `rest` connector's timeout ceiling so operators reason about
/// one bounded-external-call policy across both connectors.
const CONNECTOR_MAX_TIMEOUT_MS: u64 = 120_000;

#[allow(clippy::expect_used)]
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest::Client::builder().build() should not fail with valid config")
});

/// Resolves the target URL, shared by `download` and `upload`. See the
/// module doc's "Upstream-output fallback" section for the search order.
fn resolve_url(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<String, ExecutionLedgerError> {
    step_config_string(step, "url")
        .or_else(|| input_string(step, "url"))
        .or_else(|| {
            let upstream = ctx.upstream_output?;
            upstream
                .get("url")
                .or_else(|| upstream.get("response").and_then(|r| r.get("value")))
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .ok_or(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.http_download.url".to_string(),
        })
}

fn resolve_timeout_ms(step: &ConnectorStep) -> u64 {
    step_config_u64(step, "timeoutMs")
        .or_else(|| input_u64(step, "timeoutMs"))
        .unwrap_or(30_000)
        .min(CONNECTOR_MAX_TIMEOUT_MS)
}

pub(crate) async fn execute(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let url = resolve_url(ctx, step)?;
    let timeout_ms = resolve_timeout_ms(step);

    let response = HTTP_CLIENT
        .get(&url)
        .timeout(Duration::from_millis(timeout_ms))
        .header("x-execution-ledger-correlation-id", ctx.correlation_id)
        .send()
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "reqwest",
            source: Box::new(source),
        })?;

    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "reqwest",
            source: Box::new(source),
        })?;

    if !status.is_success() {
        return Err(ExecutionLedgerError::DependencyFailed {
            dependency: "reqwest",
            source: Box::new(std::io::Error::other(format!(
                "http_download request failed with status {status}"
            ))),
        });
    }

    Ok(json!({
        "stepId": step.id,
        "connector": "http_download",
        "url": url,
        "bytes": bytes.len(),
        "content": base64::engine::general_purpose::STANDARD.encode(&bytes),
        "contentEncoding": "base64",
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Resolves the bytes to PUT for [`upload`].
///
/// In priority order:
/// 1. Upstream step output shaped like `{ "content": "<base64>", "contentEncoding": "base64" }`
///    (the shape `blob_archive`'s `zip` action, `pgp`'s `encrypt` action, or
///    `sftp download` all produce).
/// 2. `input.body`, JSON-serialized — an explicit static payload.
///
/// Unlike `sftp`'s upload action, there is no "serialize the whole input"
/// fallback: this connector exists specifically for binary transfers, and
/// silently sending a JSON blob to a SAS URL because no real payload was
/// found would be a worse failure mode than failing closed.
fn resolve_upload_bytes(
    step: &ConnectorStep,
    upstream_output: Option<&Value>,
) -> Result<Vec<u8>, ExecutionLedgerError> {
    if let Some(upstream) = upstream_output {
        let content = upstream.get("content").and_then(Value::as_str);
        let encoding = upstream.get("contentEncoding").and_then(Value::as_str);
        if let (Some(content), Some("base64")) = (content, encoding) {
            return base64::engine::general_purpose::STANDARD.decode(content).map_err(
                |source| ExecutionLedgerError::DependencyFailed {
                    dependency: "base64",
                    source: Box::new(source),
                },
            );
        }
    }

    if let Some(body) = step.input.get("body") {
        return serde_json::to_vec(body).map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "serde_json",
            source: Box::new(source),
        });
    }

    Err(ExecutionLedgerError::InvalidConfiguration {
        name: "connector.http_download.upload (no base64 content in upstream output or input.body)"
            .to_string(),
    })
}

pub(crate) async fn upload(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let url = resolve_url(ctx, step)?;
    let timeout_ms = resolve_timeout_ms(step);
    let bytes = resolve_upload_bytes(step, ctx.upstream_output)?;
    let blob_type_header = step_config_string(step, "blobTypeHeader").unwrap_or_else(|| "BlockBlob".to_string());

    let mut request = HTTP_CLIENT
        .put(&url)
        .timeout(Duration::from_millis(timeout_ms))
        .header("x-execution-ledger-correlation-id", ctx.correlation_id);
    if !blob_type_header.is_empty() {
        request = request.header("x-ms-blob-type", &blob_type_header);
    }

    let response = request
        .body(bytes.clone())
        .send()
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "reqwest",
            source: Box::new(source),
        })?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(ExecutionLedgerError::DependencyFailed {
            dependency: "reqwest",
            source: Box::new(std::io::Error::other(format!(
                "http_download upload failed with status {status}: {text}"
            ))),
        });
    }

    Ok(json!({
        "stepId": step.id,
        "connector": "http_download",
        "action": "upload",
        "url": url,
        "bytes": bytes.len(),
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Compensation is a no-op for both actions: `download` is read-only.
/// `upload` wrote to an external URL, and deleting/reversing that is
/// destructive and target-specific — requires operator review, same
/// reasoning as `azure_blob`/`sftp upload`.
#[allow(clippy::unused_async)] // satisfies async connector interface; dispatched via .await in mod.rs
pub(crate) async fn compensate(
    _ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
    _original_output: &Value,
) -> Result<Value, ExecutionLedgerError> {
    Ok(json!({
        "stepId": step.id,
        "connector": "http_download",
        "compensated": true,
        "action": "noop",
        "reason": "read-only action"
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
            id: "download-step".to_string(),
            connector: ConnectorKind::HttpDownload,
            action: None,
            external_id: None,
            input: json!({}),
            config,
            input_schema: None,
            output_schema: None,
        }
    }

    #[test]
    fn resolve_upload_bytes_prefers_upstream_base64() -> Result<(), Box<dyn std::error::Error>> {
        let step = test_step(json!({}));
        let upstream = json!({
            "content": base64::engine::general_purpose::STANDARD.encode("zip bytes"),
            "contentEncoding": "base64"
        });
        let resolved = resolve_upload_bytes(&step, Some(&upstream)).map_err(|e| e.to_string())?;
        assert_eq!(resolved, b"zip bytes");
        Ok(())
    }

    #[test]
    fn resolve_upload_bytes_falls_back_to_input_body() -> Result<(), Box<dyn std::error::Error>> {
        let mut step = test_step(json!({}));
        step.input = json!({"body": {"key": "value"}});
        let resolved = resolve_upload_bytes(&step, None).map_err(|e| e.to_string())?;
        assert_eq!(resolved, serde_json::to_vec(&json!({"key": "value"}))?);
        Ok(())
    }

    #[test]
    fn resolve_upload_bytes_errors_with_no_payload_anywhere() {
        let step = test_step(json!({}));
        assert!(resolve_upload_bytes(&step, None).is_err());
    }

    #[tokio::test]
    async fn upload_missing_payload_is_invalid_configuration() {
        let ctx = test_ctx();
        let step = test_step(json!({"url": "http://execution-ledger.invalid.test/upload"}));
        let result = upload(&ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(message.contains("upload"), "got: {message:?}");
    }

    #[tokio::test]
    async fn missing_url_is_invalid_configuration() {
        let ctx = test_ctx();
        let step = test_step(json!({}));
        let result = execute(&ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(message.contains("url"), "got: {message:?}");
    }

    #[tokio::test]
    async fn falls_back_to_upstream_output_url_and_fails_on_the_real_request() {
        // Confirms the upstream fallback is actually consulted (the error
        // becomes a network/DependencyFailed for the bogus host, not the
        // InvalidConfiguration this same step produces with no url at all).
        let upstream = json!({"url": "http://execution-ledger.invalid.test/package.zip"});
        let ctx = ConnectorExecutionContext {
            run_id: Uuid::new_v4(),
            correlation_id: "test-correlation",
            job_id: "test-job",
            event: "test",
            environment: "test",
            upstream_output: Some(&upstream),
        };
        let step = test_step(json!({}));
        let result = execute(&ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            !message.contains("InvalidConfiguration") && !message.is_empty(),
            "expected a dependency/network error, got: {message:?}"
        );
    }

    #[tokio::test]
    async fn falls_back_to_upstream_response_value_shape_from_erp_step() {
        // Confirms the erp/rest connector's { "response": { "value": "<url>" } }
        // output shape is consulted, not just a top-level "url" field.
        let upstream = json!({"response": {"value": "http://execution-ledger.invalid.test/package.zip"}});
        let ctx = ConnectorExecutionContext {
            run_id: Uuid::new_v4(),
            correlation_id: "test-correlation",
            job_id: "test-job",
            event: "test",
            environment: "test",
            upstream_output: Some(&upstream),
        };
        let step = test_step(json!({}));
        let result = execute(&ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            !message.contains("InvalidConfiguration") && !message.is_empty(),
            "expected a dependency/network error, got: {message:?}"
        );
    }

    #[tokio::test]
    async fn compensate_is_noop() -> Result<(), Box<dyn std::error::Error>> {
        let ctx = test_ctx();
        let step = test_step(json!({}));
        let result = compensate(&ctx, &step, &json!({})).await.map_err(|e| e.to_string())?;
        assert_eq!(result.get("compensated").and_then(Value::as_bool), Some(true));
        Ok(())
    }
}
