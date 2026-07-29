//! SFTP connector.
//!
//! Transfers files over SFTP using SSH2. Runs the blocking SFTP operation
//! on a dedicated thread with a 60-second timeout to prevent hanging the
//! tokio blocking thread pool.
//!
//! Supports two actions:
//!
//! - **`upload`** (default when `action` is omitted): writes the step's
//!   `input` to `remotePath` on the server. This is the original behavior
//!   and stays the default so existing job templates that don't set
//!   `action` keep working unchanged. If `input` is left empty/null, it
//!   falls back to the previous step's base64 output instead (e.g. `pgp`
//!   encrypt or another `sftp` download) — the same "leave `data`/`input`
//!   empty to chain from upstream" convention `azure_blob` already uses.
//! - **`download`**: reads `remotePath` from the server and returns its
//!   content, base64-encoded, in the step output (`content` /
//!   `contentEncoding` fields). There is no local-disk write — the file
//!   never touches the worker's filesystem. A downstream step (e.g.
//!   `azure_blob`) reads the content via `ctx.upstream_output`.
//!
//! ## Configuration
//!
//! | Key | Source | Required | Default | Notes |
//! |-----|--------|----------|---------|-------|
//! | `action` | config | no | `upload` | `upload` or `download` |
//! | `host` | config or env | yes | — | SFTP server hostname |
//! | `username` | config or env | yes | — | SSH username |
//! | `port` | config or env | no | `22` | SSH port |
//! | `remotePath` | config, input, or externalId | yes | — | File path on server |
//! | `password` | config or env | no | — | SSH password (mutually exclusive with privateKeyPath) |
//! | `privateKeyPath` | config or env | no | — | Path to SSH private key |
//! | `passphrase` | config or env | no | — | Passphrase for encrypted private key |
//!
//! ## Fail-closed behavior
//!
//! - Returns `InvalidConfiguration` if `host`, `username`, or `remotePath` is missing.
//! - Returns `InvalidConfiguration` if `remotePath` contains `..` (path traversal).
//! - Returns `InvalidConfiguration` if neither `password` nor `privateKeyPath` is provided.
//! - Returns `DependencyFailed` if SSH connection, authentication, or file transfer fails.
//! - Returns `DependencyFailed` if the operation exceeds 60 seconds.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;
use tokio::task;

use base64::Engine;
use execution_ledger_core::ExecutionLedgerError;
use serde_json::{Value, json};

use crate::connectors::helpers::{input_string, step_config_string, step_config_u64};
use crate::connectors::{ConnectorExecutionContext, ConnectorStep};

/// Resolves the bytes to upload for the default `upload` action.
///
/// If the step's own `input` is non-empty, it is serialized and uploaded
/// verbatim — this is the original behavior and stays unchanged so existing
/// job templates that put data directly in `input` keep working exactly as
/// before. Only when `input` is empty/null does this fall back to the
/// upstream step's base64 output (the shape `sftp download` and `pgp`
/// produce), matching the same "leave `data`/`input` empty to chain from the
/// previous step" convention `azure_blob` already documents.
fn resolve_upload_bytes(
    step: &ConnectorStep,
    upstream_output: Option<&Value>,
) -> Result<Vec<u8>, ExecutionLedgerError> {
    let input_is_empty = matches!(&step.input, Value::Null)
        || matches!(&step.input, Value::Object(map) if map.is_empty());

    if !input_is_empty {
        return serde_json::to_vec_pretty(&step.input).map_err(|source| {
            ExecutionLedgerError::DependencyFailed {
                dependency: "serde_json",
                source: Box::new(source),
            }
        });
    }

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

    serde_json::to_vec_pretty(&step.input).map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "serde_json",
        source: Box::new(source),
    })
}

/// Resolved connection and target details shared by upload and download.
struct SftpTarget {
    host: String,
    port: u16,
    username: String,
    remote_path: String,
    password: Option<String>,
    private_key_path: Option<String>,
    passphrase: Option<String>,
}

