//! Intent worker: dequeues intents from local or Service Bus queue and
//! executes connector steps with per-replica bounded concurrency.
//!
//! Drasi WAL reading has been split into a separate `execution-ledger-drasi`
//! service. This binary is now a pure intent processor.
//!
//! Queue engine implementations live in `engines/`.

mod engines;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use execution_ledger_core::{ExecutionLedgerConfig, QueueEngine, db, queue};
use execution_ledger_domain::connectors::{
    ConnectorExecutionContext, ConnectorKind, ConnectorStep,
};
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Semaphore;

use engines::{local, servicebus};

pub(crate) const INTENT_QUEUE_NAME: &str = "execution-ledger-intents";
pub(crate) const MAX_INTENT_ATTEMPTS: i32 = 5;
pub(crate) const RETRY_BACKOFF_SECONDS: i64 = 5;
pub(crate) const RETRY_BACKOFF_MAX_SECONDS: u64 = 60;
const RUN_LEASE_TTL_SECONDS: i64 = 300;
const RUN_HEARTBEAT_INTERVAL_SECONDS: u64 = 60;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    execution_ledger_core::observability::init_runtime_telemetry("execution-ledger-worker");

    let config = ExecutionLedgerConfig::from_env_with_vault().await?;

    let pool =
        db::connect_with_max_connections(&config.database_url, config.pg_max_connections).await?;
    db::bootstrap_runtime_tables(&pool).await?;
    db::ensure_default_platform_tenant(&pool).await?;
    db::ensure_default_apim_client(&pool).await?;

    let pool = Arc::new(pool);
    let offload = Arc::new(execution_ledger_core::offload::PayloadOffload::from_env()?);
    let runner_id = worker_runner_id();
    let environment = Arc::from(config.environment.as_str());
    let max_concurrent = config.worker_max_concurrent_intents.max(1);
    let semaphore = Arc::new(Semaphore::new(max_concurrent));

    tracing::info!(
        ?config.queue_engine,
        max_concurrent_intents = max_concurrent,
        %runner_id,
        %environment,
        "execution-ledger-worker: starting intent processor"
    );

    match config.queue_engine {
        QueueEngine::Local => {
            local::run_local_intent_worker(pool, offload, semaphore, runner_id, environment)
                .await?;
        }
        QueueEngine::ServiceBus => {
            let queue_name = config.servicebus_queue_name.as_deref().ok_or(
                execution_ledger_core::ExecutionLedgerError::InvalidConfiguration {
                    name: "EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME".to_string(),
                },
            )?;
            let client = queue::ServiceBusQueueClient::from_managed_identity(
                config.servicebus_namespace.as_deref().ok_or(
                    execution_ledger_core::ExecutionLedgerError::InvalidConfiguration {
                        name: "EXECUTION_LEDGER_SERVICEBUS_NAMESPACE".to_string(),
                    },
                )?,
                queue_name,
            )?;
            let client = Arc::new(client);
            servicebus::run_servicebus_intent_worker(
                pool,
                offload,
                semaphore,
                client,
                runner_id,
                environment,
            )
            .await?;
        }
    }

    Ok(())
}

// ── Shared intent processing ────────────────────────────────────────────────

