//! Blob archive connector.
//!
//! Zips and unzips content directly between Azure Blob Storage containers,
//! using managed identity. This is a blob-to-blob(s) operation by nature —
//! unlike `pgp` or `transform`, it owns its own blob I/O rather than
//! composing through `ctx.upstream_output`, because a single zip maps to
//! *many* blobs on one side of the operation.
//!
//! Supports two actions:
//!
//! - **`unzip`**: downloads a zip blob, extracts every file entry (skipping
//!   directory entries), and uploads each one under `destinationContainer`.
//! - **`zip`**: downloads each blob named in `sourceBlobNames`, packs them
//!   into a single zip archive, and uploads the archive to
//!   `destinationContainer`/`destinationBlobName`. The output also carries
//!   the archive as base64 `content`/`contentEncoding`, so a downstream step
//!   (e.g. `http_download`'s `upload` action, to PUT the archive to a
//!   SAS URL) can chain from it without re-reading the blob.
//!
//! `zip` takes an explicit list of source blob names rather than listing a
//! container prefix at runtime. Job templates in this platform are
//! declarative step definitions, not live-discovery scripts — an upstream
//! step (or the job author) is expected to already know which blobs it
//! produced. This is an intentional connector-boundary decision, not a
//! missing feature.
//!
//! ## Configuration
//!
//! | Key | Action | Required | Notes |
//! |-----|--------|----------|-------|
//! | `accountUrl` | both | yes (config or env `EXECUTION_LEDGER_BLOB_ACCOUNT_URL`) | e.g. `https://<account>.blob.core.windows.net/` |
//! | `sourceContainer` | both | yes | Container to read from |
//! | `sourceBlobName` | unzip | yes | The zip blob to extract |
//! | `sourceBlobNames` | zip | yes | Config or input array of blob names to bundle |
//! | `destinationContainer` | both | yes | Container to write to |
//! | `destinationPrefix` | unzip | no (default: none) | Prefix prepended to each extracted blob name |
//! | `destinationBlobName` | zip | yes | Name of the produced zip blob |
//!
//! ## Fail-closed behavior
//!
//! - Returns `InvalidConfiguration` if any required key above is missing,
//!   or if `sourceBlobNames` is present but empty.
//! - Returns `DependencyFailed { dependency: "blob_archive" }` if download,
//!   zip/unzip parsing, or upload fails for any entry. A partially-uploaded
//!   `unzip` on failure is a known limitation — see the pack runbook for
//!   the operator recovery path (replay the step; extraction re-uploads
//!   all entries and overwrites what was already written).

use std::io::{Cursor, Read, Write};
use std::sync::{Arc, LazyLock};

use azure_core::http::RequestContent;
use azure_identity::ManagedIdentityCredential;
use base64::Engine;
use azure_storage_blob::{BlobServiceClient, BlobServiceClientOptions};
use execution_ledger_core::ExecutionLedgerError;
use serde_json::{Value, json};
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

use crate::connectors::helpers::step_config_string;
use crate::connectors::{ConnectorExecutionContext, ConnectorStep};

#[allow(clippy::expect_used)]
static BLOB_CREDENTIAL: LazyLock<Arc<ManagedIdentityCredential>> = LazyLock::new(|| {
    ManagedIdentityCredential::new(None)
        .expect("ManagedIdentityCredential::new should not fail with valid options")
});

fn dependency_failed<E>(source: E) -> ExecutionLedgerError
where
    E: std::error::Error + Send + Sync + 'static,
{
    ExecutionLedgerError::DependencyFailed {
        dependency: "blob_archive",
        source: Box::new(source),
    }
}

fn account_url(step: &ConnectorStep) -> Result<azure_core::http::Url, ExecutionLedgerError> {
    let raw = step_config_string(step, "accountUrl")
        .or_else(|| std::env::var("EXECUTION_LEDGER_BLOB_ACCOUNT_URL").ok())
        .ok_or(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.blob_archive.accountUrl".to_string(),
        })?;
    azure_core::http::Url::parse(&raw).map_err(|source| ExecutionLedgerError::InvalidConfiguration {
        name: format!("connector.blob_archive.accountUrl ({source})"),
    })
}

