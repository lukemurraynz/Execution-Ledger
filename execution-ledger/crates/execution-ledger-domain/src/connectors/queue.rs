//! Queue connector.
//!
//! Supports two actions:
//!
//! - **`send`** (default when `action` is omitted): sends/enqueues a message.
//!   This is the original behavior and stays the default so existing job
//!   templates that don't set `action` keep working unchanged.
//! - **`receive`**: receives (and removes) one message from the queue,
//!   returning its content in the step output (`body` field; parsed as JSON
//!   when the message is valid JSON, otherwise as plain text). Returns
//!   `{"received": false}` when the queue is empty — this is a normal
//!   polling outcome, not an error.
//!
//! Supports two providers:
//!
//! - **`servicebus`** (default): Uses Azure Service Bus with managed
//!   identity. Connection strings are explicitly rejected. `receive` uses
//!   peek-lock and completes (removes) the message immediately upon
//!   successful receive within the same step — see "Receive and
//!   compensation" below for why, and what that means for reliability.
//! - **`filesystem` / `local`**: Reads/writes a local directory for
//!   development and testing. `receive` picks the oldest file by
//!   modification time, reads it, and deletes it.
//!
//! ## Receive and compensation
//!
//! Every other connector in this codebase commits its external side effect
//! within a single step execution, with `compensate()` as the sole recovery
//! mechanism for undoing a successful step after a later step fails — there
//! is no cross-step "defer this until the whole job succeeds" protocol. The
//! internal intent worker's own Service Bus consumption loop *does* defer
//! completion until after processing succeeds, but that happens within one
//! atomic unit of work (a single Rust function, one queue message), not
//! across independently-committed connector steps with a database commit
//! between each. `receive` follows the connector-step model instead: it
//! completes the message immediately, and captures the message body in a
//! `stateBlob` (the same save-then-restore pattern `azure_compute` and
//! `azure_sql` use) so that if a later step in the same job fails and the
//! run is compensated, `compensate()` re-publishes the message back onto
//! the queue rather than silently losing it. This means a message is
//! removed from the queue as soon as it's received, not only once the
//! whole job succeeds — know this before using `receive` for payloads
//! where losing at-least-once redelivery semantics matters.
//!
//! ## Configuration
//!
//! | Key | Source | Required | Default | Notes |
//! |-----|--------|----------|---------|-------|
//! | `provider` | config or env | no | `servicebus` | `servicebus` or `filesystem` |
//! | `queueName` | config or env | no | `default` | Queue or directory name |
//! | `namespace` | config or env | yes (servicebus) | — | Service Bus namespace |
//! | `directory` | config or env | yes (filesystem) | — | Root directory for local queues |
//!
//! ## Fail-closed behavior
//!
//! - Returns `InvalidConfiguration` if required config is missing.
//! - Returns `InvalidConfiguration` if `EXECUTION_LEDGER_SERVICEBUS_CONNECTION_STRING`
//!   is set (managed identity only).
//! - Returns `DependencyFailed` if Service Bus or filesystem operations fail.

use execution_ledger_core::ExecutionLedgerError;
use execution_ledger_core::queue::{ServiceBusQueueClient, ServiceBusRawReceiveOutcome};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;

use crate::connectors::helpers::{parse_json_or_text, step_config_string};
use crate::connectors::{ConnectorExecutionContext, ConnectorStep};

/// Resolved Service Bus queue/namespace target shared by send and receive,
/// including the connection-string rejection check.
struct ServiceBusTarget {
    queue_name: String,
    namespace: String,
}

fn resolve_servicebus_target(
    step: &ConnectorStep,
) -> Result<ServiceBusTarget, ExecutionLedgerError> {
    let queue_name = step_config_string(step, "queueName")
        .or_else(|| std::env::var("EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME").ok())
        .ok_or(ExecutionLedgerError::InvalidConfiguration {
            name: "EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME".to_string(),
        })?;
    if step_config_string(step, "connectionString")
        .or_else(|| std::env::var("EXECUTION_LEDGER_SERVICEBUS_CONNECTION_STRING").ok())
        .is_some_and(|value| !value.trim().is_empty())
    {
        return Err(ExecutionLedgerError::InvalidConfiguration {
            name:
                "EXECUTION_LEDGER_SERVICEBUS_CONNECTION_STRING (not allowed; managed identity only)"
                    .to_string(),
        });
    }
    let namespace = step_config_string(step, "namespace")
        .or_else(|| std::env::var("EXECUTION_LEDGER_SERVICEBUS_NAMESPACE").ok())
        .ok_or(ExecutionLedgerError::InvalidConfiguration {
            name: "EXECUTION_LEDGER_SERVICEBUS_NAMESPACE".to_string(),
        })?;
    Ok(ServiceBusTarget {
        queue_name,
        namespace,
    })
}

