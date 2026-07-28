//! Blob connector.
//!
//! Writes data to blob storage. Supports two providers:
//!
//! - **Azure Blob Storage**: Uploads to a blob URL using managed identity.
//! - **`filesystem`**: Writes to a local directory for development and testing.
//!
//! ## Payload resolution
//!
//! In priority order:
//! 1. `input.body` — an explicit payload in the step's own static definition. Defaults
//!    `contentType` to `application/json`.
//! 2. Upstream step output shaped like `{ "content": "<base64>", "contentEncoding": "base64" }`
//!    (the shape produced by the `sftp` connector's `download` action) — decoded back to
//!    the original bytes, so a download-then-upload pipeline transfers the real file
//!    content rather than a JSON-wrapped re-encoding of it. Defaults `contentType` to
//!    `application/octet-stream`.
//! 3. The full step `input`, JSON-serialized (default when neither above applies).
//!
//! ## Configuration
//!
//! | Key | Source | Required | Default | Notes |
//! |-----|--------|----------|---------|-------|
//! | `url` | config or input | no | — | Azure Blob URL (triggers Azure path) |
//! | `directory` | config or env | yes (no url) | — | Local root directory |
//! | `container` | config or input | no | `default` | Container or subdirectory name |
//! | `blobName` | config or input | no | `{runId}-{stepId}.json` | Blob filename |
//! | `contentType` | config or input | no | see payload resolution above | Content type header |
//!
//! ## Fail-closed behavior
//!
//! - Returns `InvalidConfiguration` if neither `url` nor `directory` is provided.
//! - Returns `DependencyFailed` if Azure Blob Storage or filesystem operations fail.
//! - Returns `DependencyFailed` if an upstream `contentEncoding: "base64"` payload fails to decode.

use azure_core::http::{RequestContent, Url};
use azure_identity::ManagedIdentityCredential;
use azure_storage_blob::BlobClient;
use base64::Engine;
use execution_ledger_core::ExecutionLedgerError;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;

use crate::connectors::helpers::{input_string, step_config_string};
use crate::connectors::{ConnectorExecutionContext, ConnectorStep};

/// Resolved payload bytes plus the content-type default appropriate for
/// where they came from (see the module-level "Payload resolution" doc).
struct ResolvedPayload {
    bytes: Vec<u8>,
    default_content_type: &'static str,
}

fn resolve_payload(
    step: &ConnectorStep,
    upstream_output: Option<&Value>,
) -> Result<ResolvedPayload, ExecutionLedgerError> {
    if let Some(body) = step.input.get("body") {
        let bytes =
            serde_json::to_vec(body).map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "serde_json",
                source: Box::new(source),
            })?;
        return Ok(ResolvedPayload {
            bytes,
            default_content_type: "application/json",
        });
    }

    if let Some(upstream) = upstream_output {
        let content = upstream.get("content").and_then(Value::as_str);
        let encoding = upstream.get("contentEncoding").and_then(Value::as_str);
        if let (Some(content), Some("base64")) = (content, encoding) {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(content)
                .map_err(|source| ExecutionLedgerError::DependencyFailed {
                    dependency: "base64",
                    source: Box::new(source),
                })?;
            return Ok(ResolvedPayload {
                bytes,
                default_content_type: "application/octet-stream",
            });
        }
    }

    let bytes = serde_json::to_vec(&step.input).map_err(|source| {
        ExecutionLedgerError::DependencyFailed {
            dependency: "serde_json",
            source: Box::new(source),
        }
    })?;
    Ok(ResolvedPayload {
        bytes,
        default_content_type: "application/json",
    })
}

/// Shared managed-identity credential for blob uploads. Created once per process.
///
/// Panics at startup if managed identity is unavailable — fatal platform
/// misconfiguration for MSI-dependent services.
#[allow(clippy::expect_used)]
static BLOB_CREDENTIAL: LazyLock<Arc<ManagedIdentityCredential>> = LazyLock::new(|| {
    ManagedIdentityCredential::new(None)
        .expect("ManagedIdentityCredential::new should not fail with valid options")
});

