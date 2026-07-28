//! Service Bus intent worker: dequeues intents from Azure Service Bus, claims
//! messages with lock tokens, and processes them with bounded concurrency.

use std::sync::Arc;
use std::time::Duration;

use execution_ledger_core::{
    observability::{emit_runtime_event, log_counter},
    queue,
};
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Semaphore;

use crate::{
    MAX_INTENT_ATTEMPTS, RETRY_BACKOFF_MAX_SECONDS, mark_run_attempt_failure, process_intent,
};

pub(crate) async fn run_servicebus_intent_worker(
    pool: Arc<PgPool>,
    offload: Arc<execution_ledger_core::offload::PayloadOffload>,
    semaphore: Arc<Semaphore>,
    servicebus: Arc<queue::ServiceBusQueueClient>,
    runner_id: Arc<str>,
    environment: Arc<str>,
) -> execution_ledger_core::error::Result<()> {
    let mut consecutive_receive_failures: u32 = 0;
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            receive_result = servicebus.receive_intent() => {
                let claimed = match receive_result {
                    Ok(queue::ServiceBusReceiveOutcome::Claimed(claimed)) => {
                        consecutive_receive_failures = 0;
                        claimed
                    }
                    Ok(queue::ServiceBusReceiveOutcome::NoMessage) => continue,
                    Ok(queue::ServiceBusReceiveOutcome::PoisonDeadLettered {
                        delivery_count,
                        reason,
                    }) => {
                        consecutive_receive_failures = 0;
                        tracing::warn!(
                            delivery_count,
                            poison_reason = %reason,
                            "execution-ledger-worker: dead-lettered invalid Service Bus intent envelope"
                        );
                        emit_runtime_event(
                            "worker.intent.poisoned",
                            &json!({
                                "attempts": delivery_count,
                                "reason": reason,
                                "engine": "servicebus"
                            }),
                        );
                        log_counter(
                            "execution_ledger_worker_intents_total",
                            1,
                            &json!({"engine":"servicebus","outcome":"poison_dead_lettered"}),
                        );
                        continue;
                    }
                    Err(error) => {
                        consecutive_receive_failures += 1;
                        let delay_secs = (2u64
                            * 2u64.saturating_pow(consecutive_receive_failures - 1))
                            .min(RETRY_BACKOFF_MAX_SECONDS);
                        tracing::warn!(
                            %error,
                            delay_secs,
                            consecutive_failures = consecutive_receive_failures,
                            "execution-ledger-worker: service bus receive failed, retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                        continue;
                    }
                };

                let Ok(permit) = semaphore
                    .clone()
                    .acquire_owned()
                    .await
                else {
                    tracing::warn!("execution-ledger-worker: semaphore closed, shutting down service bus worker");
                    return Ok(());
                };
                let p = pool.clone();
                let o = offload.clone();
                let sb = servicebus.clone();
                let rid = runner_id.clone();
                let env = environment.clone();
                tasks.spawn(async move {
                    let _guard = permit;
                    process_and_complete_servicebus(p, o, sb, claimed, rid, env).await;
                });
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(join_err) = result {
                    tracing::error!(
                        %join_err,
                        "execution-ledger-worker: service bus intent task panicked"
                    );
                }
            }
        }
    }
}

async fn process_and_complete_servicebus(
    pool: Arc<PgPool>,
    offload: Arc<execution_ledger_core::offload::PayloadOffload>,
    servicebus: Arc<queue::ServiceBusQueueClient>,
    claimed: queue::ClaimedServiceBusIntent,
    runner_id: Arc<str>,
    environment: Arc<str>,
) {
    match process_intent(&pool, &claimed.intent, &offload, &runner_id, &environment).await {
        Ok(()) => {
            if let Err(error) = servicebus.complete_locked_message(&claimed.lock_uri).await {
                tracing::error!(
                    %error,
                    intent_id = %claimed.intent.id,
                    "execution-ledger-worker: ServiceBus complete failed after successful DB commit; \
                     message will be redelivered but step idempotency keys prevent duplicate side effects"
                );
            }
            emit_runtime_event(
                "worker.intent.completed",
                &json!({
                    "intentId": claimed.intent.id,
                    "attempts": claimed.delivery_count,
                    "engine": "servicebus"
                }),
            );
            log_counter(
                "execution_ledger_worker_intents_total",
                1,
                &json!({"engine":"servicebus","outcome":"completed"}),
            );
        }
        Err(error) => {
            let terminal_failure =
                mark_run_attempt_failure(&pool, claimed.intent.id, &runner_id, &error)
                    .await
                    .unwrap_or_default();
            if terminal_failure || claimed.delivery_count >= MAX_INTENT_ATTEMPTS {
                let dead_letter_updated = sqlx::query(
                    "UPDATE EXECUTION_LEDGER_jobs_runs
                     SET status = 'failed',
                         completed_at = now(),
                         updated_at = now(),
                         lease_owner = NULL,
                         lease_expires_at = NULL,
                         last_error = jsonb_build_object(
                             'message', $2,
                             'engine', 'servicebus',
                             'deliveryCount', $3
                         )
                     WHERE id = $1 AND status IN ('pending', 'running', 'failed')",
                )
                .bind(claimed.intent.id)
                .bind(error.to_string())
                .bind(claimed.delivery_count)
                .execute(&*pool)
                .await;

                match dead_letter_updated {
                    Ok(_) => {
                        let _ = servicebus.complete_locked_message(&claimed.lock_uri).await;
                    }
                    Err(dl_err) => {
                        tracing::error!(
                            %dl_err,
                            intent_id = %claimed.intent.id,
                            "execution-ledger-worker: dead-letter DB update failed; unlocking for retry"
                        );
                        let _ = servicebus.unlock_locked_message(&claimed.lock_uri).await;
                    }
                }
            } else {
                let _ = servicebus.unlock_locked_message(&claimed.lock_uri).await;
            }
            emit_runtime_event(
                "worker.intent.failed",
                &json!({
                    "intentId": claimed.intent.id,
                    "attempts": claimed.delivery_count,
                    "error": error.to_string(),
                    "engine": "servicebus"
                }),
            );
            log_counter(
                "execution_ledger_worker_intents_total",
                1,
                &json!({"engine":"servicebus","outcome":"failed"}),
            );
        }
    }
}
