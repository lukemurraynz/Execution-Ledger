//! Microsoft Graph notification connector.
//!
//! Sends outbound notifications through Microsoft Graph using managed identity.
//! Supports:
//! - `teams_channel_message`: post a message to a Teams channel
//! - `outlook_send_mail`: send an email as a user mailbox
//!
//! ## Configuration
//!
//! | Key | Source | Required | Notes |
//! |-----|--------|----------|-------|
//! | `url` | config or input | no | Full Graph endpoint. If present, `target` is ignored and `input.body` (or full input) is forwarded |
//! | `target` | config or input | yes (without `url`) | `teams_channel_message` or `outlook_send_mail` |
//! | `teamId` | config or input | yes (`teams_channel_message`) | Teams team ID |
//! | `channelId` | config or input | yes (`teams_channel_message`) | Teams channel ID |
//! | `messageText` | config or input | yes (`teams_channel_message`) | Message content (HTML content type) |
//! | `userId` | config or input | yes (`outlook_send_mail`) | Mailbox user ID or UPN |
//! | `to` | config or input | yes (`outlook_send_mail`) | Recipient email or array of emails |
//! | `subject` | config or input | yes (`outlook_send_mail`) | Email subject |
//! | `bodyText` | config or input | yes (`outlook_send_mail`) | Email body text |
//! | `bodyContentType` | config or input | no | `Text` (default) or `HTML` |
//! | `saveToSentItems` | config or input | no | `false` default |
//! | `timeoutMs` | config or input | no | Request timeout, capped at 120s |
//!
//! ## Fail-closed behavior
//!
//! - Returns `InvalidConfiguration` for missing required target fields.
//! - Returns `DependencyFailed` when token acquisition or Graph calls fail.
//! - Never falls back to connection strings or API keys.

use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use azure_core::credentials::TokenCredential;
use azure_identity::{ManagedIdentityCredential, ManagedIdentityCredentialOptions, UserAssignedId};
use execution_ledger_core::ExecutionLedgerError;
use serde_json::{Value, json};

use crate::connectors::helpers::{
    input_string, input_u64, parse_json_or_text, request_body, step_config_string, step_config_u64,
};
use crate::connectors::{ConnectorExecutionContext, ConnectorStep};

const GRAPH_SCOPE: &str = "https://graph.microsoft.com/.default";
const GRAPH_BASE_URL: &str = "https://graph.microsoft.com/v1.0";
const GRAPH_MAX_TIMEOUT_MS: u64 = 120_000;

#[allow(clippy::expect_used)]
static GRAPH_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest::Client should build with valid TLS config")
});

#[allow(clippy::expect_used)]
static GRAPH_CREDENTIAL: LazyLock<Arc<ManagedIdentityCredential>> = LazyLock::new(|| {
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

fn config_or_input_string(step: &ConnectorStep, key: &str) -> Option<String> {
    step_config_string(step, key).or_else(|| input_string(step, key))
}

fn config_or_input_u64(step: &ConnectorStep, key: &str) -> Option<u64> {
    step_config_u64(step, key).or_else(|| input_u64(step, key))
}

fn required_config(step: &ConnectorStep, key: &str) -> Result<String, ExecutionLedgerError> {
    config_or_input_string(step, key).ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
        name: format!("connector.ms_graph_notify.{key}"),
    })
}

fn recipients_from(step: &ConnectorStep) -> Option<Vec<String>> {
    if let Some(single) = config_or_input_string(step, "to") {
        return Some(vec![single]);
    }
    let from_config = step.config.get("to");
    let from_input = step.input.get("to");
    from_config.or(from_input).and_then(|value| {
        value.as_array().map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
    })
}

fn build_target_request(
    step: &ConnectorStep,
) -> Result<(String, String, Value), ExecutionLedgerError> {
    if let Some(url) = config_or_input_string(step, "url") {
        // keep custom Graph calls generic via url+body to avoid a large action matrix.
        return Ok(("custom".to_string(), url, request_body(step)));
    }

    let target = required_config(step, "target")?;
    match target.as_str() {
        "teams_channel_message" => {
            let team_id = required_config(step, "teamId")?;
            let channel_id = required_config(step, "channelId")?;
            let message_text = required_config(step, "messageText")?;
            Ok((
                target,
                format!("{GRAPH_BASE_URL}/teams/{team_id}/channels/{channel_id}/messages"),
                json!({
                    "body": {
                        "contentType": "html",
                        "content": message_text
                    }
                }),
            ))
        }
        "outlook_send_mail" => {
            let user_id = required_config(step, "userId")?;
            let subject = required_config(step, "subject")?;
            let body_text = required_config(step, "bodyText")?;
            let recipients = recipients_from(step).ok_or_else(|| {
                ExecutionLedgerError::InvalidConfiguration {
                    name: "connector.ms_graph_notify.to".to_string(),
                }
            })?;
            if recipients.is_empty() {
                return Err(ExecutionLedgerError::InvalidConfiguration {
                    name: "connector.ms_graph_notify.to".to_string(),
                });
            }

            let body_content_type = config_or_input_string(step, "bodyContentType")
                .unwrap_or_else(|| "Text".to_string());
            let save_to_sent_items = step
                .config
                .get("saveToSentItems")
                .and_then(Value::as_bool)
                .or_else(|| step.input.get("saveToSentItems").and_then(Value::as_bool))
                .unwrap_or(false);

            Ok((
                target,
                format!("{GRAPH_BASE_URL}/users/{user_id}/sendMail"),
                json!({
                    "message": {
                        "subject": subject,
                        "body": {
                            "contentType": body_content_type,
                            "content": body_text
                        },
                        "toRecipients": recipients.iter().map(|address| {
                            json!({
                                "emailAddress": {
                                    "address": address
                                }
                            })
                        }).collect::<Vec<_>>()
                    },
                    "saveToSentItems": save_to_sent_items
                }),
            ))
        }
        _ => Err(ExecutionLedgerError::InvalidConfiguration {
            name: format!("connector.ms_graph_notify.target ('{target}')"),
        }),
    }
}