pub(crate) async fn process_intent(
    pool: &PgPool,
    intent: &execution_ledger_core::ExecutionLedgerIntent,
    offload: &execution_ledger_core::offload::PayloadOffload,
    runner_id: &str,
    environment: &str,
) -> execution_ledger_core::error::Result<()> {
    let job_id = intent.target.as_deref().ok_or(
        execution_ledger_core::ExecutionLedgerError::InvalidConfiguration {
            name: "ExecutionLedgerIntent.target".to_string(),
        },
    )?;

    let mut tx = pool.begin().await.map_err(|source| {
        execution_ledger_core::ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        }
    })?;

    let maybe_run = sqlx::query_as::<_, (i32, serde_json::Value, Option<String>)>(
        "UPDATE EXECUTION_LEDGER_jobs_runs
         SET status = 'running',
             started_at = COALESCE(started_at, now()),
             updated_at = now(),
             lease_owner = $3,
             lease_expires_at = now() + make_interval(secs => $4)
         WHERE id = $1
           AND job_id = $2
           AND (
             status = 'pending'
             OR (
                 status = 'running'
                 AND (
                     lease_expires_at IS NULL
                     OR lease_expires_at < now()
                     OR lease_owner = $3
                 )
             )
           )
         RETURNING current_attempt, input_data, correlation_id",
    )
    .bind(intent.id)
    .bind(job_id)
    .bind(runner_id)
    .bind(RUN_LEASE_TTL_SECONDS)
    .fetch_optional(&mut *tx)
    .await
    .map_err(
        |source| execution_ledger_core::ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        },
    )?;

    let Some((attempt_number, input_data, existing_correlation_id)) = maybe_run else {
        return Err(execution_ledger_core::ExecutionLedgerError::Conflict {
            resource: "EXECUTION_LEDGER_jobs_runs",
        });
    };
    let heartbeat = start_run_heartbeat(pool.clone(), intent.id, runner_id.to_string());

    let correlation_id = existing_correlation_id
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            intent
                .data
                .get("correlationId")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| intent.id.to_string());

    // Compensation intent: undo completed steps in reverse order.
    // The normal execution path is skipped entirely — compensation
    // queries completed steps from the database and invokes each
    // connector's compensate() function.
    //
    // F12 fix: each step is compensated in its own short transaction
    // and committed immediately so partial progress survives a failure
    // in a later step. Previously all steps were marked 'compensated'
    // in a single uncommitted transaction — a mid-loop failure lost
    // those markers and left the operator with no visibility into
    // which steps had already been undone.
    if intent.event == "target.compensate" {
        return process_compensation_intent(
            pool,
            intent,
            job_id,
            &input_data,
            &correlation_id,
            environment,
            heartbeat,
            tx,
        )
        .await;
    }

    let replay_step_ids = parse_replay_step_ids(intent);
    let connector_steps = resolve_connector_steps(job_id, &input_data)?;
    let inline_output = if connector_steps.is_empty() {
        json!({
            "event": intent.event,
            "source": intent.source,
            "processedAt": chrono::Utc::now().to_rfc3339(),
            "correlationId": correlation_id,
            "data": input_data
        })
    } else {
        let mut step_results = Vec::with_capacity(connector_steps.len());
        let mut upstream_output: Option<serde_json::Value> = None;

        for (idx, step) in connector_steps.iter().enumerate() {
            let step_index = i32::try_from(idx).map_err(|_| {
                execution_ledger_core::ExecutionLedgerError::InvalidConfiguration {
                    name: "step.idx".to_string(),
                }
            })?;
            let step_key = build_step_idempotency_key(&intent.id.to_string(), step, step_index);
            let force_replay = !replay_step_ids.is_empty() && replay_step_ids.contains(&step.id);

            if !force_replay {
                let existing_output = sqlx::query_as::<_, (serde_json::Value,)>(
                    "SELECT output
                     FROM EXECUTION_LEDGER_steps_runs
                     WHERE job_run_id = $1 AND step_idempotency_key = $2 AND status = 'completed'
                     ORDER BY updated_at DESC
                     LIMIT 1",
                )
                .bind(intent.id)
                .bind(&step_key)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|source| {
                    execution_ledger_core::ExecutionLedgerError::DependencyFailed {
                        dependency: "postgres",
                        source: Box::new(source),
                    }
                })?;

                if let Some((cached_output,)) = existing_output {
                    step_results.push(json!({
                        "stepId": step.id,
                        "idempotencyKey": step_key,
                        "reused": true,
                        "output": cached_output
                    }));
                    continue;
                }
            }

            let step_context = ConnectorExecutionContext {
                run_id: intent.id,
                correlation_id: &correlation_id,
                job_id,
                event: &intent.event,
                environment,
                upstream_output: upstream_output.as_ref(),
            };
            let step_result =
                execution_ledger_domain::connectors::execute_step(&step_context, step).await;

            match step_result {
                Ok(output) => {
                    sqlx::query(
                        "INSERT INTO EXECUTION_LEDGER_steps_runs (
                            job_run_id, step_idempotency_key, step_id, external_id, correlation_id, status, output, attempt, idx
                         ) VALUES ($1, $2, $3, $4, $5, 'completed', $6, $7, $8)
                         ON CONFLICT (step_idempotency_key) WHERE step_idempotency_key IS NOT NULL
                         DO UPDATE SET status = EXCLUDED.status,
                                       output = EXCLUDED.output,
                                       error = NULL,
                                       updated_at = now()",
                    )
                    .bind(intent.id)
                    .bind(&step_key)
                    .bind(&step.id)
                    .bind(step.external_id.clone())
                    .bind(&correlation_id)
                    .bind(&output)
                    .bind(attempt_number)
                    .bind(step_index)
                    .execute(&mut *tx)
                    .await
                    .map_err(|source| {
                        execution_ledger_core::ExecutionLedgerError::DependencyFailed {
                            dependency: "postgres",
                            source: Box::new(source),
                        }
                    })?;
                    step_results.push(json!({
                        "stepId": step.id,
                        "idempotencyKey": step_key,
                        "reused": false,
                        "output": output
                    }));
                    upstream_output = Some(output);
                }
                Err(error) => {
                    // write directly to pool, not &mut *tx — the tx rolls back
                    // on this error return, which would erase the step record and hide
                    // which step failed from the API. Best-effort: don't mask the original error.
                    let _ = sqlx::query(
                        "INSERT INTO EXECUTION_LEDGER_steps_runs (
                            job_run_id, step_idempotency_key, step_id, external_id, correlation_id, status, error, attempt, idx
                         ) VALUES ($1, $2, $3, $4, $5, 'failed', $6, $7, $8)
                         ON CONFLICT (step_idempotency_key) WHERE step_idempotency_key IS NOT NULL
                         DO UPDATE SET status = EXCLUDED.status,
                                       error = EXCLUDED.error,
                                       updated_at = now()",
                    )
                    .bind(intent.id)
                    .bind(&step_key)
                    .bind(&step.id)
                    .bind(step.external_id.clone())
                    .bind(&correlation_id)
                    .bind(json!({ "message": error.to_string() }))
                    .bind(attempt_number)
                    .bind(step_index)
                    .execute(pool)
                    .await;
                    return Err(error);
                }
            }
        }

        json!({
            "event": intent.event,
            "source": intent.source,
            "processedAt": chrono::Utc::now().to_rfc3339(),
            "job": job_id,
            "correlationId": correlation_id,
            "steps": step_results
        })
    };
    let output = offload.store_if_needed(intent.id, &inline_output)?;

    sqlx::query(
        "INSERT INTO EXECUTION_LEDGER_job_attempts (
            job_run_id, attempt_number, status, output_data, started_at, completed_at, runner_id
         ) VALUES ($1, $2, 'completed', $3, now(), now(), $4)
         ON CONFLICT (job_run_id, attempt_number)
         DO UPDATE SET status = EXCLUDED.status,
                       output_data = EXCLUDED.output_data,
                       completed_at = EXCLUDED.completed_at,
                       updated_at = now()",
    )
    .bind(intent.id)
    .bind(attempt_number)
    .bind(&output)
    .bind(runner_id)
    .execute(&mut *tx)
    .await
    .map_err(
        |source| execution_ledger_core::ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        },
    )?;

    let completed_update = sqlx::query(
        "UPDATE EXECUTION_LEDGER_jobs_runs
         SET status = 'completed',
             state_data = $2,
             completed_at = now(),
             updated_at = now(),
             lease_owner = NULL,
             lease_expires_at = NULL,
             expires_at = COALESCE(expires_at, now() + interval '7 days')
         WHERE id = $1 AND lease_owner = $3",
    )
    .bind(intent.id)
    .bind(output)
    .bind(runner_id)
    .execute(&mut *tx)
    .await
    .map_err(
        |source| execution_ledger_core::ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        },
    )?;
    if completed_update.rows_affected() != 1 {
        return Err(execution_ledger_core::ExecutionLedgerError::Conflict {
            resource: "EXECUTION_LEDGER_jobs_runs",
        });
    }

    tx.commit().await.map_err(|source| {
        execution_ledger_core::ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        }
    })?;

    heartbeat.stop().await;

    Ok(())
}