/// Resolves host/port/username/auth — everything `connect_and_authenticate`
/// needs. Does not require `remotePath`, so this is also what
/// `test_connection` (used by CLI preflight) calls directly: proving the
/// platform can reach and authenticate to an endpoint doesn't require
/// naming a specific file on it.
fn resolve_connection(step: &ConnectorStep) -> Result<SftpTarget, ExecutionLedgerError> {
    let host = step_config_string(step, "host")
        .or_else(|| std::env::var("EXECUTION_LEDGER_SFTP_HOST").ok())
        .ok_or(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.sftp.host".to_string(),
        })?;
    let username = step_config_string(step, "username")
        .or_else(|| std::env::var("EXECUTION_LEDGER_SFTP_USERNAME").ok())
        .ok_or(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.sftp.username".to_string(),
        })?;
    let port_raw = step_config_u64(step, "port")
        .or_else(|| {
            std::env::var("EXECUTION_LEDGER_SFTP_PORT")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
        })
        .unwrap_or(22);
    if port_raw > u64::from(u16::MAX) {
        return Err(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.sftp.port".to_string(),
        });
    }
    #[allow(clippy::cast_possible_truncation)] // checked against u16::MAX above
    let port = port_raw as u16;

    let password = step_config_string(step, "password")
        .or_else(|| std::env::var("EXECUTION_LEDGER_SFTP_PASSWORD").ok());
    let private_key_path = step_config_string(step, "privateKeyPath")
        .or_else(|| std::env::var("EXECUTION_LEDGER_SFTP_PRIVATE_KEY_PATH").ok());
    let passphrase = step_config_string(step, "passphrase")
        .or_else(|| std::env::var("EXECUTION_LEDGER_SFTP_PASSPHRASE").ok());

    Ok(SftpTarget {
        host,
        port,
        username,
        remote_path: String::new(),
        password,
        private_key_path,
        passphrase,
    })
}

fn resolve_target(step: &ConnectorStep) -> Result<SftpTarget, ExecutionLedgerError> {
    let mut target = resolve_connection(step)?;

    let remote_path = step_config_string(step, "remotePath")
        .or_else(|| input_string(step, "remotePath"))
        .or_else(|| step.external_id.clone())
        .ok_or(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.sftp.remotePath".to_string(),
        })?;

    // Reject paths containing ".." segments to prevent path traversal attacks.
    if remote_path.contains("..") {
        return Err(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.sftp.remotePath (path traversal rejected)".to_string(),
        });
    }

    target.remote_path = remote_path;
    Ok(target)
}

/// Proves the platform can reach and authenticate to an SFTP endpoint,
/// without transferring anything. Used by `execution-ledger-cli`'s
/// `preflight` command — see `connectors::test_connectivity`.
pub(crate) async fn test_connection(step: &ConnectorStep) -> Result<Value, ExecutionLedgerError> {
    let target = resolve_connection(step)?;
    let host_for_log = target.host.clone();
    let port = target.port;

    let join = task::spawn_blocking(move || {
        connect_and_authenticate(&target)?;
        Ok(json!({
            "connector": "sftp",
            "connected": true,
            "host": host_for_log,
            "port": port
        }))
    });

    run_with_timeout(join).await
}

/// Connects, performs the SSH handshake, and authenticates. Blocking —
/// callers must invoke this from inside `task::spawn_blocking`.
fn connect_and_authenticate(target: &SftpTarget) -> Result<ssh2::Session, ExecutionLedgerError> {
    let tcp = TcpStream::connect((target.host.as_str(), target.port)).map_err(|source| {
        ExecutionLedgerError::DependencyFailed {
            dependency: "sftp",
            source: Box::new(source),
        }
    })?;
    let mut session =
        ssh2::Session::new().map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "sftp",
            source: Box::new(source),
        })?;
    session.set_tcp_stream(tcp);
    session
        .handshake()
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "sftp",
            source: Box::new(source),
        })?;

    if let Some(private_key_path) = &target.private_key_path {
        session
            .userauth_pubkey_file(
                &target.username,
                None,
                PathBuf::from(private_key_path).as_path(),
                target.passphrase.as_deref(),
            )
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "sftp",
                source: Box::new(source),
            })?;
    } else if let Some(password) = &target.password {
        session
            .userauth_password(&target.username, password)
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "sftp",
                source: Box::new(source),
            })?;
    } else {
        return Err(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.sftp.password".to_string(),
        });
    }

    if !session.authenticated() {
        return Err(ExecutionLedgerError::DependencyFailed {
            dependency: "sftp",
            source: Box::new(std::io::Error::other("sftp authentication failed")),
        });
    }

    Ok(session)
}

/// Wraps a blocking SFTP closure in the shared 60-second timeout.
async fn run_with_timeout(
    join: tokio::task::JoinHandle<Result<Value, ExecutionLedgerError>>,
) -> Result<Value, ExecutionLedgerError> {
    let join_result = tokio::time::timeout(Duration::from_mins(1), join)
        .await
        .map_err(|_| ExecutionLedgerError::DependencyFailed {
            dependency: "sftp",
            source: Box::new(std::io::Error::other("sftp operation timed out after 60s")),
        })?;
    join_result.map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "sftp",
        source: Box::new(source),
    })?
}

