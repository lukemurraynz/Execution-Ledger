//! Local intent worker: polls the Postgres-backed local queue, claims intents
//! via `SELECT … FOR UPDATE SKIP LOCKED`, and processes them with bounded concurrency.

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
    INTENT_QUEUE_NAME, MAX_INTENT_ATTEMPTS, RETRY_BACKOFF_SECONDS, mark_run_attempt_failure,
    process_intent,
};

pub(crate) async fn run_local_intent_worker(
    pool: Arc<PgPool>,
    offload: Arc<execution_ledger_core::offload::PayloadOffload>,
    semaphore: Arc<Semaphore>,
    runner_id: Arc<str>,
    environment: Arc<str>,
) -> execution_ledger_core::error::Result<()> {
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let Some(claimed) = queue::claim_local_intent(&pool, INTENT_QUEUE_NAME).await? else {
                    continue;
                };
                let Ok(permit) = semaphore
                    .clone()
                    .acquire_owned()
                    .await
                else {
                    tracing::warn!("execution-ledger-worker: semaphore closed, shutting down local worker");
                    return Ok(());
                };
                let p = pool.clone();
                let o = offload.clone();
                let rid = runner_id.clone();
                let env = environment.clone();
                tasks.spawn(async move {
                    let _guard = permit;
                    process_and_complete_local(p, o, claimed, rid, env).await;
                });
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(join_err) = result {
                    tracing::error!(
                        %join_err,
                        "execution-ledger-worker: local intent task panicked"
                    );
                }
            }
        }
    }
}

async fn process_and_complete_local(
    pool: Arc<PgPool>,
    offload: Arc<execution_ledger_core::offload::PayloadOffload>,
    claimed: queue::ClaimedLocalIntent,
    runner_id: Arc<str>,
    environment: Arc<str>,
) {
    match process_intent(&pool, &claimed.intent, &offload, &runner_id, &environment).await {
        Ok(()) => {
            let _ = queue::ack_claimed_local_intent(&pool, claimed.queue_entry_id).await;
            emit_runtime_event(
                "worker.intent.completed",
                &json!({
                    "queueEntryId": claimed.queue_entry_id,
                    "intentId": claimed.intent.id,
                    "attempts": claimed.attempts
                }),
            );
            log_counter(
                "execution_ledger_worker_intents_total",
                1,
                &json!({"engine":"local","outcome":"completed"}),
            );
        }
        Err(error) => {
            let _ = mark_run_attempt_failure(&pool, claimed.intent.id, &runner_id, &error).await;
            let _ = queue::fail_claimed_local_intent(
                &pool,
                claimed.queue_entry_id,
                claimed.attempts,
                MAX_INTENT_ATTEMPTS,
                RETRY_BACKOFF_SECONDS,
                &error.to_string(),
            )
            .await;
            emit_runtime_event(
                "worker.intent.failed",
                &json!({
                    "queueEntryId": claimed.queue_entry_id,
                    "intentId": claimed.intent.id,
                    "attempts": claimed.attempts,
                    "error": error.to_string()
                }),
            );
            log_counter(
                "execution_ledger_worker_intents_total",
                1,
                &json!({"engine":"local","outcome":"failed"}),
            );
        }
    }
}