/// Handle a `target.compensate` intent: undo completed steps in reverse
/// order using each connector's `compensate()` function.
///
/// The F12 fix: each step is compensated in its own short transaction
/// and committed immediately so partial progress survives a failure
/// in a later step.
#[allow(clippy::too_many_arguments)] // transaction-processing fn with many required params
async fn process_compensation_intent(
    pool: &PgPool,
    intent: &execution_ledger_core::ExecutionLedgerIntent,
    job_id: &str,
    input_data: &serde_json::Value,
    correlation_id: &str,
    environment: &str,
    heartbeat: RunHeartbeat,
    tx: sqlx::Transaction<'static, sqlx::Postgres>,
) -> execution_ledger_core::error::Result<()> {
    // Read-only lookup — no transaction needed.
    let completed_steps: Vec<(String, i32, serde_json::Value)> = sqlx::query_as(
        "SELECT step_id, idx, output
         FROM EXECUTION_LEDGER_steps_runs
         WHERE job_run_id = $1 AND status = 'completed'
         ORDER BY idx DESC",
    )
    .bind(intent.id)
    .fetch_all(pool)
    .await
    .map_err(
        |source| execution_ledger_core::ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        },
    )?;

    if !completed_steps.is_empty() {
        let step_defs = resolve_connector_steps(job_id, input_data)?;
        let step_def_map: std::collections::HashMap<&str, &ConnectorStep> =
            step_defs.iter().map(|s| (s.id.as_str(), s)).collect();
        for (step_id, _idx, original_output) in &completed_steps {
            let Some(step_def) = step_def_map.get(step_id.as_str()) else {
                continue;
            };
            let step_context = ConnectorExecutionContext {
                run_id: intent.id,
                correlation_id,
                job_id,
                event: &intent.event,
                environment,
                upstream_output: None, // compensation: no upstream step context
            };
            let compensate_result = execution_ledger_domain::connectors::compensate_step(
                &step_context,
                step_def,
                original_output,
            )
            .await;

            match compensate_result {
                Err(error) => {
                    let _ = sqlx::query(
                        "UPDATE EXECUTION_LEDGER_jobs_runs
                         SET status = 'compensation_failed',
                             last_error = jsonb_build_object('message', $2, 'failedStep', $3)
                         WHERE id = $1",
                    )
                    .bind(intent.id)
                    .bind(error.to_string())
                    .bind(step_id)
                    .execute(pool)
                    .await;
                    return Err(error);
                }
                Ok(_compensate_output) => {
                    // Commit each compensated step immediately so
                    // partial progress is durable and visible.
                    let mut step_tx = pool.begin().await.map_err(|source| {
                        execution_ledger_core::ExecutionLedgerError::DependencyFailed {
                            dependency: "postgres",
                            source: Box::new(source),
                        }
                    })?;
                    sqlx::query(
                        "UPDATE EXECUTION_LEDGER_steps_runs
                         SET status = 'compensated',
                             updated_at = now()
                         WHERE job_run_id = $1 AND step_id = $2",
                    )
                    .bind(intent.id)
                    .bind(step_id)
                    .execute(&mut *step_tx)
                    .await
                    .map_err(|source| {
                        execution_ledger_core::ExecutionLedgerError::DependencyFailed {
                            dependency: "postgres",
                            source: Box::new(source),
                        }
                    })?;
                    step_tx.commit().await.map_err(|source| {
                        execution_ledger_core::ExecutionLedgerError::DependencyFailed {
                            dependency: "postgres",
                            source: Box::new(source),
                        }
                    })?;
                }
            }
        }
    }

    // Roll back the outer transaction (only used for the initial
    // run claim SELECT … FOR UPDATE) and use a fresh one for the
    // final run status update to keep the write path clean.
    tx.rollback().await.map_err(|source| {
        execution_ledger_core::ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        }
    })?;

    let mut final_tx = pool.begin().await.map_err(|source| {
        execution_ledger_core::ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        }
    })?;
    sqlx::query(
        "UPDATE EXECUTION_LEDGER_jobs_runs
         SET status = 'compensated',
             completed_at = now(),
             updated_at = now(),
             lease_owner = NULL,
             lease_expires_at = NULL
         WHERE id = $1",
    )
    .bind(intent.id)
    .execute(&mut *final_tx)
    .await
    .map_err(
        |source| execution_ledger_core::ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        },
    )?;
    final_tx.commit().await.map_err(|source| {
        execution_ledger_core::ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        }
    })?;
    heartbeat.stop().await;
    execution_ledger_core::observability::emit_runtime_event(
        "worker.compensation.completed",
        &json!({
            "runId": intent.id,
            "jobId": job_id,
            "correlationId": correlation_id
        }),
    );
    execution_ledger_core::observability::log_counter(
        "execution_ledger_worker_compensations_total",
        1,
        &json!({"outcome":"completed"}),
    );
    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────────────