/// Uploads (writes) the step's `input` to `remotePath` on the server.
/// This is the default action when `action` is omitted from step config.
pub(crate) async fn execute(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let target = resolve_target(step)?;
    let data = resolve_upload_bytes(step, ctx.upstream_output)?;
    let host_for_log = target.host.clone();
    let username_for_log = target.username.clone();
    let remote_path_for_log = target.remote_path.clone();
    let port = target.port;
    let step_id = step.id.clone();
    let correlation_id = ctx.correlation_id.to_string();
    let event = ctx.event.to_string();

    let join = task::spawn_blocking(move || {
        let session = connect_and_authenticate(&target)?;
        let sftp = session
            .sftp()
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "sftp",
                source: Box::new(source),
            })?;
        let mut file = sftp
            .create(PathBuf::from(&target.remote_path).as_path())
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "sftp",
                source: Box::new(source),
            })?;
        file.write_all(&data)
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "sftp",
                source: Box::new(source),
            })?;

        Ok(json!({
            "stepId": step_id,
            "connector": "sftp",
            "action": "upload",
            "host": host_for_log,
            "port": port,
            "username": username_for_log,
            "remotePath": remote_path_for_log,
            "bytes": data.len(),
            "correlationId": correlation_id,
            "event": event
        }))
    });

    run_with_timeout(join).await
}

/// Reads `remotePath` from the server and returns its content, base64-encoded,
/// in the step output. Does not write to the worker's local filesystem — a
/// downstream step reads the content via `ctx.upstream_output`.
pub(crate) async fn download(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let target = resolve_target(step)?;
    let host_for_log = target.host.clone();
    let username_for_log = target.username.clone();
    let remote_path_for_log = target.remote_path.clone();
    let port = target.port;
    let step_id = step.id.clone();
    let correlation_id = ctx.correlation_id.to_string();
    let event = ctx.event.to_string();

    let join = task::spawn_blocking(move || {
        let session = connect_and_authenticate(&target)?;
        let sftp = session
            .sftp()
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "sftp",
                source: Box::new(source),
            })?;
        let mut file = sftp
            .open(PathBuf::from(&target.remote_path).as_path())
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "sftp",
                source: Box::new(source),
            })?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "sftp",
                source: Box::new(source),
            })?;

        Ok(json!({
            "stepId": step_id,
            "connector": "sftp",
            "action": "download",
            "host": host_for_log,
            "port": port,
            "username": username_for_log,
            "remotePath": remote_path_for_log,
            "bytes": data.len(),
            "content": base64::engine::general_purpose::STANDARD.encode(&data),
            "contentEncoding": "base64",
            "correlationId": correlation_id,
            "event": event
        }))
    });

    run_with_timeout(join).await
}

