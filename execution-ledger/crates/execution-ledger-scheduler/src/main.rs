//! `apps/scheduler` equivalent: a timer tick that emits job-launch intents.
//!
//! `apps/scheduler` today runs as an Azure Functions timer trigger; Azure
//! Durable Functions has no Rust support, so this is a plain `tokio::time`
//! interval loop instead of a hosted Functions binding. Deploy as a
//! long-running container/service rather than an Azure Functions custom
//! handler.

use execution_ledger_core::{
    ExecutionLedgerConfig, ExecutionLedgerIntent, ExecutionLedgerIntentSource, QueueEngine, db,
    queue,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

const TICK_INTERVAL: Duration = Duration::from_mins(1);
const LEASE_OWNER: &str = "scheduler:execution-ledger";
const INTENT_QUEUE_NAME: &str = "execution-ledger-intents";
const CLAIM_LIMIT: i64 = 20;
const LEASE_SECONDS: i64 = 600;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    execution_ledger_core::observability::init_runtime_telemetry("execution-ledger-scheduler");

    let config = ExecutionLedgerConfig::from_env_with_vault().await?;
    tracing::info!(environment = %config.environment, "execution-ledger-scheduler starting");
    let max_conn = std::env::var("EXECUTION_LEDGER_PG_MAX_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse().ok());
    let pool = db::connect_with_max_connections(&config.database_url, max_conn).await?;
    db::bootstrap_runtime_tables(&pool).await?;
    let servicebus_client = if matches!(config.queue_engine, QueueEngine::ServiceBus) {
        let queue_name = config.servicebus_queue_name.as_deref().ok_or(
            execution_ledger_core::ExecutionLedgerError::InvalidConfiguration {
                name: "EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME".to_string(),
            },
        )?;
        Some(queue::ServiceBusQueueClient::from_managed_identity(
            config.servicebus_namespace.as_deref().ok_or(
                execution_ledger_core::ExecutionLedgerError::InvalidConfiguration {
                    name: "EXECUTION_LEDGER_SERVICEBUS_NAMESPACE".to_string(),
                },
            )?,
            queue_name,
        )?)
    } else {
        None
    };

    let mut ticker = tokio::time::interval(TICK_INTERVAL);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(error) = run_tick(&pool, &config, servicebus_client.as_ref()).await {
                    tracing::error!(%error, "execution-ledger-scheduler: tick failed");
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("execution-ledger-scheduler shutting down");
                break;
            }
        }
    }

    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct ClaimedSchedule {
    scheduler_id: String,
    job_id: String,
    next_run: chrono::DateTime<chrono::Utc>,
    cron_expression: String,
    timezone: String,
    misfire_policy: String,
}