struct RunHeartbeat {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl RunHeartbeat {
    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for RunHeartbeat {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn start_run_heartbeat(pool: PgPool, run_id: sqlx::types::Uuid, runner_id: String) -> RunHeartbeat {
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(RUN_HEARTBEAT_INTERVAL_SECONDS));
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                _ = ticker.tick() => {
                    let heartbeat_update = sqlx::query(
                        "UPDATE EXECUTION_LEDGER_jobs_runs
                         SET updated_at = now(),
                             lease_expires_at = now() + make_interval(secs => $3)
                         WHERE id = $1
                           AND lease_owner = $2
                           AND status = 'running'",
                    )
                    .bind(run_id)
                    .bind(&runner_id)
                    .bind(RUN_LEASE_TTL_SECONDS)
                    .execute(&pool)
                    .await;
                    match heartbeat_update {
                        Ok(result) if result.rows_affected() == 1 => {}
                        Ok(_) => {
                            tracing::warn!(
                                %run_id,
                                lease_owner = %runner_id,
                                "execution-ledger-worker: heartbeat lost lease ownership"
                            );
                            break;
                        }
                        Err(error) => {
                            tracing::error!(
                                %error,
                                %run_id,
                                lease_owner = %runner_id,
                                "execution-ledger-worker: heartbeat update failed"
                            );
                            break;
                        }
                    }
                }
            }
        }
    });
    RunHeartbeat {
        stop: Some(stop_tx),
        task: Some(task),
    }
}

