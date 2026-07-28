//! REST and ERP connectors.
//!
//! Executes HTTP requests to external APIs. The `erp` connector uses the
//! same implementation as `rest` — the distinction is semantic (connector
//! kind in the step definition) rather than behavioral.
//!
//! ## Configuration
//!
//! | Key | Source | Required | Default | Notes |
//! |-----|--------|----------|---------|-------|
//! | `url` | config or input | yes | — | Target URL |
//! | `method` | config or input | no | `POST` | HTTP method |
//! | `timeoutMs` | config or input | no | `30000` | Clamped to 120s max |
//! | `headers` | config | no | — | Custom headers (some denylisted) |
//!
//! ## Fail-closed behavior
//!
//! - Returns `InvalidConfiguration` if `url` is missing.
//! - Returns `InvalidConfiguration` if any denylisted header is present
//!   (authorization, cookie, host, x-forwarded-for, etc.).
//! - Returns `DependencyFailed` if the HTTP request fails or returns a
//!   non-2xx status.

use std::sync::LazyLock;
use std::time::Duration;

use execution_ledger_core::ExecutionLedgerError;
use reqwest::Method;
use serde_json::{Value, json};

use crate::connectors::{
    ConnectorExecutionContext, ConnectorStep,
    helpers::{
        input_string, input_u64, method_from, parse_json_or_text, request_body, step_config_object,
        step_config_string, step_config_u64,
    },
};

/// Headers that must never be forwarded from connector config to outbound
/// requests — these could be used for SSRF, auth bypass, or host routing
/// attacks.
const DENYLISTED_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "host",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
    "proxy-authorization",
];

/// Maximum timeout for connector-initiated HTTP requests (120 seconds).
/// Connector config values above this are clamped.
const CONNECTOR_MAX_TIMEOUT_MS: u64 = 120_000;

/// Shared HTTP client for all connector requests. Created once per process
/// to reuse TLS sessions and connection pools — creating a new client per
/// request wastes resources and adds latency at scale.
///
/// Panics at startup if the platform TLS backend is missing — fatal
/// misconfiguration, fail fast.
#[allow(clippy::expect_used)]
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest::Client::builder().build() should not fail with valid config")
});

