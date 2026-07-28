//! Connector-step execution layer.
//!
//! Each connector is implemented in its own module. The `execute_step`
//! function dispatches to the correct connector based on `ConnectorKind`.
//!
//! # Adding a new connector
//!
//! 1. Add a variant to `ConnectorKind` (with `#[serde(rename_all = "snake_case")]`).
//! 2. Create a new file `connectors/<name>.rs` with a public `execute` function:
//!    ```rust,ignore
//!    pub(crate) async fn execute(
//!        ctx: &ConnectorExecutionContext<'_>,
//!        step: &ConnectorStep,
//!    ) -> Result<Value, ExecutionLedgerError> { ... }
//!    ```
//! 3. Add a `compensate` function following the save-then-restore pattern (see below).
//! 4. Add a match arm in `execute_step` and `compensate_step` below.
//! 5. Add a doc comment to the module explaining config, inputs, and fail-closed behavior.
//!
//! # Compensation — save-then-restore pattern
//!
//! Every connector must implement a `compensate()` function. The default is a
//! no-op (read-only connectors and most side-effecting connectors that cannot
//! be safely undone). For connectors that CAN be undone, follow this pattern:
//!
//! ## The pattern
//!
//! 1. **Save state before mutate.** Include a read step (e.g. `vm-get`,
//!    `sql-database-get`) before any resize/mutate step in the job definition.
//!    The read step captures the current state in its output.
//!
//! 2. **The worker propagates state between steps.** After each step completes,
//!    its output becomes `ctx.upstream_output` for the next step. The mutate
//!    step reads this and stores it in its own output as `stateBlob` so
//!    compensation can find it later.
//!
//! 3. **Compensate reads `stateBlob` from persisted output.** When the worker
//!    processes a `target.compensate` intent, it queries completed steps in
//!    reverse order and calls `compensate_step()` with the original step
//!    definition and output. The `compensate()` function reads `stateBlob`
//!    from `original_output` and calls the reverse action.
//!
//! ## Example: VMSS resize with automatic rollback
//!
//! ```json
//! { "steps": [
//!   { "id": "capture", "connector": "azure_compute", "action": "vmss-get", "config": { "vmssName": "workers" } },
//!   { "id": "scale-up", "connector": "azure_compute", "action": "vmss-resize", "config": { "vmssName": "workers", "targetSku": {"name":"D4s_v5"} } },
//!   { "id": "validate", "connector": "rest", "config": { "url": "https://health/" } }
//! ]}
//! ```
//!
//! If `validate` fails:
//! 1. Worker detects `target.compensate`, queries completed steps: `[validate, scale-up, capture]`
//! 2. `validate` compensate → no-op (read-only GET)
//! 3. `scale-up` compensate → reads `stateBlob` from its output (set by the worker from `capture`'s output), calls `vmss_resize` with the original SKU
//! 4. `capture` compensate → no-op (read-only get)
//! 5. Run marked `compensated`
//!
//! ## Implementing `compensate()` for a new connector
//!
//! ```rust,ignore
//! pub(crate) async fn compensate(
//!     ctx: &ConnectorExecutionContext<'_>,
//!     step: &ConnectorStep,
//!     original_output: &Value,
//! ) -> Result<Value, ExecutionLedgerError> {
//!     let action = original_output.get("action").and_then(Value::as_str).unwrap_or("");
//!     match action {
//!         // Read-only actions → no-op
//!         "my-read-action" => Ok(json!({"compensated": true, "action": "noop", "reason": "read-only"})),
//!         // Mutate actions → read stateBlob and reverse
//!         "my-mutate-action" => {
//!             let Some(state_blob) = original_output.get("stateBlob") else {
//!                 return Ok(json!({"compensated": true, "action": "noop", "reason": "no stateBlob"}));
//!             };
//!             // Build a restore step and call the reverse action
//!             let restore_step = ConnectorStep { /* ... with restore params from stateBlob */ };
//!             my_restore_action(ctx, &restore_step).await?;
//!             Ok(json!({"compensated": true, "action": "restore"}))
//!         }
//!         _ => Ok(json!({"compensated": true, "action": "noop", "reason": "unknown action"})),
//!     }
//! }
//! ```
//!
//! # Module overview
//!
//! | Module | Connector | What it does |
//! |--------|-----------|-------------|
//! | `platform_health` | `platform_health` | Validates platform readiness |
//! | `rest` | `rest`, `erp` | HTTP requests to external APIs |
//! | `ms_graph_notify` | `ms_graph_notify` | Sends Microsoft Graph Teams/Outlook notifications |
//! | `queue` | `queue` | Sends messages to queue systems |
//! | `azure_blob` | `azure_blob` | Reads or writes Azure Blob Storage |
//! | `sftp` | `sftp` | Transfers files over SFTP |
//! | `azure_compute` | `azure_compute` | Resizes VM/VMSS/App Service Plan via ARM API |
//! | `azure_sql` | `azure_sql` | Resizes SQL Database and Elastic Pool via ARM API |
//! | `transform` | `transform` | Reshapes data between steps |
//! | `pgp` | `pgp` | Encrypts/signs or decrypts/verifies a payload with OpenPGP |
//! | `blob_archive` | `blob_archive` | Zips/unzips content between blob containers |
//! | `http_download` | `http_download` | Downloads a URL's bytes into the pipeline as base64, or PUTs upstream bytes to a URL (`upload` action) |