pub(crate) async fn mark_run_attempt_failure(
    pool: &PgPool,
    run_id: sqlx::types::Uuid,
    runner_id: &str,
    error: &execution_ledger_core::ExecutionLedgerError,
) -> execution_ledger_core::error::Result<bool> {
    let error_message = error.to_string();
    let updated_status = sqlx::query_as::<_, (String,)>(
        "UPDATE EXECUTION_LEDGER_jobs_runs
         SET current_attempt = current_attempt + 1,
             status = CASE
                        WHEN (current_attempt + 1) > max_attempts THEN 'failed'
                        ELSE 'pending'
                      END,
             completed_at = CASE
                              WHEN (current_attempt + 1) > max_attempts THEN now()
                              ELSE NULL
                            END,
             updated_at = now(),
             lease_owner = NULL,
             lease_expires_at = NULL,
             last_error = jsonb_build_object(
                 'message', $3,
                 'runner', $2
             )
         WHERE id = $1
           AND lease_owner = $2
           AND status = 'running'
         RETURNING status",
    )
    .bind(run_id)
    .bind(runner_id)
    .bind(&error_message)
    .fetch_optional(pool)
    .await
    .map_err(
        |source| execution_ledger_core::ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        },
    )?;

    let (status,) = if let Some(status) = updated_status {
        status
    } else {
        let fallback_status = sqlx::query_as::<_, (String,)>(
           "UPDATE EXECUTION_LEDGER_jobs_runs
            SET current_attempt = current_attempt + 1,
                status = CASE
                           WHEN (current_attempt + 1) > max_attempts THEN 'failed'
                           ELSE 'pending'
                         END,
                completed_at = CASE
                                 WHEN (current_attempt + 1) > max_attempts THEN now()
                                 ELSE NULL
                               END,
                updated_at = now(),
                lease_owner = NULL,
                lease_expires_at = NULL,
                last_error = jsonb_build_object(
                    'message', $3,
                    'runner', $2
                )
            WHERE id = $1
              AND status = 'pending'
            RETURNING status",
        )
        .bind(run_id)
        .bind(runner_id)
        .bind(&error_message)
        .fetch_optional(pool)
        .await
        .map_err(
           |source| execution_ledger_core::ExecutionLedgerError::DependencyFailed {
               dependency: "postgres",
               source: Box::new(source),
           },
        )?;

        fallback_status.ok_or(execution_ledger_core::ExecutionLedgerError::Conflict {
           resource: "EXECUTION_LEDGER_jobs_runs",
        })?
    };
    Ok(status == "failed")
}