fn resolve_filesystem_target(
    step: &ConnectorStep,
) -> Result<(String, PathBuf), ExecutionLedgerError> {
    let root = step_config_string(step, "directory")
        .or_else(|| std::env::var("EXECUTION_LEDGER_QUEUE_DIR").ok())
        .ok_or(ExecutionLedgerError::InvalidConfiguration {
            name: "EXECUTION_LEDGER_QUEUE_DIR".to_string(),
        })?;
    let queue_name = step_config_string(step, "queueName").unwrap_or_else(|| "default".to_string());
    let directory = PathBuf::from(&root).join(&queue_name);
    Ok((queue_name, directory))
}

fn dependency_error(
    dependency: &'static str,
    source: impl std::error::Error + Send + Sync + 'static,
) -> ExecutionLedgerError {
    ExecutionLedgerError::DependencyFailed {
        dependency,
        source: Box::new(source),
    }
}

pub(crate) async fn execute(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let provider = step_config_string(step, "provider")
        .or_else(|| std::env::var("EXECUTION_LEDGER_QUEUE_PROVIDER").ok())
        .unwrap_or_else(|| "servicebus".to_string());
    let payload = json!({
        "stepId": step.id,
        "jobId": ctx.job_id,
        "correlationId": ctx.correlation_id,
        "event": ctx.event,
        "input": step.input,
        "config": step.config,
        "runId": ctx.run_id,
    });

    match provider.as_str() {
        "servicebus" => execute_servicebus(ctx, step, &payload).await,
        "filesystem" | "local" => execute_filesystem(ctx, step, &payload).await,
        _ => Err(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.queue.provider".to_string(),
        }),
    }
}