pub(crate) async fn execute(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let resolved = resolve_payload(step, ctx.upstream_output)?;
    let bytes = resolved.bytes;
    let content_type = step_config_string(step, "contentType")
        .or_else(|| input_string(step, "contentType"))
        .unwrap_or_else(|| resolved.default_content_type.to_string());

    // Azure Blob path: if a URL is provided, upload directly using managed identity.
    if let Some(url) = step_config_string(step, "url").or_else(|| input_string(step, "url")) {
        let endpoint =
            Url::parse(&url).map_err(|source| ExecutionLedgerError::InvalidConfiguration {
                name: format!("connector.blob.url ({source})"),
            })?;
        let client = BlobClient::new(endpoint, Some((*BLOB_CREDENTIAL).clone()), None).map_err(
            |source| ExecutionLedgerError::DependencyFailed {
                dependency: "azure_storage_blob",
                source: Box::new(source),
            },
        )?;
        client
            .upload(RequestContent::from(bytes.clone()), None)
            .await
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "azure_storage_blob",
                source: Box::new(source),
            })?;
        return Ok(json!({
            "stepId": step.id,
            "connector": "azure_blob",
            "provider": "azure-blob",
            "url": url,
            "contentType": content_type,
            "bytes": bytes.len(),
            "correlationId": ctx.correlation_id,
            "event": ctx.event
        }));
    }

    // Filesystem path: write to a local directory.
    let directory = step_config_string(step, "directory")
        .or_else(|| std::env::var("EXECUTION_LEDGER_BLOB_DIR").ok())
        .ok_or(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_blob.directory".to_string(),
        })?;
    let container = step_config_string(step, "container").unwrap_or_else(|| "default".to_string());
    let blob_name = step_config_string(step, "blobName")
        .or_else(|| step.external_id.clone())
        .unwrap_or_else(|| format!("{}-{}.json", ctx.run_id, step.id));
    let path = PathBuf::from(directory).join(&container).join(&blob_name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "filesystem",
            source: Box::new(source),
        })?;
    }
    fs::write(&path, bytes).map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "filesystem",
        source: Box::new(source),
    })?;

    Ok(json!({
        "stepId": step.id,
        "connector": "azure_blob",
        "provider": "filesystem",
        "container": container,
        "blobName": blob_name,
        "path": path.to_string_lossy(),
        "contentType": content_type,
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Compensation is a no-op for blob: deleting uploaded blobs is
/// destructive and should be guided by operator review rather than
/// automatic reversal. A compensating queue message or audit record is
/// the safer pattern.
#[allow(clippy::unused_async)] // satisfies async connector interface; dispatched via .await in mod.rs
pub(crate) async fn compensate(
    _ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
    _original_output: &Value,
) -> Result<Value, ExecutionLedgerError> {
    Ok(json!({
        "stepId": step.id,
        "connector": "azure_blob",
        "compensated": true,
        "action": "noop"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::ConnectorKind;
    use serde_json::json;

    fn test_step(input: Value) -> ConnectorStep {
        ConnectorStep {
            id: "blob-step".to_string(),
            connector: ConnectorKind::AzureBlob,
            action: None,
            external_id: None,
            input,
            config: json!({}),
            input_schema: None,
            output_schema: None,
        }
    }

    #[test]
    fn resolve_payload_prefers_explicit_body() -> Result<(), Box<dyn std::error::Error>> {
        let step = test_step(json!({"body": {"key": "value"}}));
        let resolved = resolve_payload(&step, None)?;
        assert_eq!(resolved.default_content_type, "application/json");
        assert_eq!(
            resolved.bytes,
            serde_json::to_vec(&json!({"key": "value"}))?
        );
        Ok(())
    }

    #[test]
    fn resolve_payload_decodes_base64_upstream_output() -> Result<(), Box<dyn std::error::Error>> {
        let step = test_step(json!({}));
        let original = b"hello from sftp";
        let encoded = base64::engine::general_purpose::STANDARD.encode(original);
        let upstream = json!({"content": encoded, "contentEncoding": "base64"});
        let resolved = resolve_payload(&step, Some(&upstream))?;
        assert_eq!(resolved.bytes, original);
        assert_eq!(resolved.default_content_type, "application/octet-stream");
        Ok(())
    }

    #[test]
    fn resolve_payload_ignores_upstream_without_base64_marker()
    -> Result<(), Box<dyn std::error::Error>> {
        let step = test_step(json!({}));
        let upstream = json!({"content": "some-string", "someOtherField": true});
        let resolved = resolve_payload(&step, Some(&upstream))?;
        // Falls through to serializing step.input, not the upstream content,
        // because contentEncoding is not the literal "base64".
        assert_eq!(resolved.bytes, serde_json::to_vec(&json!({}))?);
        assert_eq!(resolved.default_content_type, "application/json");
        Ok(())
    }

    #[test]
    fn resolve_payload_falls_back_to_full_input_json() -> Result<(), Box<dyn std::error::Error>> {
        let step = test_step(json!({"key": "value"}));
        let resolved = resolve_payload(&step, None)?;
        assert_eq!(
            resolved.bytes,
            serde_json::to_vec(&json!({"key": "value"}))?
        );
        assert_eq!(resolved.default_content_type, "application/json");
        Ok(())
    }

    #[test]
    fn resolve_payload_reports_invalid_base64() {
        let step = test_step(json!({}));
        let upstream = json!({"content": "not valid base64 !!!", "contentEncoding": "base64"});
        let result = resolve_payload(&step, Some(&upstream));
        assert!(matches!(
            result,
            Err(ExecutionLedgerError::DependencyFailed {
                dependency: "base64",
                ..
            })
        ));
    }
}