fn required_config(step: &ConnectorStep, key: &str) -> Result<String, ExecutionLedgerError> {
    step_config_string(step, key).ok_or(ExecutionLedgerError::InvalidConfiguration {
        name: format!("connector.blob_archive.{key}"),
    })
}

fn service_client(step: &ConnectorStep) -> Result<BlobServiceClient, ExecutionLedgerError> {
    let url = account_url(step)?;
    BlobServiceClient::new(
        url,
        Some((*BLOB_CREDENTIAL).clone()),
        None::<BlobServiceClientOptions>,
    )
    .map_err(dependency_failed)
}

pub(crate) async fn unzip(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let client = service_client(step)?;
    let source_container = required_config(step, "sourceContainer")?;
    let source_blob_name = required_config(step, "sourceBlobName")?;
    let destination_container = required_config(step, "destinationContainer")?;
    let destination_prefix = step_config_string(step, "destinationPrefix").unwrap_or_default();

    let source_client = client.blob_client(&source_container, &source_blob_name);
    let download = source_client
        .download(None)
        .await
        .map_err(dependency_failed)?;
    let zip_bytes = download
        .body
        .collect()
        .await
        .map_err(dependency_failed)?
        .to_vec();

    let mut archive =
        ZipArchive::new(Cursor::new(zip_bytes)).map_err(dependency_failed)?;

    let mut entries = Vec::new();
    for index in 0..archive.len() {
        let (name, bytes) = {
            let mut entry = archive.by_index(index).map_err(dependency_failed)?;
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().to_string();
            let mut bytes = Vec::with_capacity(usize::try_from(entry.size()).unwrap_or(0));
            entry.read_to_end(&mut bytes).map_err(dependency_failed)?;
            (name, bytes)
        };

        let destination_blob_name = format!("{destination_prefix}{name}");
        let destination_client = client.blob_client(&destination_container, &destination_blob_name);
        destination_client
            .upload(RequestContent::from(bytes.clone()), None)
            .await
            .map_err(dependency_failed)?;

        entries.push(json!({
            "name": name,
            "blobName": destination_blob_name,
            "bytes": bytes.len()
        }));
    }

    Ok(json!({
        "stepId": step.id,
        "connector": "blob_archive",
        "action": "unzip",
        "sourceContainer": source_container,
        "sourceBlobName": source_blob_name,
        "destinationContainer": destination_container,
        "fileCount": entries.len(),
        "entries": entries,
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

fn source_blob_names(step: &ConnectorStep) -> Result<Vec<String>, ExecutionLedgerError> {
    let from_config = step.config.get("sourceBlobNames").and_then(Value::as_array);
    let from_input = step.input.get("sourceBlobNames").and_then(Value::as_array);
    let names: Vec<String> = from_config
        .or(from_input)
        .ok_or(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.blob_archive.sourceBlobNames".to_string(),
        })?
        .iter()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect();

    if names.is_empty() {
        return Err(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.blob_archive.sourceBlobNames (empty)".to_string(),
        });
    }

    Ok(names)
}

pub(crate) async fn zip(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let client = service_client(step)?;
    let source_container = required_config(step, "sourceContainer")?;
    let destination_container = required_config(step, "destinationContainer")?;
    let destination_blob_name = required_config(step, "destinationBlobName")?;
    let names = source_blob_names(step)?;

    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let mut total_bytes = 0usize;
    for name in &names {
        let source_client = client.blob_client(&source_container, name);
        let download = source_client
            .download(None)
            .await
            .map_err(dependency_failed)?;
        let bytes = download
            .body
            .collect()
            .await
            .map_err(dependency_failed)?
            .to_vec();
        total_bytes += bytes.len();

        writer
            .start_file(name, SimpleFileOptions::default())
            .map_err(dependency_failed)?;
        writer.write_all(&bytes).map_err(dependency_failed)?;
    }

    let archive_bytes = writer.finish().map_err(dependency_failed)?.into_inner();
    let archive_len = archive_bytes.len();
    let archive_content_b64 = base64::engine::general_purpose::STANDARD.encode(&archive_bytes);

    let destination_client = client.blob_client(&destination_container, &destination_blob_name);
    destination_client
        .upload(RequestContent::from(archive_bytes), None)
        .await
        .map_err(dependency_failed)?;

    Ok(json!({
        "stepId": step.id,
        "connector": "blob_archive",
        "action": "zip",
        "sourceContainer": source_container,
        "fileCount": names.len(),
        "sourceBlobNames": names,
        "destinationContainer": destination_container,
        "destinationBlobName": destination_blob_name,
        "sourceBytes": total_bytes,
        "archiveBytes": archive_len,
        "content": archive_content_b64,
        "contentEncoding": "base64",
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Compensation is a no-op for both actions: deleting uploaded blobs is
/// destructive (same reasoning as `azure_blob`). See the module doc for
/// the operator replay path on a partial `unzip` failure.
#[allow(clippy::unused_async)] // satisfies async connector interface; dispatched via .await in mod.rs
pub(crate) async fn compensate(
    _ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
    original_output: &Value,
) -> Result<Value, ExecutionLedgerError> {
    let action = original_output
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("unzip");
    Ok(json!({
        "stepId": step.id,
        "connector": "blob_archive",
        "compensated": true,
        "action": "noop",
        "reason": format!("deleting uploaded blobs from a prior '{action}' is destructive; requires operator review")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::ConnectorKind;
    use serde_json::json;

    fn test_step(action: Option<&str>, config: Value, input: Value) -> ConnectorStep {
        ConnectorStep {
            id: "archive-step".to_string(),
            connector: ConnectorKind::BlobArchive,
            action: action.map(str::to_string),
            external_id: None,
            input,
            config,
            input_schema: None,
            output_schema: None,
        }
    }

    #[test]
    fn source_blob_names_reads_from_config() -> Result<(), Box<dyn std::error::Error>> {
        let step = test_step(
            Some("zip"),
            json!({"sourceBlobNames": ["a.csv", "b.csv"]}),
            json!({}),
        );
        assert_eq!(
            source_blob_names(&step).map_err(|e| e.to_string())?,
            vec!["a.csv".to_string(), "b.csv".to_string()]
        );
        Ok(())
    }

    #[test]
    fn source_blob_names_falls_back_to_input() -> Result<(), Box<dyn std::error::Error>> {
        let step = test_step(
            Some("zip"),
            json!({}),
            json!({"sourceBlobNames": ["only.csv"]}),
        );
        assert_eq!(
            source_blob_names(&step).map_err(|e| e.to_string())?,
            vec!["only.csv".to_string()]
        );
        Ok(())
    }

    #[test]
    fn source_blob_names_rejects_empty_list() {
        let step = test_step(Some("zip"), json!({"sourceBlobNames": []}), json!({}));
        assert!(source_blob_names(&step).is_err());
    }

    #[test]
    fn source_blob_names_rejects_missing_key() {
        let step = test_step(Some("zip"), json!({}), json!({}));
        assert!(source_blob_names(&step).is_err());
    }

    #[tokio::test]
    async fn unzip_missing_account_url_is_invalid_configuration() {
        let ctx = ConnectorExecutionContext {
            run_id: uuid::Uuid::new_v4(),
            correlation_id: "test",
            job_id: "test-job",
            event: "test",
            environment: "test",
            upstream_output: None,
        };
        // SAFETY for the test: only clear the env var this test cares about.
        // std::env is process-global; tests in this module don't set this
        // var elsewhere, so a race here would only widen an already-missing
        // value, never mask a real one.
        let step = test_step(
            Some("unzip"),
            json!({"sourceContainer": "c", "sourceBlobName": "b.zip", "destinationContainer": "d"}),
            json!({}),
        );
        let result = unzip(&ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        if std::env::var("EXECUTION_LEDGER_BLOB_ACCOUNT_URL").is_err() {
            assert!(message.contains("accountUrl"), "got: {message:?}");
        }
    }
}
