//! Runtime database primitives shared by API, worker, and scheduler.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, migrate::Migrator};
use uuid::Uuid;

use crate::error::{ExecutionLedgerError, Result};
use crate::offload::cleanup_offload_files;

static RUNTIME_MIGRATOR: Migrator = sqlx::migrate!("./migrations");
const DEFAULT_APIM_CLIENT_ID: &str = "execution-ledger-apim";
const DEFAULT_APIM_CLIENT_DISPLAY_NAME: &str = "Execution Ledger APIM gateway";
const DEFAULT_PLATFORM_TENANT_ID: &str = "00000000-0000-0000-0000-000000000001";
const DEFAULT_PLATFORM_TENANT_NAME: &str = "Execution Ledger Platform Tenant";

/// Opens the shared connection pool with default settings.
///
/// # Errors
/// Returns [`ExecutionLedgerError::DependencyFailed`] when Postgres is unreachable.
pub async fn connect(database_url: &str) -> Result<PgPool> {
    connect_with_max_connections(database_url, None).await
}

/// Opens the shared connection pool with an explicit max connection limit.
///
/// Pass `None` to use the sqlx default (10 connections).
/// At scale, set this to stay within the `PostgreSQL` Flexible Server
/// `max_connections` limit for your SKU.
///
/// # Errors
/// Returns [`ExecutionLedgerError::DependencyFailed`] when Postgres is unreachable.
pub async fn connect_with_max_connections(
    database_url: &str,
    max_connections: Option<u32>,
) -> Result<PgPool> {
    let mut options = PgPoolOptions::new();
    if let Some(max) = max_connections {
        options = options.max_connections(max);
    }
    options
        .connect(database_url)
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        })
}

/// Applies versioned runtime schema migrations required for
/// full-capability Execution Ledger delivery.
///
/// # Errors
/// Returns [`ExecutionLedgerError::DependencyFailed`] when migration execution fails.
pub async fn bootstrap_runtime_tables(pool: &PgPool) -> Result<()> {
    RUNTIME_MIGRATOR
        .run(pool)
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        })?;
    Ok(())
}

/// Ensures the default APIM client principal used for operational probes and
/// gateway calls is present and active.
///
/// # Errors
/// Returns [`ExecutionLedgerError::DependencyFailed`] when Postgres rejects the upsert.
pub async fn ensure_default_apim_client(pool: &PgPool) -> Result<()> {
    sqlx::query(
        "INSERT INTO EXECUTION_LEDGER_api_clients (client_id, display_name, status, scopes)
         VALUES ($1, $2, 'active', jsonb_build_array('platform:read', 'platform:write', 'mcp:execute'))
         ON CONFLICT (client_id) DO UPDATE
         SET display_name = EXCLUDED.display_name,
             status = 'active',
             scopes = EXCLUDED.scopes,
             updated_at = now()",
    )
    .bind(DEFAULT_APIM_CLIENT_ID)
    .bind(DEFAULT_APIM_CLIENT_DISPLAY_NAME)
    .execute(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;
    Ok(())
}

/// Ensures the platform has at least one active tenant so APIM-only platform
/// operations (runbooks/probes) can resolve tenant-scoped routes.
///
/// # Errors
/// Returns [`ExecutionLedgerError::DependencyFailed`] when Postgres rejects the upsert.
pub async fn ensure_default_platform_tenant(pool: &PgPool) -> Result<()> {
    sqlx::query(
        "INSERT INTO EXECUTION_LEDGER_tenants (id, name, status)
         VALUES ($1::uuid, $2, 'active')
         ON CONFLICT (id) DO UPDATE
         SET name = EXCLUDED.name,
             status = 'active'",
    )
    .bind(DEFAULT_PLATFORM_TENANT_ID)
    .bind(DEFAULT_PLATFORM_TENANT_NAME)
    .execute(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;
    Ok(())
}

/// Ensures a tenant row exists and is active.
///
/// # Errors
/// Returns [`ExecutionLedgerError::DependencyFailed`] when Postgres rejects the upsert.
pub async fn ensure_tenant_active(pool: &PgPool, tenant_id: Uuid, tenant_name: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO EXECUTION_LEDGER_tenants (id, name, status)
         VALUES ($1::uuid, $2, 'active')
         ON CONFLICT (id) DO UPDATE
         SET name = EXCLUDED.name,
             status = 'active'",
    )
    .bind(tenant_id)
    .bind(tenant_name)
    .execute(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;
    Ok(())
}

/// Whether `tenant_id` is an onboarded, active tenant — the real
/// multi-tenancy authorization gate. A validly-signed Entra token alone is
/// not enough; the tenant must also have a row here with `status =
/// 'active'`, which is how an org is onboarded or suspended without a code
/// change.
///
/// # Errors
/// Returns [`ExecutionLedgerError::DependencyFailed`] when the query fails.
pub async fn is_tenant_active(pool: &PgPool, tenant_id: Uuid) -> Result<bool> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT status FROM EXECUTION_LEDGER_tenants WHERE id = $1")
            .bind(tenant_id)
            .fetch_optional(pool)
            .await
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "postgres",
                source: Box::new(source),
            })?;

    Ok(matches!(row, Some((status,)) if status == "active"))
}

