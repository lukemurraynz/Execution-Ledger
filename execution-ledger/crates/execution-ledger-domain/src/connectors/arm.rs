//! Shared ARM (Azure Resource Manager) infrastructure used by Azure compute
//! and SQL connectors. Extracted from `azure_compute` and `azure_sql` to avoid
//! ~180 lines of near-identical code duplication.
//!
//! Every ARM connector uses the same MSI auth flow, retry policy, and async
//! operation polling. This module provides those primitives so bugfixes to
//! auth, retry, or polling apply everywhere at once.

use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use azure_core::credentials::TokenCredential;
use azure_identity::{ManagedIdentityCredential, ManagedIdentityCredentialOptions, UserAssignedId};
use execution_ledger_core::ExecutionLedgerError;
use reqwest::Method;
use serde_json::{Value, json};

use crate::connectors::ConnectorExecutionContext;

/// Scope for all Azure Resource Manager management-plane APIs.
pub(crate) const ARM_SCOPE: &str = "https://management.azure.com/.default";

/// Maximum timeout for an ARM API call (90 seconds — ARM async ops may take longer).
pub(crate) const ARM_TIMEOUT_MS: u64 = 90_000;

/// Shared HTTP client for ARM calls. Reuses TLS sessions across calls.
///
/// Panics at startup if the platform TLS backend is missing — this is a
/// fatal misconfiguration that should fail fast, not degrade silently.
#[allow(clippy::expect_used)]
static ARM_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_millis(ARM_TIMEOUT_MS))
        .build()
        .expect("reqwest::Client should build with valid TLS config")
});

/// Shared managed-identity credential for ARM calls. Created once per process
/// to reuse TLS sessions and token caches. Supports user-assigned MSI via
/// `AZURE_MANAGED_IDENTITY_CLIENT_ID` env var.
///
/// Panics at startup if managed identity is unavailable — this is a fatal
/// platform misconfiguration for a service that requires MSI for ARM auth.
#[allow(clippy::expect_used)]
static ARM_CREDENTIAL: LazyLock<Arc<ManagedIdentityCredential>> = LazyLock::new(|| {
    let options = match std::env::var("AZURE_MANAGED_IDENTITY_CLIENT_ID").ok() {
        Some(client_id) => ManagedIdentityCredentialOptions {
            user_assigned_id: Some(UserAssignedId::ClientId(client_id)),
            ..Default::default()
        },
        None => ManagedIdentityCredentialOptions::default(),
    };
    ManagedIdentityCredential::new(Some(options))
        .expect("ManagedIdentityCredential::new should not fail with valid options")
});

/// Acquires an MSI access token for ARM management-plane scope.
///
/// Uses the process-wide cached credential (supports user-assigned MSI via
/// `AZURE_MANAGED_IDENTITY_CLIENT_ID` env var).
pub(crate) async fn msi_token() -> Result<String, ExecutionLedgerError> {
    let token = ARM_CREDENTIAL
        .get_token(&[ARM_SCOPE], None)
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "azure_identity",
            source: Box::new(source),
        })?;

    let access_token = token.token.secret();
    if access_token.trim().is_empty() {
        return Err(ExecutionLedgerError::DependencyFailed {
            dependency: "azure_identity",
            source: Box::new(std::io::Error::other(
                "MSI token request returned an empty access token",
            )),
        });
    }
    Ok(access_token.to_string())
}

/// An extra header to attach to every ARM request (name, value).
pub(crate) type ArmHeader<'a> = (&'a str, &'a str);

/// Execute an ARM request with MSI auth, correlation header, and caller-supplied
/// extra headers. Retries once on 429 (rate-limited) or 503 (service unavailable).
pub(crate) async fn arm_request(
    ctx: &ConnectorExecutionContext<'_>,
    method: Method,
    url: &str,
    body: Value,
    extra_headers: &[ArmHeader<'_>],
) -> Result<reqwest::Response, ExecutionLedgerError> {
    const MAX_RETRIES: u8 = 2;

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let delay = 1u64 << (attempt - 1); // 1s, 2s
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }

        let token = msi_token().await?;
        let mut req = ARM_CLIENT
            .request(method.clone(), url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .header("x-execution-ledger-correlation-id", ctx.correlation_id);

        for (name, value) in extra_headers {
            req = req.header(*name, *value);
        }

        if !body.is_null() {
            req = req.json(&body);
        }

        let response =
            req.send()
                .await
                .map_err(|source| ExecutionLedgerError::DependencyFailed {
                    dependency: "reqwest",
                    source: Box::new(source),
                })?;

        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        {
            continue;
        }
        return Ok(response);
    }
    Err(ExecutionLedgerError::DependencyFailed {
        dependency: "arm-request",
        source: Box::new(std::io::Error::other("arm_request: retries exhausted")),
    })
}

/// Polls an ARM async operation URL until it reaches a terminal state.
///
/// ARM returns `202 Accepted` with an `Azure-AsyncOperation` header for
/// long-running operations. This function polls that URL every 5 seconds
/// until `provisioningState` is `Succeeded`, `Failed`, or `Cancelled`.
pub(crate) async fn poll_arm_async_operation(
    ctx: &ConnectorExecutionContext<'_>,
    async_url: &str,
) -> Result<String, ExecutionLedgerError> {
    const POLL_INTERVAL_SECS: u64 = 5;
    const MAX_POLLS: u8 = 60; // 5 min max

    let token = msi_token().await?;

    for poll_num in 1..=MAX_POLLS {
        if poll_num > 1 {
            tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
        }

        let response = ARM_CLIENT
            .get(async_url)
            .header("Authorization", format!("Bearer {token}"))
            .header("x-execution-ledger-correlation-id", ctx.correlation_id)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "reqwest",
                source: Box::new(source),
            })?;

        let body: Value = response.json().await.unwrap_or_else(|_| json!({}));
        let provisioning_state = body
            .get("properties")
            .and_then(|p| p.get("provisioningState"))
            .and_then(|s| s.as_str())
            .unwrap_or("Unknown");

        match provisioning_state {
            "Succeeded" => return Ok("Succeeded".to_string()),
            "Creating" | "Accepted" | "Running" | "Updating" => {
                // Non-terminal — keep polling
            }
            _ => {
                return Err(ExecutionLedgerError::DependencyFailed {
                    dependency: "arm-async-operation",
                    source: Box::new(std::io::Error::other(format!(
                        "ARM async operation ended with state: {provisioning_state}"
                    ))),
                });
            }
        }
    }

    Err(ExecutionLedgerError::DependencyFailed {
        dependency: "arm-async-operation",
        source: Box::new(std::io::Error::other(format!(
            "ARM async operation polling timed out after {MAX_POLLS} polls at {async_url}"
        ))),
    })
}