pub mod contracts;
pub mod helpers;

mod arm;
mod azure_compute;
mod azure_sql;
mod blob;
mod blob_archive;
mod http_download;
mod ms_graph_notify;
mod pgp;
mod platform_health;
mod queue;
mod rest;
mod schema;
mod sftp;
mod transform;

use execution_ledger_core::ExecutionLedgerError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Identifies which connector implementation to use for a step.
///
/// Each variant maps to a module in this directory. The serde name
/// (e.g. `platform_health`) is the value used in job step definitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorKind {
    PlatformHealth,
    Rest,
    Queue,
    /// Accepted as both `"azure_blob"` (canonical) and `"blob"` (alias) in
    /// step definitions so operator payloads can use either form.
    #[serde(alias = "blob")]
    AzureBlob,
    MsGraphNotify,
    Sftp,
    Erp,
    AzureCompute,
    AzureSql,
    Transform,
    Pgp,
    BlobArchive,
    HttpDownload,
}

/// A single step within a job run, defining which connector to execute
/// and what configuration/input to pass to it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorStep {
    /// Unique step identifier within the run.
    pub id: String,
    /// Which connector implementation to use.
    pub connector: ConnectorKind,
    /// Optional action modifier (connector-specific).
    #[serde(default)]
    pub action: Option<String>,
    /// External identifier for the step (e.g. source file path, entity ID).
    #[serde(default)]
    pub external_id: Option<String>,
    /// Input data passed to the connector. Also accepts `data` as an alias.
    #[serde(default, alias = "data")]
    pub input: Value,
    /// Connector-specific configuration. Values can come from App Configuration
    /// or environment variables at runtime.
    #[serde(default)]
    pub config: Value,
    /// Optional JSON Schema to validate step input before execution.
    /// When set, the input is validated against this schema and the step
    /// fails before reaching the connector if validation fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    /// Optional JSON Schema to validate step output after execution.
    /// When set, the connector output is validated against this schema and
    /// the step fails with a schema error if validation fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

/// Context passed to every connector execution, carrying run-level
/// metadata for traceability, correlation, and step-to-step data flow.
#[derive(Debug, Clone)]
pub struct ConnectorExecutionContext<'a> {
    pub run_id: Uuid,
    pub correlation_id: &'a str,
    pub job_id: &'a str,
    pub event: &'a str,
    pub environment: &'a str,
    /// Output from the previous step in the run, if any.
    /// Connectors that depend on upstream state (e.g. resize-after-save)
    /// read this to capture the previous step's result into their own output
    /// so compensation can restore it later.
    pub upstream_output: Option<&'a Value>,
}

