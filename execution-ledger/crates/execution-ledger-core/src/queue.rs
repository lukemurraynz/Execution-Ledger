use crate::error::{ExecutionLedgerError, Result};
use azure_core::credentials::TokenCredential;
use azure_identity::ManagedIdentityCredential;
use execution_ledger_types::{EnvelopeValidationError, ExecutionLedgerIntent};
use reqwest::StatusCode;
use sqlx::PgPool;
use std::sync::Arc;

const SERVICEBUS_CLIENT_TIMEOUT_SECONDS: u64 = 75;
const SERVICEBUS_RECEIVE_WAIT_SECONDS: u64 = 5;
const SERVICEBUS_DEAD_LETTER_DRAIN_WAIT_SECONDS: u64 = 2;
const SERVICEBUS_SCOPE: &str = "https://servicebus.azure.net/.default";

#[derive(Debug)]
pub struct ClaimedLocalIntent {
    pub queue_entry_id: i64,
    pub attempts: i32,
    pub intent: ExecutionLedgerIntent,
}

#[derive(Debug)]
pub struct ClaimedServiceBusIntent {
    pub lock_uri: String,
    pub delivery_count: i32,
    pub intent: ExecutionLedgerIntent,
}

#[derive(Debug)]
pub enum ServiceBusReceiveOutcome {
    NoMessage,
    Claimed(ClaimedServiceBusIntent),
    PoisonDeadLettered { delivery_count: i32, reason: String },
}

#[derive(Debug)]
pub enum ServiceBusRawReceiveOutcome {
    NoMessage,
    Received {
        lock_uri: String,
        delivery_count: i32,
        body: Vec<u8>,
    },
}

#[derive(Debug, Clone)]
pub struct ServiceBusQueueClient {
    endpoint: String,
    queue_name: String,
    client: reqwest::Client,
    credential: Arc<ManagedIdentityCredential>,
}

// F08 fix: replaced Box::leak with to_string() — no more permanent heap
// allocation on validation errors.
#[allow(clippy::needless_pass_by_value)] // map_err takes ownership
fn map_validation(error: EnvelopeValidationError) -> ExecutionLedgerError {
    ExecutionLedgerError::InvalidConfiguration {
        name: format!("ExecutionLedgerIntent: {error}"),
    }
}

fn map_servicebus_error(message: String) -> ExecutionLedgerError {
    ExecutionLedgerError::DependencyFailed {
        dependency: "servicebus",
        source: Box::new(std::io::Error::other(message)),
    }
}

impl ServiceBusQueueClient {
    pub fn from_managed_identity(namespace: &str, queue_name: &str) -> Result<Self> {
        let trimmed = namespace.trim();
        if trimmed.is_empty() {
            return Err(ExecutionLedgerError::InvalidConfiguration {
                name: "EXECUTION_LEDGER_SERVICEBUS_NAMESPACE".to_string(),
            });
        }
        let endpoint = if trimmed.starts_with("https://") {
            trimmed.trim_end_matches('/').to_string()
        } else if trimmed.starts_with("sb://") {
            trimmed.trim_end_matches('/').replace("sb://", "https://")
        } else {
            format!("https://{}", trimmed.trim_end_matches('/'))
        };
        Self::new(endpoint, queue_name)
    }

    fn new(endpoint: String, queue_name: &str) -> Result<Self> {
        let queue = queue_name.trim();
        if queue.is_empty() {
            return Err(ExecutionLedgerError::InvalidConfiguration {
                name: "EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME".to_string(),
            });
        }
        let credential = ManagedIdentityCredential::new(None).map_err(|source| {
            ExecutionLedgerError::DependencyFailed {
                dependency: "azure_identity",
                source: Box::new(source),
            }
        })?;
        Ok(Self {
            endpoint,
            queue_name: queue.to_string(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(
                    SERVICEBUS_CLIENT_TIMEOUT_SECONDS,
                ))
                .build()
                .map_err(|source| ExecutionLedgerError::DependencyFailed {
                    dependency: "reqwest",
                    source: Box::new(source),
                })?,
            credential,
        })
    }

    pub async fn enqueue_intent(&self, intent: &ExecutionLedgerIntent) -> Result<bool> {
        intent.validate().map_err(map_validation)?;
        let body = serde_json::to_vec(intent).map_err(|source| {
            ExecutionLedgerError::DependencyFailed {
                dependency: "serde_json",
                source: Box::new(source),
            }
        })?;
        self.enqueue_body(&body).await
    }

    pub async fn enqueue_json_message(&self, message: &serde_json::Value) -> Result<bool> {
        let body = serde_json::to_vec(message).map_err(|source| {
            ExecutionLedgerError::DependencyFailed {
                dependency: "serde_json",
                source: Box::new(source),
            }
        })?;
        self.enqueue_body(&body).await
    }