fn parse_replay_step_ids(intent: &execution_ledger_core::ExecutionLedgerIntent) -> HashSet<String> {
    if intent.event != "target.replay" {
        return HashSet::new();
    }
    intent
        .data
        .get("replay")
        .and_then(|value| value.get("stepIds"))
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(ToString::to_string)
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default()
}

fn resolve_connector_steps(
    job_id: &str,
    input_data: &serde_json::Value,
) -> execution_ledger_core::error::Result<Vec<ConnectorStep>> {
    if let Some(steps) = input_data
        .get("steps")
        .and_then(serde_json::Value::as_array)
    {
        return steps
            .iter()
            .cloned()
            .map(|value| {
                serde_json::from_value::<ConnectorStep>(value).map_err(|source| {
                    execution_ledger_core::ExecutionLedgerError::DependencyFailed {
                        dependency: "serde_json",
                        source: Box::new(source),
                    }
                })
            })
            .collect();
    }

    if job_id == "platform.health" {
        return Ok(vec![ConnectorStep {
            id: "platform.health".to_string(),
            connector: ConnectorKind::PlatformHealth,
            action: None,
            external_id: None,
            input: serde_json::Value::Null,
            config: serde_json::Value::Null,
            input_schema: None,
            output_schema: None,
        }]);
    }

    Ok(Vec::new())
}

fn build_step_idempotency_key(run_id: &str, step: &ConnectorStep, step_index: i32) -> String {
    format!(
        "{run_id}:{}:{}:{}",
        step.id,
        step.external_id.as_deref().unwrap_or_default(),
        step_index
    )
}