/// Execute a connector step by dispatching to the appropriate connector module.
///
/// Returns the connector output as a JSON `Value`, or an `ExecutionLedgerError`
/// if the step configuration is invalid or the connector operation fails.
pub async fn execute_step(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    if step.id.trim().is_empty() {
        return Err(ExecutionLedgerError::InvalidConfiguration {
            name: "step.id".to_string(),
        });
    }

    // Validate input against the step's input schema, if one is defined.
    // This fails the step before any connector code runs — the connector
    // never sees a payload that does not match the declared contract.
    if let Some(ref input_schema) = step.input_schema {
        schema::validate(&step.id, "input", input_schema, &step.input)?;
    }

    let output = match step.connector {
        ConnectorKind::PlatformHealth => platform_health::execute(ctx, step).await,
        ConnectorKind::Rest => rest::execute("rest", ctx, step).await,
        ConnectorKind::Erp => rest::execute("erp", ctx, step).await,
        ConnectorKind::MsGraphNotify => ms_graph_notify::execute(ctx, step).await,
        ConnectorKind::Queue => match step.action.as_deref() {
            None | Some("send") => queue::execute(ctx, step).await,
            Some("receive") => queue::receive(ctx, step).await,
            Some(action) => Err(ExecutionLedgerError::InvalidConfiguration {
                name: format!("connector.queue.action ('{action}')"),
            }),
        },
        ConnectorKind::AzureBlob => blob::execute(ctx, step).await,
        ConnectorKind::Sftp => match step.action.as_deref() {
            None | Some("upload") => sftp::execute(ctx, step).await,
            Some("download") => sftp::download(ctx, step).await,
            Some(action) => Err(ExecutionLedgerError::InvalidConfiguration {
                name: format!("connector.sftp.action ('{action}')"),
            }),
        },
        ConnectorKind::AzureCompute => match step.action.as_deref() {
            Some("vm-get" | "vm-save-state") => azure_compute::vm_get(ctx, step).await,
            Some("vmss-get") => azure_compute::vmss_get(ctx, step).await,
            Some("vmss-resize") => azure_compute::vmss_resize(ctx, step).await,
            Some("appservice-get") => azure_compute::appservice_get(ctx, step).await,
            Some("appservice-resize") => azure_compute::appservice_resize(ctx, step).await,
            None => Err(ExecutionLedgerError::InvalidConfiguration {
                name: "connector.azure_compute.action".to_string(),
            }),
            Some(action) => Err(ExecutionLedgerError::InvalidConfiguration {
                name: format!("connector.azure_compute.action ('{action}')"),
            }),
        },
        ConnectorKind::AzureSql => match step.action.as_deref() {
            Some("sql-database-get") => azure_sql::sql_database_get(ctx, step).await,
            Some("sql-database-resize") => azure_sql::sql_database_resize(ctx, step).await,
            Some("sql-elastic-pool-get") => azure_sql::sql_elastic_pool_get(ctx, step).await,
            Some("sql-elastic-pool-resize") => azure_sql::sql_elastic_pool_resize(ctx, step).await,
            None => Err(ExecutionLedgerError::InvalidConfiguration {
                name: "connector.azure_sql.action".to_string(),
            }),
            Some(action) => Err(ExecutionLedgerError::InvalidConfiguration {
                name: format!("connector.azure_sql.action ('{action}')"),
            }),
        },
        ConnectorKind::Transform => transform::execute(ctx, step).await,
        ConnectorKind::Pgp => match step.action.as_deref() {
            Some("encrypt") => pgp::encrypt(ctx, step).await,
            Some("decrypt") => pgp::decrypt(ctx, step).await,
            action => Err(ExecutionLedgerError::InvalidConfiguration {
                name: format!("connector.pgp.action ({action:?})"),
            }),
        },
        ConnectorKind::BlobArchive => match step.action.as_deref() {
            Some("unzip") => blob_archive::unzip(ctx, step).await,
            Some("zip") => blob_archive::zip(ctx, step).await,
            action => Err(ExecutionLedgerError::InvalidConfiguration {
                name: format!("connector.blob_archive.action ({action:?})"),
            }),
        },
        ConnectorKind::HttpDownload => match step.action.as_deref() {
            None | Some("download") => http_download::execute(ctx, step).await,
            Some("upload") => http_download::upload(ctx, step).await,
            Some(action) => Err(ExecutionLedgerError::InvalidConfiguration {
                name: format!("connector.http_download.action ('{action}')"),
            }),
        },
    }?;

    // Validate output against the step's output schema, if one is defined.
    // This fails the step after connector execution but before the caller
    // sees the result — the run is marked failed rather than succeeding
    // with a malformed payload.
    if let Some(ref output_schema) = step.output_schema {
        schema::validate(&step.id, "output", output_schema, &output)?;
    }

    Ok(output)
}