    async fn enqueue_body(&self, body: &[u8]) -> Result<bool> {
        let url = format!("{}/{}/messages", self.endpoint, self.queue_name);
        let token = self.authorization_header(&url).await?;
        let response = self
            .client
            .post(&url)
            .header("Authorization", token)
            .header("Content-Type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "reqwest",
                source: Box::new(source),
            })?;
        let status = response.status();
        if status.is_success() {
            return Ok(true);
        }
        let body = response.text().await.unwrap_or_default();
        Err(map_servicebus_error(format!(
            "enqueue failed with status {status}: {body}"
        )))
    }

    pub async fn receive_intent(&self) -> Result<ServiceBusReceiveOutcome> {
        match self.receive_raw_message().await? {
            ServiceBusRawReceiveOutcome::NoMessage => Ok(ServiceBusReceiveOutcome::NoMessage),
            ServiceBusRawReceiveOutcome::Received {
                lock_uri,
                delivery_count,
                body,
            } => match parse_servicebus_intent(&body) {
                Ok(intent) => Ok(ServiceBusReceiveOutcome::Claimed(ClaimedServiceBusIntent {
                    lock_uri,
                    delivery_count,
                    intent,
                })),
                Err(error) => {
                    let reason = error.to_string();
                    self.dead_letter_locked_message(
                        &lock_uri,
                        "InvalidExecutionLedgerIntent",
                        &reason,
                    )
                    .await?;
                    Ok(ServiceBusReceiveOutcome::PoisonDeadLettered {
                        delivery_count,
                        reason,
                    })
                }
            },
        }
    }

    /// Peek-lock receive with no assumption about the message body's shape —
    /// the caller is responsible for parsing and for settling the lock
    /// (`complete_locked_message`, `unlock_locked_message`, or
    /// `dead_letter_locked_message`). `receive_intent` is a thin wrapper over
    /// this that additionally parses and validates the body as an
    /// `ExecutionLedgerIntent`, for the internal worker's use.
    pub async fn receive_raw_message(&self) -> Result<ServiceBusRawReceiveOutcome> {
        let receive_resource_uri = format!("{}/{}/messages/head", self.endpoint, self.queue_name);
        let url = format!("{receive_resource_uri}?timeout={SERVICEBUS_RECEIVE_WAIT_SECONDS}");
        let token = self.authorization_header(&receive_resource_uri).await?;
        let response = self
            .client
            .post(url)
            .header("Authorization", token)
            .header("Content-Length", "0")
            .body("")
            .send()
            .await
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "reqwest",
                source: Box::new(source),
            })?;

        let status = response.status();
        if status == StatusCode::NO_CONTENT {
            return Ok(ServiceBusRawReceiveOutcome::NoMessage);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(map_servicebus_error(format!(
                "receive failed with status {status}: {body}"
            )));
        }

        let lock_uri = response
            .headers()
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .map(ToString::to_string)
            .ok_or_else(|| {
                map_servicebus_error("receive response missing lock Location header".to_string())
            })?;

        let delivery_count = response
            .headers()
            .get("BrokerProperties")
            .and_then(|v| v.to_str().ok())
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|v| v.get("DeliveryCount").and_then(serde_json::Value::as_i64))
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(1);

        let body =
            response
                .bytes()
                .await
                .map_err(|source| ExecutionLedgerError::DependencyFailed {
                    dependency: "reqwest",
                    source: Box::new(source),
                })?;

        Ok(ServiceBusRawReceiveOutcome::Received {
            lock_uri,
            delivery_count,
            body: body.to_vec(),
        })
    }

    pub async fn complete_locked_message(&self, lock_uri: &str) -> Result<()> {
        self.settle_locked_message(lock_uri, reqwest::Method::DELETE, "complete")
            .await
    }

    pub async fn unlock_locked_message(&self, lock_uri: &str) -> Result<()> {
        self.settle_locked_message(lock_uri, reqwest::Method::PUT, "unlock")
            .await
    }

    async fn settle_locked_message(
        &self,
        lock_uri: &str,
        method: reqwest::Method,
        operation: &'static str,
    ) -> Result<()> {
        let token = self.authorization_header(lock_uri).await?;
        let response = self
            .client
            .request(method, lock_uri)
            .header("Authorization", token)
            .send()
            .await
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "reqwest",
                source: Box::new(source),
            })?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(map_servicebus_error(format!(
            "{operation} failed with status {status}: {body}"
        )))
    }

    pub async fn dead_letter_locked_message(
        &self,
        lock_uri: &str,
        reason: &str,
        description: &str,
    ) -> Result<()> {
        let dead_letter_uri = format!("{lock_uri}/$deadletter");
        let token = self.authorization_header(&dead_letter_uri).await?;
        let broker_properties = serde_json::json!({
            "DeadLetterReason": reason,
            "DeadLetterErrorDescription": truncate_dead_letter_description(description),
        });
        let response = self
            .client
            .post(&dead_letter_uri)
            .header("Authorization", token)
            .header("BrokerProperties", broker_properties.to_string())
            .header("Content-Length", "0")
            .body("")
            .send()
            .await
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "reqwest",
                source: Box::new(source),
            })?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(map_servicebus_error(format!(
            "dead-letter failed with status {status}: {body}"
        )))
    }

    pub async fn drain_dead_letter_messages(&self, limit: usize) -> Result<u64> {
        if limit == 0 {
            return Ok(0);
        }

        let mut drained = 0_u64;
        for _ in 0..limit {
            let Some(lock_uri) = self.receive_dead_letter_lock_uri().await? else {
                break;
            };
            self.complete_locked_message(&lock_uri).await?;
            drained += 1;
        }
        Ok(drained)
    }

    async fn receive_dead_letter_lock_uri(&self) -> Result<Option<String>> {
        let dead_letter_resource_uri = format!(
            "{}/{}/$deadletterqueue/messages/head",
            self.endpoint, self.queue_name
        );
        let url = format!(
            "{dead_letter_resource_uri}?timeout={SERVICEBUS_DEAD_LETTER_DRAIN_WAIT_SECONDS}"
        );
        let token = self.authorization_header(&dead_letter_resource_uri).await?;
        let response = self
            .client
            .post(url)
            .header("Authorization", token)
            .header("Content-Length", "0")
            .body("")
            .send()
            .await
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "reqwest",
                source: Box::new(source),
            })?;
        let status = response.status();
        if status == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(map_servicebus_error(format!(
                "dead-letter receive failed with status {status}: {body}"
            )));
        }

        response
            .headers()
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .map(ToString::to_string)
            .map(Some)
            .ok_or_else(|| {
                map_servicebus_error(
                    "dead-letter receive response missing lock Location header".to_string(),
                )
            })
    }

    async fn authorization_header(&self, _resource_uri: &str) -> Result<String> {
        let token = self.managed_identity_access_token().await?;
        Ok(format!("Bearer {token}"))
    }

    async fn managed_identity_access_token(&self) -> Result<String> {
        let token = self
            .credential
            .get_token(&[SERVICEBUS_SCOPE], None)
            .await
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "azure_identity",
                source: Box::new(source),
            })?;
        let access_token = token.token.secret();
        if access_token.trim().is_empty() {
            return Err(map_servicebus_error(
                "managed identity token request returned an empty access token".to_string(),
            ));
        }
        Ok(access_token.to_string())
    }
}