/// Compensation depends on which action produced the original output:
/// `download` is read-only (no-op); `upload` wrote a file, and deleting it
/// is destructive and should be guided by operator review rather than
/// automatic reversal — a compensating file or notification is the safer
/// pattern.
#[allow(clippy::unused_async)] // satisfies async connector interface; dispatched via .await in mod.rs
pub(crate) async fn compensate(
    _ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
    original_output: &Value,
) -> Result<Value, ExecutionLedgerError> {
    let action = original_output
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("upload");
    let reason = if action == "download" {
        "read-only action"
    } else {
        "deleting uploaded files is destructive; requires operator review"
    };
    Ok(json!({
        "stepId": step.id,
        "connector": "sftp",
        "compensated": true,
        "action": "noop",
        "reason": reason
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::ConnectorKind;
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

    fn test_step(action: Option<&str>, port: u64) -> ConnectorStep {
        ConnectorStep {
            id: "sftp-step".to_string(),
            connector: ConnectorKind::Sftp,
            action: action.map(str::to_string),
            external_id: None,
            input: json!({"body": "test payload"}),
            config: json!({
                "host": "invalid.test.host",
                "username": "testuser",
                "port": port,
                "remotePath": "/tmp/test.txt"
            }),
            input_schema: None,
            output_schema: None,
        }
    }

    fn test_step_with_input(input: Value) -> ConnectorStep {
        let mut step = test_step(None, 22);
        step.input = input;
        step
    }

    #[test]
    fn resolve_upload_bytes_uses_explicit_input_when_non_empty() -> Result<(), Box<dyn std::error::Error>>
    {
        let step = test_step_with_input(json!({"body": "explicit"}));
        let resolved = resolve_upload_bytes(&step, None).map_err(|e| e.to_string())?;
        assert_eq!(resolved, serde_json::to_vec_pretty(&json!({"body": "explicit"}))?);
        Ok(())
    }

    #[test]
    fn resolve_upload_bytes_prefers_explicit_input_over_upstream()
    -> Result<(), Box<dyn std::error::Error>> {
        let step = test_step_with_input(json!({"body": "explicit"}));
        let upstream = json!({
            "content": base64::engine::general_purpose::STANDARD.encode("upstream bytes"),
            "contentEncoding": "base64"
        });
        let resolved = resolve_upload_bytes(&step, Some(&upstream)).map_err(|e| e.to_string())?;
        assert_eq!(resolved, serde_json::to_vec_pretty(&json!({"body": "explicit"}))?);
        Ok(())
    }

    #[test]
    fn resolve_upload_bytes_falls_back_to_upstream_when_input_empty()
    -> Result<(), Box<dyn std::error::Error>> {
        let step = test_step_with_input(json!({}));
        let upstream = json!({
            "content": base64::engine::general_purpose::STANDARD.encode("encrypted payload"),
            "contentEncoding": "base64"
        });
        let resolved = resolve_upload_bytes(&step, Some(&upstream)).map_err(|e| e.to_string())?;
        assert_eq!(resolved, b"encrypted payload");
        Ok(())
    }

    #[test]
    fn resolve_upload_bytes_falls_back_when_input_null() -> Result<(), Box<dyn std::error::Error>> {
        let step = test_step_with_input(Value::Null);
        let upstream = json!({
            "content": base64::engine::general_purpose::STANDARD.encode("payload"),
            "contentEncoding": "base64"
        });
        let resolved = resolve_upload_bytes(&step, Some(&upstream)).map_err(|e| e.to_string())?;
        assert_eq!(resolved, b"payload");
        Ok(())
    }

    #[test]
    fn resolve_upload_bytes_serializes_empty_input_with_no_upstream()
    -> Result<(), Box<dyn std::error::Error>> {
        let step = test_step_with_input(json!({}));
        let resolved = resolve_upload_bytes(&step, None).map_err(|e| e.to_string())?;
        assert_eq!(resolved, serde_json::to_vec_pretty(&json!({}))?);
        Ok(())
    }

    #[tokio::test]
    async fn rejects_port_above_u16_max() {
        let ctx = test_ctx();
        let step = test_step(None, u64::from(u16::MAX) + 1);
        let result = execute(&ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            message.contains("port"),
            "expected port error, got: {message:?}"
        );
    }

    #[tokio::test]
    async fn accepts_port_within_u16_range() {
        let ctx = test_ctx();
        let step = test_step(None, 22);
        // Port is valid; connection to invalid.test.host will fail,
        // but the error should not be about port configuration.
        let result = execute(&ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            !message.is_empty() && !message.contains("port"),
            "expected non-port error, got: {message:?}"
        );
    }

    #[tokio::test]
    async fn download_rejects_port_above_u16_max() {
        let ctx = test_ctx();
        let step = test_step(Some("download"), u64::from(u16::MAX) + 1);
        let result = download(&ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            message.contains("port"),
            "expected port error, got: {message:?}"
        );
    }

    #[tokio::test]
    async fn download_rejects_path_traversal() {
        let ctx = test_ctx();
        let mut step = test_step(Some("download"), 22);
        step.config = json!({
            "host": "invalid.test.host",
            "username": "testuser",
            "remotePath": "/tmp/../etc/passwd"
        });
        let result = download(&ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            message.contains("path traversal"),
            "expected path traversal rejection, got: {message:?}"
        );
    }

    #[tokio::test]
    async fn compensate_download_is_noop_read_only() {
        let ctx = test_ctx();
        let step = test_step(Some("download"), 22);
        let original_output = json!({"action": "download"});
        let result = compensate(&ctx, &step, &original_output).await;
        let value = result.unwrap_or_else(|e| json!({"compensateError": e.to_string()}));
        assert_eq!(
            value,
            json!({
                "stepId": "sftp-step",
                "connector": "sftp",
                "compensated": true,
                "action": "noop",
                "reason": "read-only action"
            })
        );
    }

    #[tokio::test]
    async fn compensate_upload_is_noop_destructive_reason() {
        let ctx = test_ctx();
        let step = test_step(None, 22);
        let original_output = json!({"action": "upload"});
        let result = compensate(&ctx, &step, &original_output).await;
        let value = result.unwrap_or_else(|e| json!({"compensateError": e.to_string()}));
        assert_eq!(
            value,
            json!({
                "stepId": "sftp-step",
                "connector": "sftp",
                "compensated": true,
                "action": "noop",
                "reason": "deleting uploaded files is destructive; requires operator review"
            })
        );
    }
}