pub(crate) async fn execute(
    connector: &'static str,
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let url = step_config_string(step, "url")
        .or_else(|| input_string(step, "url"))
        .ok_or(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.url".to_string(),
        })?;
    let method =
        method_from(step_config_string(step, "method").or_else(|| input_string(step, "method")))
            .unwrap_or(Method::POST);
    let timeout_ms = step_config_u64(step, "timeoutMs")
        .or_else(|| input_u64(step, "timeoutMs"))
        .unwrap_or(30_000)
        .min(CONNECTOR_MAX_TIMEOUT_MS);
    let body = request_body(step);
    let method_name = method.as_str().to_string();

    let mut request = HTTP_CLIENT
        .request(method, &url)
        .timeout(Duration::from_millis(timeout_ms))
        .header("x-execution-ledger-correlation-id", ctx.correlation_id)
        .header("x-execution-ledger-job-id", ctx.job_id)
        .header("x-execution-ledger-event", ctx.event);

    if let Some(headers) = step_config_object(step, "headers") {
        for (key, value) in headers {
            if let Some(value) = value.as_str() {
                // Reject denylisted headers that could be used for SSRF,
                // auth bypass, or host routing attacks.
                if DENYLISTED_HEADERS.contains(&key.to_lowercase().as_str()) {
                    return Err(ExecutionLedgerError::InvalidConfiguration {
                        name: "connector.rest.headers (denylisted header)".to_string(),
                    });
                }
                request = request.header(key, value);
            }
        }
    }

    if !body.is_null() {
        request = request.json(&body);
    }

    let response =
        request
            .send()
            .await
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "reqwest",
                source: Box::new(source),
            })?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "reqwest",
            source: Box::new(source),
        })?;
    let response_body = parse_json_or_text(&text);

    if !status.is_success() {
        return Err(ExecutionLedgerError::DependencyFailed {
            dependency: "reqwest",
            source: Box::new(std::io::Error::other(format!(
                "{connector} request failed with status {status}: {text}"
            ))),
        });
    }

    Ok(json!({
        "stepId": step.id,
        "connector": connector,
        "url": url,
        "method": method_name,
        "request": body,
        "responseStatus": status.as_u16(),
        "response": response_body,
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Compensate a REST/ERP step by calling a reverse endpoint if one is
/// configured. When `config.compensateUrl` is set, the connector sends
/// the original output as the compensation payload to that URL. Otherwise,
/// compensation is a no-op — the assumption is that downstream systems
/// handle their own reversal through a compensating queue message.
pub(crate) async fn compensate(
    connector: &'static str,
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
    original_output: &Value,
) -> Result<Value, ExecutionLedgerError> {
    let compensate_url = step_config_string(step, "compensateUrl");
    let Some(url) = compensate_url else {
        return Ok(json!({
            "stepId": step.id,
            "connector": connector,
            "compensated": true,
            "action": "noop"
        }));
    };

    let method = Method::POST;
    let body = original_output.clone();
    let timeout_ms = step_config_u64(step, "timeoutMs")
        .unwrap_or(30_000)
        .min(CONNECTOR_MAX_TIMEOUT_MS);

    let request = HTTP_CLIENT
        .request(method, &url)
        .timeout(Duration::from_millis(timeout_ms))
        .header("x-execution-ledger-correlation-id", ctx.correlation_id)
        .header("x-execution-ledger-compensation", "true")
        .json(&body);

    let response =
        request
            .send()
            .await
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "reqwest",
                source: Box::new(source),
            })?;

    if !response.status().is_success() {
        let status = response.status();
        let body_text = match response.text().await {
            Ok(t) => t,
            Err(read_err) => {
                return Err(ExecutionLedgerError::DependencyFailed {
                    dependency: "reqwest",
                    source: Box::new(std::io::Error::other(format!(
                        "{connector} compensation request failed with status {status} \
                         (response body unreadable: {read_err})"
                    ))),
                });
            }
        };
        return Err(ExecutionLedgerError::DependencyFailed {
            dependency: "reqwest",
            source: Box::new(std::io::Error::other(format!(
                "{connector} compensation request failed with status {status}: {body_text}"
            ))),
        });
    }

    Ok(json!({
        "stepId": step.id,
        "connector": connector,
        "compensated": true,
        "action": "compensate",
        "compensateUrl": url
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

    fn test_step_with_headers(headers: Value) -> ConnectorStep {
        ConnectorStep {
            id: "rest-call".to_string(),
            connector: ConnectorKind::Rest,
            action: None,
            external_id: None,
            input: json!({"body": "test"}),
            config: json!({
                "url": "http://127.0.0.1:1/test",
                "headers": headers
            }),
            input_schema: None,
            output_schema: None,
        }
    }

    #[tokio::test]
    async fn rejects_denylisted_authorization_header() {
        let ctx = test_ctx();
        let step = test_step_with_headers(json!({"Authorization": "Bearer token"}));
        let result = execute("rest", &ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            message.contains("denylisted header"),
            "expected denylisted header error, got: {message:?}"
        );
    }

    #[tokio::test]
    async fn rejects_denylisted_cookie_header() {
        let ctx = test_ctx();
        let step = test_step_with_headers(json!({"Cookie": "session=abc"}));
        let result = execute("rest", &ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            message.contains("denylisted header"),
            "expected denylisted header error, got: {message:?}"
        );
    }

    #[tokio::test]
    async fn accepts_non_denylisted_custom_header() {
        let ctx = test_ctx();
        let step = test_step_with_headers(json!({"X-Custom-Header": "value"}));
        let result = execute("rest", &ctx, &step).await;
        // Connection to 127.0.0.1:1 will fail, but not because of the header
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(
            !message.is_empty() && !message.contains("denylisted header"),
            "custom header should not be denylisted: {message:?}"
        );
    }
}