fn parse_servicebus_intent(body: &[u8]) -> Result<ExecutionLedgerIntent> {
    let intent = serde_json::from_slice::<ExecutionLedgerIntent>(body).map_err(|source| {
        ExecutionLedgerError::DependencyFailed {
            dependency: "serde_json",
            source: Box::new(source),
        }
    })?;
    intent.validate().map_err(map_validation)?;
    Ok(intent)
}

fn truncate_dead_letter_description(description: &str) -> String {
    const MAX_DESCRIPTION_LEN: usize = 512;
    description.chars().take(MAX_DESCRIPTION_LEN).collect()
}

/// Enqueue one intent into the local Postgres queue.
///
/// Returns `true` when inserted, `false` when deduplicated by `intent_id`.
pub async fn enqueue_local_intent(
    pool: &PgPool,
    queue_name: &str,
    intent: &ExecutionLedgerIntent,
) -> Result<bool> {
    intent.validate().map_err(map_validation)?;

    let payload =
        serde_json::to_value(intent).map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "serde_json",
            source: Box::new(source),
        })?;

    let result = sqlx::query(
        "INSERT INTO EXECUTION_LEDGER_local_intent_queue (queue_name, intent_id, payload, attempts, available_at)
         VALUES ($1, $2, $3, 0, now())
         ON CONFLICT (intent_id) DO NOTHING",
    )
    .bind(queue_name)
    .bind(intent.id.to_string())
    .bind(payload)
    .execute(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;

    Ok(result.rows_affected() == 1)
}