/// Fails fast with a clear, actionable error if the server is not
/// configured for logical replication, instead of letting the replication
/// source fail later with an opaque protocol error.
///
/// # Errors
/// Returns [`ExecutionLedgerError::InvalidConfiguration`] when `wal_level != logical`.
pub async fn assert_logical_replication_enabled(pool: &PgPool) -> Result<()> {
    let wal_level: (String,) = sqlx::query_as("SHOW wal_level")
        .fetch_one(pool)
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        })?;

    if wal_level.0 != "logical" {
        return Err(ExecutionLedgerError::InvalidConfiguration {
            name: "postgresql.conf: wal_level (must be 'logical' for the drasi-source-postgres replication slot; requires a Postgres restart to change)".to_string(),
        });
    }

    Ok(())
}

#[derive(Debug, Clone)]
pub struct KvRecord {
    pub key: String,
    pub value_json: Value,
    pub version: i32,
    pub expires_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

pub async fn kv_get(pool: &PgPool, key: &str) -> Result<Option<KvRecord>> {
    let row = sqlx::query_as::<_, (String, Value, i32, Option<DateTime<Utc>>, DateTime<Utc>)>(
        "SELECT key, value_json, version, expires_at, updated_at
         FROM EXECUTION_LEDGER_kv
         WHERE key = $1 AND (expires_at IS NULL OR expires_at > now())",
    )
    .bind(key)
    .fetch_optional(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;

    Ok(row.map(
        |(key, value_json, version, expires_at, updated_at)| KvRecord {
            key,
            value_json,
            version,
            expires_at,
            updated_at,
        },
    ))
}

pub async fn kv_set(
    pool: &PgPool,
    key: &str,
    value_json: &Value,
    expected_version: Option<i32>,
    ttl_seconds: Option<i64>,
) -> Result<KvRecord> {
    let serialized_len = serde_json::to_vec(value_json)
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "serde_json",
            source: Box::new(source),
        })?
        .len();
    if serialized_len > 5_120 {
        return Err(ExecutionLedgerError::PayloadTooLarge {
            resource: "EXECUTION_LEDGER_kv.value_json",
        });
    }

    let expires_at = ttl_seconds.map(|seconds| Utc::now() + chrono::TimeDelta::seconds(seconds));