fn worker_runner_id() -> Arc<str> {
    let configured = std::env::var("EXECUTION_LEDGER_RUNNER_ID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let container_app_replica_name = std::env::var("CONTAINER_APP_REPLICA_NAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    configured
        .or(container_app_replica_name)
        .or(hostname)
        .unwrap_or_else(|| {
            format!(
                "execution-ledger-worker-{}-{}",
                std::process::id(),
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
            )
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── parse_replay_step_ids ───────────────────────────────────────────

    #[test]
    fn replay_parses_explicit_step_ids() {
        let intent = execution_ledger_core::ExecutionLedgerIntent {
            id: sqlx::types::Uuid::nil(),
            source: execution_ledger_core::ExecutionLedgerIntentSource::Api,
            event: "target.replay".to_string(),
            target: Some("my-job".to_string()),
            data: json!({"replay": {"stepIds": ["step-a", "step-c"]}}),
            timestamp: chrono::Utc::now(),
            trace_id: None,
            span_id: None,
            parent_span_id: None,
        };
        let ids = parse_replay_step_ids(&intent);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("step-a"));
        assert!(ids.contains("step-c"));
    }

    #[test]
    fn replay_non_replay_event_returns_empty() {
        let intent = execution_ledger_core::ExecutionLedgerIntent {
            id: sqlx::types::Uuid::nil(),
            source: execution_ledger_core::ExecutionLedgerIntentSource::Api,
            event: "target.invoked".to_string(),
            target: Some("my-job".to_string()),
            data: json!({"replay": {"stepIds": ["step-a"]}}),
            timestamp: chrono::Utc::now(),
            trace_id: None,
            span_id: None,
            parent_span_id: None,
        };
        let ids = parse_replay_step_ids(&intent);
        assert!(ids.is_empty());
    }

    #[test]
    fn replay_missing_replay_key_returns_empty() {
        let intent = execution_ledger_core::ExecutionLedgerIntent {
            id: sqlx::types::Uuid::nil(),
            source: execution_ledger_core::ExecutionLedgerIntentSource::Api,
            event: "target.replay".to_string(),
            target: Some("my-job".to_string()),
            data: json!({}),
            timestamp: chrono::Utc::now(),
            trace_id: None,
            span_id: None,
            parent_span_id: None,
        };
        let ids = parse_replay_step_ids(&intent);
        assert!(ids.is_empty());
    }

    // ── resolve_connector_steps ─────────────────────────────────────────

    #[test]
    fn resolves_steps_from_input_array() -> execution_ledger_core::error::Result<()> {
        let input = json!({
            "steps": [
                {"id": "health", "connector": "platform_health", "input": {}},
                {"id": "call-api", "connector": "rest", "input": {}}
            ]
        });
        let steps = resolve_connector_steps("any-job", &input)?;
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].id, "health");
        assert_eq!(steps[1].id, "call-api");
        Ok(())
    }

    #[test]
    fn resolves_platform_health_fallback() -> execution_ledger_core::error::Result<()> {
        let steps = resolve_connector_steps("platform.health", &json!({}))?;
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].id, "platform.health");
        assert!(matches!(steps[0].connector, ConnectorKind::PlatformHealth));
        Ok(())
    }

    #[test]
    fn resolves_empty_for_unknown_job_without_steps() -> execution_ledger_core::error::Result<()> {
        let steps = resolve_connector_steps("unknown-job", &json!({}))?;
        assert!(steps.is_empty());
        Ok(())
    }

    #[test]
    fn rejects_invalid_step_json() {
        let input = json!({"steps": [{"not-a-valid-step": true}]});
        let result = resolve_connector_steps("any-job", &input);
        assert!(result.is_err());
    }

    // ── build_step_idempotency_key ──────────────────────────────────────

    #[test]
    fn idempotency_key_format_is_stable() {
        let step = ConnectorStep {
            id: "upload".to_string(),
            connector: ConnectorKind::AzureBlob,
            action: None,
            external_id: Some("file-42".to_string()),
            input: json!({}),
            config: json!({}),
            input_schema: None,
            output_schema: None,
        };
        let key = build_step_idempotency_key("run-abc", &step, 3);
        assert_eq!(key, "run-abc:upload:file-42:3");
    }

    #[test]
    fn idempotency_key_handles_missing_external_id() {
        let step = ConnectorStep {
            id: "health".to_string(),
            connector: ConnectorKind::PlatformHealth,
            action: None,
            external_id: None,
            input: json!({}),
            config: json!({}),
            input_schema: None,
            output_schema: None,
        };
        let key = build_step_idempotency_key("run-xyz", &step, 0);
        assert_eq!(key, "run-xyz:health::0");
    }
}