/// Claims one available intent in FIFO order and acquires a short lease.
pub async fn claim_local_intent(
    pool: &PgPool,
    queue_name: &str,
) -> Result<Option<ClaimedLocalIntent>> {
    const LEASE_SECONDS: i64 = 30;

    let row = sqlx::query_as::<_, (i64, i32, serde_json::Value)>(
        "WITH next AS (
            SELECT id
            FROM EXECUTION_LEDGER_local_intent_queue
            WHERE queue_name = $1
              AND dead_lettered_at IS NULL
              AND available_at <= now()
              AND (lease_until IS NULL OR lease_until < now())
            ORDER BY id ASC
            FOR UPDATE SKIP LOCKED
            LIMIT 1
         )
         UPDATE EXECUTION_LEDGER_local_intent_queue q
         SET attempts = attempts + 1,
             lease_until = now() + make_interval(secs => $2)
         USING next
         WHERE q.id = next.id
         RETURNING q.id, q.attempts, q.payload",
    )
    .bind(queue_name)
    .bind(LEASE_SECONDS)
    .fetch_optional(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;

    let Some((queue_entry_id, attempts, payload)) = row else {
        return Ok(None);
    };

    let intent = serde_json::from_value::<ExecutionLedgerIntent>(payload).map_err(|source| {
        ExecutionLedgerError::DependencyFailed {
            dependency: "serde_json",
            source: Box::new(source),
        }
    })?;

    intent.validate().map_err(map_validation)?;
    Ok(Some(ClaimedLocalIntent {
        queue_entry_id,
        attempts,
        intent,
    }))
}

pub async fn ack_claimed_local_intent(pool: &PgPool, queue_entry_id: i64) -> Result<bool> {
    let result = sqlx::query("DELETE FROM EXECUTION_LEDGER_local_intent_queue WHERE id = $1")
        .bind(queue_entry_id)
        .execute(pool)
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        })?;
    Ok(result.rows_affected() == 1)
}

pub async fn fail_claimed_local_intent(
    pool: &PgPool,
    queue_entry_id: i64,
    attempts: i32,
    max_attempts: i32,
    backoff_seconds: i64,
    error: &str,
) -> Result<()> {
    if attempts >= max_attempts {
        let result = sqlx::query(
            "UPDATE EXECUTION_LEDGER_local_intent_queue
             SET lease_until = NULL,
                 dead_lettered_at = now(),
                 last_error = $2
             WHERE id = $1",
        )
        .bind(queue_entry_id)
        .bind(error)
        .execute(pool)
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        })?;
        if result.rows_affected() != 1 {
            return Err(ExecutionLedgerError::Conflict {
                resource: "EXECUTION_LEDGER_local_intent_queue",
            });
        }
        return Ok(());
    }

    let result = sqlx::query(
        "UPDATE EXECUTION_LEDGER_local_intent_queue
         SET lease_until = NULL,
             available_at = now() + make_interval(secs => $2),
             last_error = $3
         WHERE id = $1",
    )
    .bind(queue_entry_id)
    .bind(backoff_seconds)
    .bind(error)
    .execute(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;
    if result.rows_affected() != 1 {
        return Err(ExecutionLedgerError::Conflict {
            resource: "EXECUTION_LEDGER_local_intent_queue",
        });
    }
    Ok(())
}

/// Legacy helper kept for callers that still expect dequeue semantics.
pub async fn dequeue_local_intent(
    pool: &PgPool,
    queue_name: &str,
) -> Result<Option<ExecutionLedgerIntent>> {
    let Some(claimed) = claim_local_intent(pool, queue_name).await? else {
        return Ok(None);
    };
    let intent = claimed.intent.clone();
    let deleted = ack_claimed_local_intent(pool, claimed.queue_entry_id).await?;
    if !deleted {
        return Err(ExecutionLedgerError::Conflict {
            resource: "EXECUTION_LEDGER_local_intent_queue",
        });
    }
    Ok(Some(intent))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    #[test]
    fn parse_servicebus_intent_reports_deserialization_errors() {
        let result = parse_servicebus_intent(br#"{"not":"an-intent"}"#);
        assert!(matches!(
            result,
            Err(ExecutionLedgerError::DependencyFailed { .. })
        ));
    }

    #[test]
    fn parse_servicebus_intent_reports_validation_errors()
    -> std::result::Result<(), serde_json::Error> {
        let payload = serde_json::json!({
            "id": Uuid::nil(),
            "source": "api",
            "event": "target.invoked",
            "target": null,
            "data": {},
            "timestamp": Utc::now(),
            "traceId": null,
            "spanId": null,
            "parentSpanId": null
        });
        let body = serde_json::to_vec(&payload)?;
        let result = parse_servicebus_intent(&body);
        assert!(matches!(
            result,
            Err(ExecutionLedgerError::InvalidConfiguration { ref name })
                if name == "ExecutionLedgerIntent: target is required unless source is event_grid"
        ));
        Ok(())
    }

    #[test]
    fn truncates_dead_letter_description_to_service_bus_safe_length() {
        let description = "x".repeat(700);
        let truncated = truncate_dead_letter_description(&description);
        assert_eq!(truncated.len(), 512);
    }
}