    if let Some(version) = expected_version {
        if version == 0 {
            let row =
                sqlx::query_as::<_, (String, Value, i32, Option<DateTime<Utc>>, DateTime<Utc>)>(
                    "INSERT INTO EXECUTION_LEDGER_kv (key, value_json, version, expires_at)
                 VALUES ($1, $2, 1, $3)
                 ON CONFLICT DO NOTHING
                 RETURNING key, value_json, version, expires_at, updated_at",
                )
                .bind(key)
                .bind(value_json)
                .bind(expires_at)
                .fetch_optional(pool)
                .await
                .map_err(|source| ExecutionLedgerError::DependencyFailed {
                    dependency: "postgres",
                    source: Box::new(source),
                })?;
            return row
                .map(
                    |(key, value_json, version, expires_at, updated_at)| KvRecord {
                        key,
                        value_json,
                        version,
                        expires_at,
                        updated_at,
                    },
                )
                .ok_or(ExecutionLedgerError::Conflict {
                    resource: "EXECUTION_LEDGER_kv",
                });
        }

        let row = sqlx::query_as::<_, (String, Value, i32, Option<DateTime<Utc>>, DateTime<Utc>)>(
            "UPDATE EXECUTION_LEDGER_kv
             SET value_json = $2, version = version + 1, expires_at = $3, updated_at = now()
             WHERE key = $1 AND version = $4
             RETURNING key, value_json, version, expires_at, updated_at",
        )
        .bind(key)
        .bind(value_json)
        .bind(expires_at)
        .bind(version)
        .fetch_optional(pool)
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        })?;
        return row
            .map(
                |(key, value_json, version, expires_at, updated_at)| KvRecord {
                    key,
                    value_json,
                    version,
                    expires_at,
                    updated_at,
                },
            )
            .ok_or(ExecutionLedgerError::Conflict {
                resource: "EXECUTION_LEDGER_kv",
            });
    }

    let row = sqlx::query_as::<_, (String, Value, i32, Option<DateTime<Utc>>, DateTime<Utc>)>(
        "INSERT INTO EXECUTION_LEDGER_kv (key, value_json, version, expires_at)
         VALUES ($1, $2, 1, $3)
         ON CONFLICT (key) DO UPDATE
             SET value_json = EXCLUDED.value_json,
                 version = EXECUTION_LEDGER_kv.version + 1,
                 expires_at = EXCLUDED.expires_at,
                 updated_at = now()
         RETURNING key, value_json, version, expires_at, updated_at",
    )
    .bind(key)
    .bind(value_json)
    .bind(expires_at)
    .fetch_one(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;

    Ok(KvRecord {
        key: row.0,
        value_json: row.1,
        version: row.2,
        expires_at: row.3,
        updated_at: row.4,
    })
}

pub async fn kv_delete(pool: &PgPool, key: &str) -> Result<bool> {
    let result = sqlx::query("DELETE FROM EXECUTION_LEDGER_kv WHERE key = $1")
        .bind(key)
        .execute(pool)
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        })?;
    Ok(result.rows_affected() > 0)
}

pub async fn kv_cleanup_expired(pool: &PgPool, limit: i64) -> Result<u64> {
    let result = sqlx::query(
        "WITH doomed AS (
            SELECT key
            FROM EXECUTION_LEDGER_kv
            WHERE expires_at IS NOT NULL AND expires_at <= now()
            ORDER BY expires_at
            LIMIT $1
          )
          DELETE FROM EXECUTION_LEDGER_kv
          WHERE key IN (SELECT key FROM doomed)",
    )
    .bind(limit)
    .execute(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;
    Ok(result.rows_affected())
}

pub async fn gc_cleanup_terminal_runs(pool: &PgPool, limit: i64) -> Result<u64> {
    let result = sqlx::query(
        "WITH doomed AS (
            SELECT id
            FROM EXECUTION_LEDGER_jobs_runs
            WHERE expires_at IS NOT NULL
              AND expires_at <= now()
           AND status IN ('completed', 'compensated', 'failed', 'cancelled', 'compensation_failed')
            ORDER BY expires_at
            LIMIT $1
         )
         DELETE FROM EXECUTION_LEDGER_jobs_runs
         WHERE id IN (SELECT id FROM doomed)",
    )
    .bind(limit)
    .execute(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;
    Ok(result.rows_affected())
}