async fn graph_access_token() -> Result<String, ExecutionLedgerError> {
    let token = GRAPH_CREDENTIAL
        .get_token(&[GRAPH_SCOPE], None)
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
                "MSI token request returned an empty Graph access token",
            )),
        });
    }
    Ok(access_token.to_string())
}

pub(crate) async fn execute(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let (target, url, payload) = build_target_request(step)?;
    let timeout_ms = config_or_input_u64(step, "timeoutMs")
        .unwrap_or(30_000)
        .min(GRAPH_MAX_TIMEOUT_MS);
    let token = graph_access_token().await?;

    let response = GRAPH_CLIENT
        .post(&url)
        .timeout(Duration::from_millis(timeout_ms))
        .header("Authorization", ["Bearer ", token.as_str()].concat())
        .header("Content-Type", "application/json")
        .header("x-execution-ledger-correlation-id", ctx.correlation_id)
        .header("x-execution-ledger-job-id", ctx.job_id)
        .header("x-execution-ledger-event", ctx.event)
        .json(&payload)
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

    if !status.is_success() {
        return Err(ExecutionLedgerError::DependencyFailed {
            dependency: "microsoft_graph",
            source: Box::new(std::io::Error::other(format!(
                "ms_graph_notify request failed with status {status}: {text}"
            ))),
        });
    }

    let response_body = if text.trim().is_empty() {
        Value::Null
    } else {
        parse_json_or_text(&text)
    };

    Ok(json!({
        "stepId": step.id,
        "connector": "ms_graph_notify",
        "target": target,
        "url": url,
        "request": payload,
        "responseStatus": status.as_u16(),
        "response": response_body,
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

#[allow(clippy::unused_async)] // satisfies async connector interface; dispatched via .await in mod.rs
pub(crate) async fn compensate(
    _ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
    _original_output: &Value,
) -> Result<Value, ExecutionLedgerError> {
    Ok(json!({
        "stepId": step.id,
        "connector": "ms_graph_notify",
        "compensated": true,
        "action": "noop"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::ConnectorKind;
    use serde_json::json;

    fn test_step(config: Value, input: Value) -> ConnectorStep {
        ConnectorStep {
            id: "notify".to_string(),
            connector: ConnectorKind::MsGraphNotify,
            action: None,
            external_id: None,
            input,
            config,
            input_schema: None,
            output_schema: None,
        }
    }

    #[test]
    fn requires_target_when_url_not_provided() {
        let step = test_step(json!({}), json!({}));
        let result = build_target_request(&step);
        assert!(
            result.is_err(),
            "missing target should fail closed with validation error"
        );
        if let Err(err) = result {
            assert!(
                err.to_string().contains("connector.ms_graph_notify.target"),
                "missing target error should name the required key"
            );
        }
    }

    #[test]
    fn builds_teams_channel_payload() {
        let step = test_step(
            json!({
                "target": "teams_channel_message",
                "teamId": "team-1",
                "channelId": "channel-1",
                "messageText": "hello team"
            }),
            json!({}),
        );

        let result = build_target_request(&step);
        assert!(
            result.is_ok(),
            "valid teams configuration should build request"
        );
        if let Ok((target, url, payload)) = result {
            assert_eq!(target, "teams_channel_message");
            assert_eq!(
                url,
                "https://graph.microsoft.com/v1.0/teams/team-1/channels/channel-1/messages"
            );
            assert_eq!(
                payload,
                json!({
                    "body": {
                        "contentType": "html",
                        "content": "hello team"
                    }
                })
            );
        }
    }

    #[test]
    fn builds_outlook_send_mail_payload() {
        let step = test_step(
            json!({
                "target": "outlook_send_mail",
                "userId": "sender@contoso.com",
                "to": ["a@contoso.com", "b@contoso.com"],
                "subject": "Daily update",
                "bodyText": "Shipped",
                "bodyContentType": "Text",
                "saveToSentItems": false
            }),
            json!({}),
        );

        let result = build_target_request(&step);
        assert!(
            result.is_ok(),
            "valid send mail configuration should build request"
        );
        if let Ok((target, url, payload)) = result {
            assert_eq!(target, "outlook_send_mail");
            assert_eq!(
                url,
                "https://graph.microsoft.com/v1.0/users/sender@contoso.com/sendMail"
            );
            assert_eq!(payload["message"]["subject"], json!("Daily update"));
            assert_eq!(
                payload["message"]["toRecipients"][0]["emailAddress"]["address"],
                json!("a@contoso.com")
            );
            assert_eq!(payload["saveToSentItems"], json!(false));
        }
    }
}