async fn execute_servicebus(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
    payload: &Value,
) -> Result<Value, ExecutionLedgerError> {
    let target = resolve_servicebus_target(step)?;
    let client =
        ServiceBusQueueClient::from_managed_identity(&target.namespace, &target.queue_name)?;
    client.enqueue_json_message(payload).await?;
    Ok(json!({
        "stepId": step.id,
        "connector": "queue",
        "action": "send",
        "provider": "servicebus",
        "queueName": target.queue_name,
        "accepted": true,
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

#[allow(clippy::unused_async)] // called via tokio::spawn with async block
async fn execute_filesystem(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
    payload: &Value,
) -> Result<Value, ExecutionLedgerError> {
    let (queue_name, directory) = resolve_filesystem_target(step)?;
    fs::create_dir_all(&directory).map_err(|source| dependency_error("filesystem", source))?;
    let path = directory.join(format!("{}-{}.json", ctx.run_id, step.id));
    let bytes = serde_json::to_vec_pretty(payload)
        .map_err(|source| dependency_error("serde_json", source))?;
    fs::write(&path, bytes).map_err(|source| dependency_error("filesystem", source))?;
    Ok(json!({
        "stepId": step.id,
        "connector": "queue",
        "action": "send",
        "provider": "filesystem",
        "queueName": queue_name,
        "messagePath": path.to_string_lossy(),
        "accepted": true,
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Receives (and removes) one message from the queue. See the module-level
/// "Receive and compensation" doc for the completion-timing tradeoff.
pub(crate) async fn receive(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let provider = step_config_string(step, "provider")
        .or_else(|| std::env::var("EXECUTION_LEDGER_QUEUE_PROVIDER").ok())
        .unwrap_or_else(|| "servicebus".to_string());

    match provider.as_str() {
        "servicebus" => receive_servicebus(ctx, step).await,
        "filesystem" | "local" => receive_filesystem(ctx, step).await,
        _ => Err(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.queue.provider".to_string(),
        }),
    }
}

fn no_message_output(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
    provider: &str,
    queue_name: &str,
) -> Value {
    json!({
        "stepId": step.id,
        "connector": "queue",
        "action": "receive",
        "provider": provider,
        "queueName": queue_name,
        "received": false,
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    })
}

async fn receive_servicebus(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let target = resolve_servicebus_target(step)?;
    let client =
        ServiceBusQueueClient::from_managed_identity(&target.namespace, &target.queue_name)?;

    match client.receive_raw_message().await? {
        ServiceBusRawReceiveOutcome::NoMessage => Ok(no_message_output(
            ctx,
            step,
            "servicebus",
            &target.queue_name,
        )),
        ServiceBusRawReceiveOutcome::Received {
            lock_uri,
            delivery_count,
            body,
        } => {
            // Complete (remove) the message now — see the module-level
            // "Receive and compensation" doc for why this happens within
            // the same step rather than being deferred.
            client.complete_locked_message(&lock_uri).await?;
            let parsed_body = parse_json_or_text(&String::from_utf8_lossy(&body));
            Ok(json!({
                "stepId": step.id,
                "connector": "queue",
                "action": "receive",
                "provider": "servicebus",
                "queueName": target.queue_name,
                "received": true,
                "deliveryCount": delivery_count,
                "body": parsed_body,
                "stateBlob": {
                    "provider": "servicebus",
                    "queueName": target.queue_name,
                    "namespace": target.namespace,
                    "body": parsed_body
                },
                "correlationId": ctx.correlation_id,
                "event": ctx.event
            }))
        }
    }
}

#[allow(clippy::unused_async)] // satisfies async connector interface; dispatched via .await in mod.rs
async fn receive_filesystem(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let (queue_name, directory) = resolve_filesystem_target(step)?;

    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(no_message_output(ctx, step, "filesystem", &queue_name));
        }
        Err(source) => return Err(dependency_error("filesystem", source)),
    };

    let mut oldest: Option<(PathBuf, std::time::SystemTime)> = None;
    for entry in entries {
        let entry = entry.map_err(|source| dependency_error("filesystem", source))?;
        let metadata = entry
            .metadata()
            .map_err(|source| dependency_error("filesystem", source))?;
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata
            .modified()
            .map_err(|source| dependency_error("filesystem", source))?;
        if oldest.as_ref().is_none_or(|(_, m)| modified < *m) {
            oldest = Some((entry.path(), modified));
        }
    }

    let Some((path, _)) = oldest else {
        return Ok(no_message_output(ctx, step, "filesystem", &queue_name));
    };

    let bytes = fs::read(&path).map_err(|source| dependency_error("filesystem", source))?;
    fs::remove_file(&path).map_err(|source| dependency_error("filesystem", source))?;
    let parsed_body = parse_json_or_text(&String::from_utf8_lossy(&bytes));

    Ok(json!({
        "stepId": step.id,
        "connector": "queue",
        "action": "receive",
        "provider": "filesystem",
        "queueName": queue_name,
        "received": true,
        "body": parsed_body,
        "stateBlob": {
            "provider": "filesystem",
            "directory": directory.to_string_lossy(),
            "queueName": queue_name,
            "body": parsed_body
        },
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Compensation depends on which action produced the original output.
/// `send` cannot be recalled — a compensating message is the correct
/// pattern for downstream consumers to unwind it. `receive` removed a
/// message from the queue; compensation re-publishes it from the captured
/// `stateBlob`, undoing the receive.
pub(crate) async fn compensate(
    _ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
    original_output: &Value,
) -> Result<Value, ExecutionLedgerError> {
    let action = original_output
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("send");

    if action != "receive" {
        return Ok(json!({
            "stepId": step.id,
            "connector": "queue",
            "compensated": true,
            "action": "noop",
            "reason": "sent messages cannot be recalled"
        }));
    }

    let received = original_output
        .get("received")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !received {
        return Ok(json!({
            "stepId": step.id,
            "connector": "queue",
            "compensated": true,
            "action": "noop",
            "reason": "no message was received"
        }));
    }

    let Some(state_blob) = original_output.get("stateBlob") else {
        return Ok(json!({
            "stepId": step.id,
            "connector": "queue",
            "compensated": true,
            "action": "noop",
            "reason": "no stateBlob to restore from"
        }));
    };

    let provider = state_blob
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("servicebus");
    let body = state_blob.get("body").cloned().unwrap_or(Value::Null);

    match provider {
        "servicebus" => {
            let queue_name = state_blob
                .get("queueName")
                .and_then(Value::as_str)
                .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
                    name: "connector.queue.compensate.stateBlob.queueName".to_string(),
                })?;
            let namespace = state_blob
                .get("namespace")
                .and_then(Value::as_str)
                .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
                    name: "connector.queue.compensate.stateBlob.namespace".to_string(),
                })?;
            let client = ServiceBusQueueClient::from_managed_identity(namespace, queue_name)?;
            client.enqueue_json_message(&body).await?;
        }
        "filesystem" => {
            let directory = state_blob
                .get("directory")
                .and_then(Value::as_str)
                .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
                    name: "connector.queue.compensate.stateBlob.directory".to_string(),
                })?;
            fs::create_dir_all(directory)
                .map_err(|source| dependency_error("filesystem", source))?;
            let path = PathBuf::from(directory).join(format!("{}-restored.json", step.id));
            let bytes = serde_json::to_vec_pretty(&body)
                .map_err(|source| dependency_error("serde_json", source))?;
            fs::write(&path, bytes).map_err(|source| dependency_error("filesystem", source))?;
        }
        _ => {
            return Ok(json!({
                "stepId": step.id,
                "connector": "queue",
                "compensated": true,
                "action": "noop",
                "reason": "unknown provider in stateBlob, cannot restore"
            }));
        }
    }

    Ok(json!({
        "stepId": step.id,
        "connector": "queue",
        "compensated": true,
        "action": "restore",
        "restoredFromStateBlob": state_blob
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

    fn test_step(action: Option<&str>, config: Value) -> ConnectorStep {
        ConnectorStep {
            id: "queue-step".to_string(),
            connector: ConnectorKind::Queue,
            action: action.map(str::to_string),
            external_id: None,
            input: json!({}),
            config,
            input_schema: None,
            output_schema: None,
        }
    }

    fn temp_queue_dir() -> PathBuf {
        std::env::temp_dir().join(format!("execution-ledger-queue-test-{}", Uuid::new_v4()))
    }

    /// Runs a connector function that fails before any network call and
    /// returns the error message (empty string if it unexpectedly succeeded).
    async fn error_message<F, Fut>(f: F) -> String
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Value, ExecutionLedgerError>>,
    {
        f().await.map_or_else(|e| e.to_string(), |_| String::new())
    }

    // --- Validation: these fail before any network call, so they run
    // deterministically with no external dependencies. ---

    #[tokio::test]
    async fn receive_servicebus_missing_queue_name_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            Some("receive"),
            json!({"provider": "servicebus", "namespace": "ns.servicebus.windows.net"}),
        );
        let message = error_message(|| receive_servicebus(&ctx, &step)).await;
        assert!(message.contains("EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME"));
    }

    #[tokio::test]
    async fn receive_servicebus_missing_namespace_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            Some("receive"),
            json!({"provider": "servicebus", "queueName": "incoming"}),
        );
        let message = error_message(|| receive_servicebus(&ctx, &step)).await;
        assert!(message.contains("EXECUTION_LEDGER_SERVICEBUS_NAMESPACE"));
    }

    #[tokio::test]
    async fn receive_servicebus_rejects_connection_string() {
        let ctx = test_ctx();
        let step = test_step(
            Some("receive"),
            json!({
                "provider": "servicebus",
                "queueName": "incoming",
                "namespace": "ns.servicebus.windows.net",
                "connectionString": "Endpoint=sb://..."
            }),
        );
        let message = error_message(|| receive_servicebus(&ctx, &step)).await;
        assert!(message.contains("EXECUTION_LEDGER_SERVICEBUS_CONNECTION_STRING"));
    }

    #[tokio::test]
    async fn receive_rejects_unknown_provider() {
        let ctx = test_ctx();
        let step = test_step(Some("receive"), json!({"provider": "carrier-pigeon"}));
        let message = error_message(|| receive(&ctx, &step)).await;
        assert!(message.contains("connector.queue.provider"));
    }

    // --- Filesystem provider: real round-trip, no mocking needed. ---

    #[tokio::test]
    async fn filesystem_receive_returns_no_message_for_empty_directory() -> std::io::Result<()> {
        let dir = temp_queue_dir();
        let ctx = test_ctx();
        let step = test_step(
            Some("receive"),
            json!({"provider": "filesystem", "directory": dir.to_string_lossy()}),
        );
        let result = receive_filesystem(&ctx, &step).await;
        let value = result.unwrap_or_else(|e| json!({"error": e.to_string()}));
        assert_eq!(value["received"], json!(false));
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn filesystem_send_then_receive_round_trips_the_same_content() -> std::io::Result<()> {
        let dir = temp_queue_dir();
        let ctx = test_ctx();
        let mut send_step = test_step(
            None,
            json!({"provider": "filesystem", "directory": dir.to_string_lossy()}),
        );
        send_step.input = json!({"orderId": "abc-123", "amount": 42});

        let send_result = execute(&ctx, &send_step).await;
        let send_value = send_result.unwrap_or_else(|e| json!({"error": e.to_string()}));
        assert_eq!(send_value["accepted"], json!(true));

        let receive_step = test_step(
            Some("receive"),
            json!({"provider": "filesystem", "directory": dir.to_string_lossy()}),
        );
        let receive_result = receive_filesystem(&ctx, &receive_step).await;
        let receive_value = receive_result.unwrap_or_else(|e| json!({"error": e.to_string()}));
        assert_eq!(receive_value["received"], json!(true));
        assert_eq!(receive_value["body"]["input"]["orderId"], json!("abc-123"));

        // A second receive finds nothing — the message was removed.
        let second_receive = receive_filesystem(&ctx, &receive_step).await;
        let second_value = second_receive.unwrap_or_else(|e| json!({"error": e.to_string()}));
        assert_eq!(second_value["received"], json!(false));

        fs::remove_dir_all(&dir)?;
        Ok(())
    }

    // --- Compensate: no-op branches are pure logic; the filesystem restore
    // branch is a real (but locally-scoped) filesystem operation. ---

    #[tokio::test]
    async fn compensate_send_is_noop_cannot_recall() {
        let ctx = test_ctx();
        let step = test_step(None, json!({}));
        let original_output = json!({"action": "send"});
        let result = compensate(&ctx, &step, &original_output).await;
        let value = result.unwrap_or_else(|e| json!({"compensateError": e.to_string()}));
        assert_eq!(
            value,
            json!({
                "stepId": "queue-step",
                "connector": "queue",
                "compensated": true,
                "action": "noop",
                "reason": "sent messages cannot be recalled"
            })
        );
    }

    #[tokio::test]
    async fn compensate_receive_without_message_is_noop() {
        let ctx = test_ctx();
        let step = test_step(Some("receive"), json!({}));
        let original_output = json!({"action": "receive", "received": false});
        let result = compensate(&ctx, &step, &original_output).await;
        let value = result.unwrap_or_else(|e| json!({"compensateError": e.to_string()}));
        assert_eq!(value["action"], json!("noop"));
        assert_eq!(value["reason"], json!("no message was received"));
    }

    #[tokio::test]
    async fn compensate_receive_without_state_blob_is_noop() {
        let ctx = test_ctx();
        let step = test_step(Some("receive"), json!({}));
        let original_output = json!({"action": "receive", "received": true});
        let result = compensate(&ctx, &step, &original_output).await;
        let value = result.unwrap_or_else(|e| json!({"compensateError": e.to_string()}));
        assert_eq!(value["action"], json!("noop"));
        assert_eq!(value["reason"], json!("no stateBlob to restore from"));
    }

    #[tokio::test]
    async fn compensate_receive_unknown_provider_is_noop() {
        let ctx = test_ctx();
        let step = test_step(Some("receive"), json!({}));
        let original_output = json!({
            "action": "receive",
            "received": true,
            "stateBlob": {"provider": "carrier-pigeon", "body": {}}
        });
        let result = compensate(&ctx, &step, &original_output).await;
        let value = result.unwrap_or_else(|e| json!({"compensateError": e.to_string()}));
        assert_eq!(value["action"], json!("noop"));
        assert_eq!(
            value["reason"],
            json!("unknown provider in stateBlob, cannot restore")
        );
    }

    #[tokio::test]
    async fn compensate_receive_filesystem_restores_the_message() -> std::io::Result<()> {
        let dir = temp_queue_dir();
        let ctx = test_ctx();
        let step = test_step(Some("receive"), json!({}));
        // stateBlob.directory mirrors what receive_filesystem actually
        // captures: the resolved root/queueName join, not the bare root.
        let queue_dir = dir.join("incoming");
        let original_output = json!({
            "action": "receive",
            "received": true,
            "stateBlob": {
                "provider": "filesystem",
                "directory": queue_dir.to_string_lossy(),
                "queueName": "incoming",
                "body": {"orderId": "restored-order"}
            }
        });

        let result = compensate(&ctx, &step, &original_output).await;
        let value = result.unwrap_or_else(|e| json!({"compensateError": e.to_string()}));
        assert_eq!(value["action"], json!("restore"));

        // The restored message is readable back via the filesystem receive path.
        let receive_step = test_step(
            Some("receive"),
            json!({
                "provider": "filesystem",
                "directory": dir.to_string_lossy(),
                "queueName": "incoming"
            }),
        );
        let receive_result = receive_filesystem(&ctx, &receive_step).await;
        let receive_value = receive_result.unwrap_or_else(|e| json!({"error": e.to_string()}));
        assert_eq!(receive_value["received"], json!(true));
        assert_eq!(receive_value["body"]["orderId"], json!("restored-order"));

        fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