pub async fn gc_cleanup_stale_schedules(pool: &PgPool, limit: i64) -> Result<u64> {
    let active_version: (i64,) = sqlx::query_as(
        "SELECT active_version FROM EXECUTION_LEDGER_scheduler_control WHERE id = TRUE",
    )
    .fetch_one(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;

    let result = sqlx::query(
        "WITH doomed AS (
            SELECT scheduler_id
            FROM EXECUTION_LEDGER_schedules
            WHERE schedule_version != $1
              AND lease_owner IS NULL
            ORDER BY updated_at
            LIMIT $2
         )
         DELETE FROM EXECUTION_LEDGER_schedules
         WHERE scheduler_id IN (SELECT scheduler_id FROM doomed)",
    )
    .bind(active_version.0)
    .bind(limit)
    .execute(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;
    Ok(result.rows_affected())
}

#[derive(Debug, Clone, Copy)]
pub struct GcSummary {
    pub terminal_runs_deleted: u64,
    pub stale_schedules_deleted: u64,
    pub expired_kv_deleted: u64,
    pub offload_files_deleted: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct RunLeaseRecoverySummary {
    pub requeued_pending: u64,
    pub failed_terminal: u64,
}

pub async fn recover_expired_run_leases(
    pool: &PgPool,
    limit: i64,
) -> Result<RunLeaseRecoverySummary> {
    let rows = sqlx::query_as::<_, (String,)>(
        "WITH doomed AS (
             SELECT id
             FROM EXECUTION_LEDGER_jobs_runs
             WHERE status = 'running'
               AND lease_expires_at IS NOT NULL
               AND lease_expires_at <= now()
             ORDER BY lease_expires_at
             LIMIT $1
             FOR UPDATE SKIP LOCKED
         )
         UPDATE EXECUTION_LEDGER_jobs_runs r
         SET status = CASE WHEN r.current_attempt < r.max_attempts THEN 'pending' ELSE 'failed' END,
             completed_at = CASE WHEN r.current_attempt < r.max_attempts THEN NULL ELSE now() END,
             lease_owner = NULL,
             lease_expires_at = NULL,
             updated_at = now(),
             last_error = COALESCE(r.last_error, '{}'::jsonb) || jsonb_build_object(
                 'code', 'lease_expired',
                 'message', 'run lease expired before completion',
                 'recoveredAt', now()
             )
         FROM doomed
         WHERE r.id = doomed.id
         RETURNING r.status",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;

    let requeued_pending = rows.iter().filter(|(status,)| status == "pending").count() as u64;
    let failed_terminal = rows.iter().filter(|(status,)| status == "failed").count() as u64;

    Ok(RunLeaseRecoverySummary {
        requeued_pending,
        failed_terminal,
    })
}

pub async fn recover_expired_schedule_leases(pool: &PgPool, limit: i64) -> Result<u64> {
    let result = sqlx::query(
        "WITH doomed AS (
             SELECT scheduler_id
             FROM EXECUTION_LEDGER_schedules
             WHERE status = 'ACTIVE'
               AND lease_until IS NOT NULL
               AND lease_until <= now()
             ORDER BY lease_until
             LIMIT $1
             FOR UPDATE SKIP LOCKED
         )
         UPDATE EXECUTION_LEDGER_schedules s
         SET lease_owner = NULL,
             lease_until = NULL,
             retry_count = retry_count + 1,
             last_error = 'lease expired; released by recovery',
             updated_at = now()
         FROM doomed
         WHERE s.scheduler_id = doomed.scheduler_id",
    )
    .bind(limit)
    .execute(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;
    Ok(result.rows_affected())
}

pub async fn drain_local_dead_letter_queue(pool: &PgPool, limit: i64) -> Result<u64> {
    let result = sqlx::query(
        "WITH doomed AS (
             SELECT id
             FROM EXECUTION_LEDGER_local_intent_queue
             WHERE dead_lettered_at IS NOT NULL
             ORDER BY dead_lettered_at
             LIMIT $1
         )
         DELETE FROM EXECUTION_LEDGER_local_intent_queue q
         USING doomed
         WHERE q.id = doomed.id",
    )
    .bind(limit)
    .execute(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;
    Ok(result.rows_affected())
}

pub async fn gc_cleanup_batch(pool: &PgPool, limit_per_table: i64) -> Result<GcSummary> {
    let offload_paths = sqlx::query_as::<_, (String,)>(
        "SELECT state_data->>'offloadPath' AS offload_path
         FROM EXECUTION_LEDGER_jobs_runs
         WHERE expires_at IS NOT NULL
           AND expires_at <= now()
           AND status IN ('completed', 'failed', 'cancelled')
           AND state_data ? 'offloadPath'
         ORDER BY expires_at
         LIMIT $1",
    )
    .bind(limit_per_table)
    .fetch_all(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?
    .into_iter()
    .map(|(path,)| path)
    .collect::<Vec<_>>();

    // F10 fix: delete offload files BEFORE deleting DB records. If the
    // process crashes between file deletion and DB deletion, orphaned DB
    // records remain (safe — they'll be cleaned up on the next GC run).
    // The reverse order (old code) orphaned files with no DB record to
    // reference them for cleanup.
    let offload_files_deleted = cleanup_offload_files(&offload_paths)?;

    let terminal_runs_deleted = gc_cleanup_terminal_runs(pool, limit_per_table).await?;
    let stale_schedules_deleted = gc_cleanup_stale_schedules(pool, limit_per_table).await?;
    let expired_kv_deleted = kv_cleanup_expired(pool, limit_per_table).await?;
    Ok(GcSummary {
        terminal_runs_deleted,
        stale_schedules_deleted,
        expired_kv_deleted,
        offload_files_deleted,
    })
}

pub async fn scheduler_set_status(pool: &PgPool, scheduler_id: &str, status: &str) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE EXECUTION_LEDGER_schedules
         SET status = $2, updated_at = now()
         WHERE scheduler_id = $1",
    )
    .bind(scheduler_id)
    .bind(status)
    .execute(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;
    Ok(result.rows_affected() > 0)
}

pub struct ScheduleReleaseSpec<'a> {
    pub scheduler_id: &'a str,
    pub job_id: &'a str,
    pub cron_expression: &'a str,
    pub timezone: &'a str,
    pub next_run: DateTime<Utc>,
}

pub async fn scheduler_release_upsert(
    pool: &PgPool,
    spec: &ScheduleReleaseSpec<'_>,
) -> Result<i64> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        })?;

    let control: (i64, i64) = sqlx::query_as(
        "SELECT active_version, next_version
         FROM EXECUTION_LEDGER_scheduler_control
         WHERE id = TRUE
         FOR UPDATE",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;

    sqlx::query(
        "INSERT INTO EXECUTION_LEDGER_schedules (
            scheduler_id, schedule_version, job_id, cron_expression, timezone, next_run, status
         ) VALUES ($1, $2, $3, $4, $5, $6, 'ACTIVE')
         ON CONFLICT (scheduler_id)
         DO UPDATE SET
            schedule_version = EXCLUDED.schedule_version,
            job_id = EXCLUDED.job_id,
            cron_expression = EXCLUDED.cron_expression,
            timezone = EXCLUDED.timezone,
            next_run = EXCLUDED.next_run,
            status = 'ACTIVE',
            retry_count = 0,
            last_error = NULL,
            updated_at = now()",
    )
    .bind(spec.scheduler_id)
    .bind(control.1)
    .bind(spec.job_id)
    .bind(spec.cron_expression)
    .bind(spec.timezone)
    .bind(spec.next_run)
    .execute(&mut *tx)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;

    sqlx::query(
        "UPDATE EXECUTION_LEDGER_scheduler_control
         SET active_version = next_version,
             next_version = next_version + 1,
             updated_at = now()
         WHERE id = TRUE",
    )
    .execute(&mut *tx)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;

    tx.commit()
        .await
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "postgres",
            source: Box::new(source),
        })?;

    Ok(control.1)
}

pub async fn list_schedules(pool: &PgPool, limit: i64) -> Result<Vec<serde_json::Value>> {
    let rows = sqlx::query_as::<_, (serde_json::Value,)>(
        "SELECT row_to_json(t) FROM (
            SELECT scheduler_id, job_id, cron_expression, timezone, next_run,
                   status, enabled, created_at, updated_at
            FROM EXECUTION_LEDGER_schedules
            ORDER BY next_run ASC NULLS LAST
            LIMIT $1
        ) t",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|source| ExecutionLedgerError::DependencyFailed {
        dependency: "postgres",
        source: Box::new(source),
    })?;
    Ok(rows.into_iter().map(|(v,)| v).collect())
}