/// One scheduler tick: claim due schedules, enqueue one intent each, and
/// advance `next_run` using each schedule's cron/timezone and misfire policy.
async fn run_tick(
    pool: &PgPool,
    config: &ExecutionLedgerConfig,
    servicebus_client: Option<&queue::ServiceBusQueueClient>,
) -> execution_ledger_core::error::Result<()> {
    let (database_now,) = sqlx::query_as::<_, (chrono::DateTime<chrono::Utc>,)>("SELECT now()")
        .fetch_one(pool)
        .await
        .map_err(
            |source| execution_ledger_core::ExecutionLedgerError::DependencyFailed {
                dependency: "postgres",
                source: Box::new(source),
            },
        )?;

    let claimed = sqlx::query_as::<_, ClaimedSchedule>(
        "WITH candidates AS (
            SELECT scheduler_id, job_id, next_run, cron_expression, timezone, misfire_policy
            FROM EXECUTION_LEDGER_schedules
            WHERE status = 'ACTIVE'
              AND next_run <= $4
              AND (lease_until IS NULL OR lease_until < $4)
            ORDER BY next_run
            FOR UPDATE SKIP LOCKED
            LIMIT $1
        )
        UPDATE EXECUTION_LEDGER_schedules s
        SET lease_owner = $2,
            lease_until = $4 + make_interval(secs => $3),
            updated_at = $4
        FROM candidates c
        WHERE s.scheduler_id = c.scheduler_id
        RETURNING s.scheduler_id, s.job_id, c.next_run, c.cron_expression, c.timezone, c.misfire_policy",
    )
    .bind(CLAIM_LIMIT)
    .bind(LEASE_OWNER)
    .bind(LEASE_SECONDS)
    .bind(database_now)
    .fetch_all(pool)
    .await
    .map_err(|source| execution_ledger_core::ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;

    let mut enqueued_count = 0_i32;
    let mut carryover_count = 0_i32;

    for schedule in &claimed {
        let dedup = deterministic_id(&schedule.scheduler_id, schedule.next_run);
        let intent = ExecutionLedgerIntent {
            id: dedup,
            source: ExecutionLedgerIntentSource::Scheduler,
            event: "target.invoked".to_string(),
            target: Some(schedule.job_id.clone()),
            data: serde_json::json!({
                "scheduledFor": schedule.next_run.to_rfc3339(),
                "schedulerId": schedule.scheduler_id
            }),
            timestamp: chrono::Utc::now(),
            trace_id: None,
            span_id: None,
            parent_span_id: None,
        };

        match config.queue_engine {
            QueueEngine::Local => {
                let _ = queue::enqueue_local_intent(pool, INTENT_QUEUE_NAME, &intent).await?;
            }
            QueueEngine::ServiceBus => {
                let client = servicebus_client.ok_or(
                    execution_ledger_core::ExecutionLedgerError::InvalidConfiguration {
                        name: "EXECUTION_LEDGER_SERVICEBUS_NAMESPACE".to_string(),
                    },
                )?;
                let _ = client.enqueue_intent(&intent).await?;
            }
        }
        enqueued_count += 1;

        let next_run = match compute_next_run(schedule, database_now) {
            Ok(next_run) => next_run,
            Err(error) => {
                sqlx::query(
                    "UPDATE EXECUTION_LEDGER_schedules
                     SET retry_count = retry_count + 1,
                         last_error = $2,
                         lease_owner = NULL,
                         lease_until = NULL,
                         updated_at = $3
                     WHERE scheduler_id = $1",
                )
                .bind(&schedule.scheduler_id)
                .bind(error)
                .bind(database_now)
                .execute(pool)
                .await
                .map_err(|source| {
                    execution_ledger_core::ExecutionLedgerError::DependencyFailed {
                        dependency: "postgres",
                        source: Box::new(source),
                    }
                })?;
                continue;
            }
        };

        if next_run <= database_now {
            carryover_count += 1;
        }

        sqlx::query(
            "UPDATE EXECUTION_LEDGER_schedules
             SET next_run = $2,
                 last_run_at = $3,
                 last_queued_at = $4,
                 retry_count = 0,
                 last_error = NULL,
                 lease_owner = NULL,
                 lease_until = NULL,
                 updated_at = $4
             WHERE scheduler_id = $1",
        )
        .bind(&schedule.scheduler_id)
        .bind(next_run)
        .bind(schedule.next_run)
        .bind(database_now)
        .execute(pool)
        .await
        .map_err(|source| {
            execution_ledger_core::ExecutionLedgerError::DependencyFailed {
                dependency: "postgres",
                source: Box::new(source),
            }
        })?;
    }

    let tick_material = format!("{LEASE_OWNER}:{}", database_now.to_rfc3339());
    let tick_id = Uuid::new_v5(&Uuid::NAMESPACE_URL, tick_material.as_bytes()).to_string();
    sqlx::query(
        "INSERT INTO EXECUTION_LEDGER_scheduler_ticks
            (id, lease_owner, database_now, due_count, enqueued_count, carryover_count)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(tick_id)
    .bind(LEASE_OWNER)
    .bind(database_now)
    .bind(i32::try_from(claimed.len()).unwrap_or(i32::MAX))
    .bind(enqueued_count)
    .bind(carryover_count)
    .execute(pool)
    .await
    .map_err(
        |source| execution_ledger_core::ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        },
    )?;

    tracing::info!(
        claimed = claimed.len(),
        enqueued = enqueued_count,
        carryover = carryover_count,
        "execution-ledger-scheduler: tick"
    );
    execution_ledger_core::observability::emit_runtime_event(
        "scheduler.tick.completed",
        &serde_json::json!({
            "claimed": claimed.len(),
            "enqueued": enqueued_count,
            "carryover": carryover_count
        }),
    );
    execution_ledger_core::observability::log_counter(
        "execution_ledger_scheduler_ticks_total",
        1,
        &serde_json::json!({"outcome":"completed"}),
    );
    #[allow(clippy::cast_precision_loss)] // usize length → f64 for metrics; lossless up to 2^53
    execution_ledger_core::observability::log_gauge(
        "execution_ledger_scheduler_claimed",
        claimed.len() as f64,
        &serde_json::json!({}),
    );
    execution_ledger_core::observability::log_gauge(
        "execution_ledger_scheduler_enqueued",
        f64::from(enqueued_count),
        &serde_json::json!({}),
    );
    execution_ledger_core::observability::log_gauge(
        "execution_ledger_scheduler_carryover",
        f64::from(carryover_count),
        &serde_json::json!({}),
    );
    Ok(())
}

fn deterministic_id(scheduler_id: &str, planned_run: chrono::DateTime<chrono::Utc>) -> Uuid {
    let planned_run_iso = planned_run.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let material = format!("{scheduler_id}:{planned_run_iso}");
    let hash = Sha256::digest(material.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x0f) | 0xa0;
    Uuid::from_bytes(bytes)
}

fn compute_next_run(
    schedule: &ClaimedSchedule,
    database_now: chrono::DateTime<chrono::Utc>,
) -> std::result::Result<chrono::DateTime<chrono::Utc>, String> {
    let timezone: chrono_tz::Tz = schedule
        .timezone
        .parse()
        .map_err(|_| format!("invalid timezone: {}", schedule.timezone))?;
    let parsed = schedule
        .cron_expression
        .parse::<cron::Schedule>()
        .map_err(|_| format!("invalid cron expression: {}", schedule.cron_expression))?;

    let from = match schedule.misfire_policy.as_str() {
        "CATCH_UP" => schedule.next_run,
        "FIRE_ONCE" | "SKIP" => database_now,
        _ => {
            return Err(format!(
                "unsupported misfire policy: {}",
                schedule.misfire_policy
            ));
        }
    };

    let from_local = from.with_timezone(&timezone);
    parsed
        .after(&from_local)
        .next()
        .map(|next| next.with_timezone(&chrono::Utc))
        .ok_or_else(|| "no future cron occurrence".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    #[test]
    #[allow(clippy::expect_used)]
    fn deterministic_id_matches_scheduler_spec_vector() {
        let planned_run = DateTime::parse_from_rfc3339("2026-01-02T03:04:05.678Z")
            .expect("test timestamp is valid RFC3339")
            .with_timezone(&Utc);
        let id = deterministic_id("scheduler-a", planned_run);
        assert_eq!(id.to_string(), "142cd1a0-c500-42ef-a3f2-e25512db91b2");
    }
}