/// Compensate a previously-executed connector step by dispatching to the
/// appropriate connector module's `compensate` function.
///
/// Compensation is the reverse of execution: given the original step
/// definition and the output it produced, the connector undoes the
/// side effect. Default behavior is a no-op that logs the compensation
/// event — connectors with reversible effects (transform, REST with
/// reverse endpoints) override this.
///
/// Returns the compensation result as a JSON `Value`, or an
/// `ExecutionLedgerError` if compensation fails.
pub async fn compensate_step(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
    original_output: &Value,
) -> Result<Value, ExecutionLedgerError> {
    if step.id.trim().is_empty() {
        return Err(ExecutionLedgerError::InvalidConfiguration {
            name: "step.id".to_string(),
        });
    }

    match step.connector {
        ConnectorKind::PlatformHealth => {
            platform_health::compensate(ctx, step, original_output).await
        }
        ConnectorKind::Rest => rest::compensate("rest", ctx, step, original_output).await,
        ConnectorKind::Erp => rest::compensate("erp", ctx, step, original_output).await,
        ConnectorKind::MsGraphNotify => {
            ms_graph_notify::compensate(ctx, step, original_output).await
        }
        ConnectorKind::Queue => queue::compensate(ctx, step, original_output).await,
        ConnectorKind::AzureBlob => blob::compensate(ctx, step, original_output).await,
        ConnectorKind::Sftp => sftp::compensate(ctx, step, original_output).await,
        ConnectorKind::AzureCompute => azure_compute::compensate(ctx, step, original_output).await,
        ConnectorKind::AzureSql => azure_sql::compensate(ctx, step, original_output).await,
        ConnectorKind::Transform => transform::compensate(ctx, step, original_output).await,
        ConnectorKind::Pgp => pgp::compensate(ctx, step, original_output).await,
        ConnectorKind::BlobArchive => blob_archive::compensate(ctx, step, original_output).await,
        ConnectorKind::HttpDownload => http_download::compensate(ctx, step, original_output).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
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

    #[tokio::test]
    async fn input_schema_rejects_invalid_payload_before_connector() {
        let ctx = test_ctx();
        let step = ConnectorStep {
            id: "validate-input".to_string(),
            connector: ConnectorKind::PlatformHealth,
            action: None,
            external_id: None,
            input: json!({"badField": 999}),
            config: json!({}),
            input_schema: Some(json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            })),
            output_schema: None,
        };
        let result = execute_step(&ctx, &step).await;
        let err = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            err.contains("validate-input"),
            "error should name the step: {err}"
        );
        assert!(
            err.contains("input"),
            "error should mention input direction: {err}"
        );
    }

    #[tokio::test]
    async fn valid_input_schema_passes_to_connector() {
        let ctx = test_ctx();
        let step = ConnectorStep {
            id: "valid-input".to_string(),
            connector: ConnectorKind::PlatformHealth,
            action: None,
            external_id: None,
            input: json!({"name": "test"}),
            config: json!({}),
            input_schema: Some(json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            })),
            output_schema: None,
        };
        let result = execute_step(&ctx, &step).await;
        assert!(
            result.is_ok(),
            "valid input should pass schema and reach connector: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn empty_step_id_is_rejected() {
        let ctx = test_ctx();
        let step = ConnectorStep {
            id: "   ".to_string(),
            connector: ConnectorKind::PlatformHealth,
            action: None,
            external_id: None,
            input: json!({}),
            config: json!({}),
            input_schema: None,
            output_schema: None,
        };
        let result = execute_step(&ctx, &step).await;
        let err = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(err.contains("step.id"));
    }
}
