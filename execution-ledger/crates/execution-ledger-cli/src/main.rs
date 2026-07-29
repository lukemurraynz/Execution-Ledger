use anyhow::{Context, bail};
use chrono::Utc;
use execution_ledger_core::{
    ExecutionLedgerConfig, ExecutionLedgerIntent, ExecutionLedgerIntentSource, QueueEngine,
    build_intent, db, queue,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

const INTENT_QUEUE_NAME: &str = "execution-ledger-intents";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        print_usage();
        return Ok(());
    };

    // Pure static-file check — no database, config, or Service Bus needed,
    // so it runs before (and without) the live infra setup below.
    if command == "validate-job" {
        let path = args.next().context("validate-job requires <path-to-job-json>")?;
        return validate_job(&path);
    }

    let config = ExecutionLedgerConfig::from_env_with_vault().await?;
    let pool = db::connect(&config.database_url).await?;
    db::bootstrap_runtime_tables(&pool).await?;
    let servicebus_client = if matches!(config.queue_engine, QueueEngine::ServiceBus) {
        let queue_name = config
            .servicebus_queue_name
            .as_deref()
            .context("EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME is required")?;
        Some(queue::ServiceBusQueueClient::from_managed_identity(
            config
                .servicebus_namespace
                .as_deref()
                .context("EXECUTION_LEDGER_SERVICEBUS_NAMESPACE is required")?,
            queue_name,
        )?)
    } else {
        None
    };

    match command.as_str() {
        "db-status" => {
            sqlx::query("SELECT 1")
                .execute(&pool)
                .await
                .context("database unreachable")?;
            println!("ok");
        }
        "kv-get" => {
            let key = args.next().context("kv-get requires <key>")?;
            let value = db::kv_get(&pool, &key).await?;
            if let Some(record) = value {
                println!("{}", serde_json::to_string_pretty(&record.value_json)?);
            } else {
                println!("null");
            }
        }
        "kv-set" => {
            let key = args.next().context("kv-set requires <key>")?;
            let json = args.next().context("kv-set requires <json-value>")?;
            let value: Value = serde_json::from_str(&json).context("invalid JSON value")?;
            let ttl_seconds = args.next().map(|v| v.parse::<i64>()).transpose()?;
            let record = db::kv_set(&pool, &key, &value, None, ttl_seconds).await?;
            println!("{}", record.version);
        }
        "kv-remove" => {
            let key = args.next().context("kv-remove requires <key>")?;
            let removed = db::kv_delete(&pool, &key).await?;
            println!("{removed}");
        }
        "scheduler-enable" => {
            let scheduler_id = args
                .next()
                .context("scheduler-enable requires <scheduler-id>")?;
            let changed = db::scheduler_set_status(&pool, &scheduler_id, "ACTIVE").await?;
            println!("{changed}");
        }
        "scheduler-disable" => {
            let scheduler_id = args
                .next()
                .context("scheduler-disable requires <scheduler-id>")?;
            let changed = db::scheduler_set_status(&pool, &scheduler_id, "DISABLED").await?;
            println!("{changed}");
        }
        "scheduler-cleanup" => {
            let limit = args
                .next()
                .map(|v| v.parse::<i64>())
                .transpose()?
                .unwrap_or(100);
            let count = db::gc_cleanup_stale_schedules(&pool, limit).await?;
            println!("{count}");
        }
        "cleanup-runs" => {
            let limit = args
                .next()
                .map(|v| v.parse::<i64>())
                .transpose()?
                .unwrap_or(500);
            let summary = db::gc_cleanup_batch(&pool, limit).await?;
            println!(
                "{} {} {} {}",
                summary.terminal_runs_deleted,
                summary.stale_schedules_deleted,
                summary.expired_kv_deleted,
                summary.offload_files_deleted
            );
        }
        "recovery-runs" => {
            let limit = args
                .next()
                .map(|v| v.parse::<i64>())
                .transpose()?
                .unwrap_or(500);
            let summary = db::recover_expired_run_leases(&pool, limit).await?;
            println!("{} {}", summary.requeued_pending, summary.failed_terminal);
        }
        "recovery-schedules" => {
            let limit = args
                .next()
                .map(|v| v.parse::<i64>())
                .transpose()?
                .unwrap_or(200);
            let recovered = db::recover_expired_schedule_leases(&pool, limit).await?;
            println!("{recovered}");
        }
        "recovery-local-poison" => {
            let limit = args
                .next()
                .map(|v| v.parse::<i64>())
                .transpose()?
                .unwrap_or(500);
            let drained = db::drain_local_dead_letter_queue(&pool, limit).await?;
            println!("{drained}");
        }
        "recovery-servicebus-poison" => {
            let limit = args
                .next()
                .map(|v| v.parse::<usize>())
                .transpose()?
                .unwrap_or(200);
            let drained = servicebus_client
                .context("service bus queue client is required")?
                .drain_dead_letter_messages(limit)
                .await?;
            println!("{drained}");
        }
        "job-launch" => {
            let tenant_id =
                Uuid::parse_str(&args.next().context("job-launch requires <tenant-id>")?)?;
            let job_id = args.next().context("job-launch requires <job-id>")?;
            let payload = args.next().unwrap_or_else(|| "{}".to_string());
            let input_data: Value =
                serde_json::from_str(&payload).context("invalid JSON payload")?;
            let idempotency = args
                .next()
                .map(|v| Uuid::parse_str(&v))
                .transpose()?
                .unwrap_or_else(|| {
                    let material = Utc::now()
                        .timestamp_nanos_opt()
                        .unwrap_or_default()
                        .to_string();
                    Uuid::new_v5(&Uuid::NAMESPACE_OID, material.as_bytes())
                });
            let correlation_id = args.next().unwrap_or_else(|| idempotency.to_string());

            let row = sqlx::query_as::<_, (Uuid, String, String)>(
                "INSERT INTO EXECUTION_LEDGER_jobs_runs (
                    id, idempotency_key, correlation_id, job_id, status, input_data, current_attempt, max_attempts, tenant_id
                 ) VALUES ($1, $1, $2, $3, 'pending', $4, 1, 4, $5)
                 ON CONFLICT (idempotency_key) DO UPDATE
                   SET updated_at = now()
                 RETURNING id, status, correlation_id",
            )
            .bind(idempotency)
            .bind(&correlation_id)
            .bind(job_id.clone())
            .bind(input_data.clone())
            .bind(tenant_id)
            .fetch_one(&pool)
            .await?;

            let intent = build_job_intent(row.0, &job_id, "target.invoked", input_data, &row.2)?;
            enqueue_job_intent(&pool, &config, servicebus_client.as_ref(), &intent).await?;
            println!("{} {} {}", row.0, row.1, row.2);
        }
        "job-replay" => {
            let tenant_id =
                Uuid::parse_str(&args.next().context("job-replay requires <tenant-id>")?)?;
            let run_id = Uuid::parse_str(&args.next().context("job-replay requires <run-id>")?)?;
            let step_ids = args.next().unwrap_or_default();
            let replay_steps = step_ids
                .split(',')
                .filter(|value| !value.trim().is_empty())
                .map(|value| value.trim().to_string())
                .collect::<Vec<_>>();
            // Optional 4th positional arg: the literal string "force".
            // Required to replay a step classified NotSafeWithoutGuard by
            // execution_ledger_domain::connectors::contracts::replay_safety
            // (e.g. ms_graph_notify, or rest/erp where this platform can't
            // know whether the target treats a repeat call as idempotent) —
            // matches the API route's `force: true` request field. Same
            // shared classification, same guardrail, whichever entry point
            // an operator uses.
            let force = args.next().as_deref() == Some("force");

            if !replay_steps.is_empty() && !force {
                let input_data = sqlx::query_as::<_, (Value,)>(
                    "SELECT input_data FROM EXECUTION_LEDGER_jobs_runs WHERE id = $1 AND tenant_id = $2",
                )
                .bind(run_id)
                .bind(tenant_id)
                .fetch_optional(&pool)
                .await?
                .context("run not found")?
                .0;

                let unsafe_steps = execution_ledger_domain::connectors::contracts::unsafe_replay_steps(
                    &input_data,
                    &replay_steps,
                );
                if !unsafe_steps.is_empty() {
                    let step_list = unsafe_steps
                        .iter()
                        .map(|s| format!("{} ({}{})", s.step_id, s.connector, s.action.as_deref().map(|a| format!(":{a}")).unwrap_or_default()))
                        .collect::<Vec<_>>()
                        .join(", ");
                    bail!(
                        "replay blocked: these steps repeat a real external action on replay, \
                         not just an in-platform retry: {step_list}. Re-run with a 4th argument \
                         \"force\" to proceed anyway."
                    );
                }
            }

            let row = sqlx::query_as::<_, (Uuid, String, String)>(
                "UPDATE EXECUTION_LEDGER_jobs_runs
                 SET status = 'pending',
                     completed_at = NULL,
                     updated_at = now(),
                     last_error = NULL
                 WHERE id = $1 AND tenant_id = $2
                 RETURNING id, job_id, correlation_id",
            )
            .bind(run_id)
            .bind(tenant_id)
            .fetch_optional(&pool)
            .await?
            .context("run not found")?;

            let intent = build_job_intent(
                row.0,
                &row.1,
                "target.replay",
                json!({ "replay": { "stepIds": replay_steps } }),
                &row.2,
            )?;
            enqueue_job_intent(&pool, &config, servicebus_client.as_ref(), &intent).await?;
            println!("{} {} replay", row.0, row.2);
        }
        "job-skip" => {
            let tenant_id =
                Uuid::parse_str(&args.next().context("job-skip requires <tenant-id>")?)?;
            let run_id = Uuid::parse_str(&args.next().context("job-skip requires <run-id>")?)?;
            let reason = args.next().unwrap_or_else(|| "manual skip".to_string());
            let changed = sqlx::query(
                "UPDATE EXECUTION_LEDGER_jobs_runs
                 SET status = 'cancelled',
                     completed_at = now(),
                     updated_at = now(),
                     state_data = jsonb_set(COALESCE(state_data, '{}'::jsonb), '{operator}', jsonb_build_object('action','skip','reason',$3), true)
                 WHERE id = $1 AND tenant_id = $2",
            )
            .bind(run_id)
            .bind(tenant_id)
            .bind(reason)
            .execute(&pool)
            .await?;
            println!("{}", changed.rows_affected());
        }
        "job-compensate" => {
            let tenant_id =
                Uuid::parse_str(&args.next().context("job-compensate requires <tenant-id>")?)?;
            let run_id =
                Uuid::parse_str(&args.next().context("job-compensate requires <run-id>")?)?;
            let payload = args.next().unwrap_or_else(|| "{}".to_string());
            let compensate_data: Value =
                serde_json::from_str(&payload).context("invalid JSON payload")?;

            let row = sqlx::query_as::<_, (Uuid, String, String)>(
                "UPDATE EXECUTION_LEDGER_jobs_runs
                 SET status = 'pending',
                     completed_at = NULL,
                     updated_at = now(),
                     last_error = NULL
                 WHERE id = $1 AND tenant_id = $2
                 RETURNING id, job_id, correlation_id",
            )
            .bind(run_id)
            .bind(tenant_id)
            .fetch_optional(&pool)
            .await?
            .context("run not found")?;

            let intent = build_job_intent(
                row.0,
                &row.1,
                "target.compensate",
                json!({ "compensate": { "data": compensate_data } }),
                &row.2,
            )?;
            enqueue_job_intent(&pool, &config, servicebus_client.as_ref(), &intent).await?;
            println!("{} {} compensate", row.0, row.2);
        }
        "job-schedules-release" => {
            let scheduler_id = args
                .next()
                .context("job-schedules-release requires <scheduler-id>")?;
            let job_id = args
                .next()
                .context("job-schedules-release requires <job-id>")?;
            let cron_expression = args
                .next()
                .context("job-schedules-release requires <cron-expression>")?;
            let timezone = args.next().unwrap_or_else(|| "UTC".to_string());
            let next_run_raw = args.next().unwrap_or_else(|| Utc::now().to_rfc3339());
            let next_run = chrono::DateTime::parse_from_rfc3339(&next_run_raw)
                .context("next-run must be RFC3339")?
                .with_timezone(&Utc);

            let version = db::scheduler_release_upsert(
                &pool,
                &db::ScheduleReleaseSpec {
                    scheduler_id: &scheduler_id,
                    job_id: &job_id,
                    cron_expression: &cron_expression,
                    timezone: &timezone,
                    next_run,
                },
            )
            .await?;
            println!("{version}");
        }
        "preflight" => {
            run_preflight(&config, &pool, servicebus_client.as_ref()).await?;
        }
        _ => {
            print_usage();
            bail!("unknown command: {command}");
        }
    }

    Ok(())
}

fn build_job_intent(
    run_id: Uuid,
    job_id: &str,
    event: &str,
    data: Value,
    correlation_id: &str,
) -> anyhow::Result<ExecutionLedgerIntent> {
    build_intent(
        run_id,
        job_id,
        event,
        ExecutionLedgerIntentSource::Manual,
        data,
        correlation_id,
    )
    .context("invalid intent envelope")
}

async fn enqueue_job_intent(
    pool: &sqlx::PgPool,
    config: &ExecutionLedgerConfig,
    servicebus_client: Option<&queue::ServiceBusQueueClient>,
    intent: &ExecutionLedgerIntent,
) -> anyhow::Result<()> {
    match config.queue_engine {
        QueueEngine::Local => {
            queue::enqueue_local_intent(pool, INTENT_QUEUE_NAME, intent).await?;
        }
        QueueEngine::ServiceBus => {
            servicebus_client
                .context("service bus client is required")?
                .enqueue_intent(intent)
                .await?;
        }
    }
    Ok(())
}

fn print_usage() {
    println!("execution-ledger-cli");
    println!(
        "  preflight                              Validate environment readiness before first trigger"
    );
    println!("  db-status");
    println!("  kv-get <key>");
    println!("  kv-set <key> <json-value> [ttl-seconds]");
    println!("  kv-remove <key>");
    println!("  scheduler-enable <scheduler-id>");
    println!("  scheduler-disable <scheduler-id>");
    println!("  scheduler-cleanup [limit]");
    println!("  cleanup-runs [limit-per-table]");
    println!("  recovery-runs [limit]");
    println!("  recovery-schedules [limit]");
    println!("  recovery-local-poison [limit]");
    println!("  recovery-servicebus-poison [limit]");
    println!(
        "  job-launch <tenant-id> <job-id> [json-payload] [idempotency-uuid] [correlation-id]"
    );
    println!("  job-replay <tenant-id> <run-id> [step-id-csv] [force]");
    println!("  job-skip <tenant-id> <run-id> [reason]");
    println!("  job-compensate <tenant-id> <run-id> [json-payload]");
    println!(
        "  job-schedules-release <scheduler-id> <job-id> <cron-expression> [timezone] [next-run-rfc3339]"
    );
    println!("  validate-job <path-to-job-json>        Static composability/replay-safety check, no DB needed");
}

/// Static check over a job template's `steps` array — no database, config,
/// or live connectors involved. Flags two things the worker/API would only
/// surface at run time (or, for replay-safety, only when someone actually
/// tries to replay a step):
///
/// 1. A step with empty `input`/`data` immediately following another step,
///    where that connector/action does not read `ctx.upstream_output`
///    (see `connectors::contracts::consumes_upstream_output`) — the classic
///    "expected this to chain from the previous step, it won't" mistake
///    (`transform` is the most common way to hit this).
/// 2. Steps whose `connector`/`action` classify as
///    `contracts::ReplaySafety::NotSafeWithoutGuard` — informational, so a
///    job author learns this at authoring time rather than discovering it
///    only when an operator's replay gets blocked mid-incident.
///
/// Deliberately does NOT try to detect missing compensation "capture" steps
/// (e.g. a `vmss-resize` with no preceding `vmss-get`) — there's no
/// structured way to know which action captures state for which mutate
/// action without guessing at naming conventions, and a confident-looking
/// wrong guess is worse than not checking at all. `RestoresState` steps get
/// a one-line reminder instead of a heuristic pass/fail.
fn validate_job(path: &str) -> anyhow::Result<()> {
    use execution_ledger_domain::connectors::contracts::{
        CompensationBehavior, ReplaySafety, compensation_behavior, likely_missing_chain,
        replay_safety,
    };
    use execution_ledger_domain::connectors::{ConnectorKind, ConnectorStep};

    fn connector_name(kind: ConnectorKind) -> String {
        serde_json::to_value(kind)
            .ok()
            .and_then(|value| value.as_str().map(ToString::to_string))
            .unwrap_or_else(|| format!("{kind:?}"))
    }

    let raw =
        std::fs::read_to_string(path).with_context(|| format!("failed to read {path}"))?;
    let job: Value =
        serde_json::from_str(&raw).with_context(|| format!("{path} is not valid JSON"))?;
    // Two job-template shapes coexist in this repo's connector-packs: a flat
    // `{"id", "steps": [...]}` (sftp-batch-pack, d365-dmf-pack, ...) and a
    // wrapped `{"jobId", "data": {"steps": [...], ...}}` trigger-request
    // shape (example-finance-settlement-pack, itsm-helpdesk-pack). Accept
    // either rather than assuming one.
    let steps_value = job
        .get("steps")
        .or_else(|| job.get("data").and_then(|data| data.get("steps")))
        .and_then(Value::as_array)
        .with_context(|| format!("{path} has no \"steps\" array (checked top level and \"data.steps\")"))?;
    let steps: Vec<ConnectorStep> = steps_value
        .iter()
        .cloned()
        .map(|value| serde_json::from_value(value).context("invalid step definition"))
        .collect::<anyhow::Result<Vec<_>>>()?;

    let mut warnings = 0usize;
    let mut replay_notes = Vec::new();
    let mut restores_state_notes = Vec::new();

    for (idx, step) in steps.iter().enumerate() {
        let action = step.action.as_deref();
        let name = connector_name(step.connector);

        if idx > 0 {
            let input_is_empty = matches!(&step.input, Value::Null)
                || matches!(&step.input, Value::Object(map) if map.is_empty());
            if input_is_empty && likely_missing_chain(step.connector, action) {
                warnings += 1;
                println!(
                    "warning: step \"{}\" (connector: {name}{}) has empty input/data and follows \
                     step \"{}\" — this connector does not read the previous step's output, and \
                     the gap looks unintentional (see likely_missing_chain in \
                     connectors/contracts.rs). The previous step's data will NOT reach this step \
                     automatically.",
                    step.id,
                    action.map(|a| format!(", action: {a}")).unwrap_or_default(),
                    steps[idx - 1].id,
                );
            }
        }

        if replay_safety(step.connector, action) == ReplaySafety::NotSafeWithoutGuard {
            replay_notes.push(format!("{} ({name}{})", step.id, action.map(|a| format!(":{a}")).unwrap_or_default()));
        }

        if compensation_behavior(step.connector, action) == CompensationBehavior::RestoresState {
            restores_state_notes.push(format!("{} ({name}{})", step.id, action.map(|a| format!(":{a}")).unwrap_or_default()));
        }
    }

    if !replay_notes.is_empty() {
        println!(
            "note: these steps repeat a real external action if replayed — replaying them \
             requires \"force\": true (API) or a 4th \"force\" argument (CLI): {}",
            replay_notes.join(", ")
        );
    }
    if !restores_state_notes.is_empty() {
        println!(
            "note: these steps restore prior state on compensate — confirm this job has a \
             preceding read/capture step for each, or compensation will no-op with \"no \
             stateBlob to restore from\": {}",
            restores_state_notes.join(", ")
        );
    }

    if warnings == 0 {
        println!("{path}: {} step(s), no composability warnings", steps.len());
    } else {
        println!("{path}: {} step(s), {warnings} warning(s)", steps.len());
    }

    Ok(())
}

struct PreflightCheck {
    name: String,
    status: &'static str,
    detail: String,
}

async fn run_preflight(
    config: &ExecutionLedgerConfig,
    pool: &sqlx::PgPool,
    servicebus_client: Option<&queue::ServiceBusQueueClient>,
) -> anyhow::Result<()> {
    use execution_ledger_domain::connectors::{ConnectorKind, ConnectorStep, test_connectivity};

    let mut checks: Vec<PreflightCheck> = Vec::new();
    let mut all_passed = true;

    // 1. Database connectivity
    let db_check = match sqlx::query("SELECT 1").execute(pool).await {
        Ok(_) => PreflightCheck {
            name: "Database connection".to_string(),
            status: "ok",
            detail: format!(
                "PostgreSQL reachable ({})",
                mask_database_url(&config.database_url)
            ),
        },
        Err(e) => {
            all_passed = false;
            PreflightCheck {
                name: "Database connection".to_string(),
                status: "error",
                detail: format!("unreachable: {e}"),
            }
        }
    };
    checks.push(db_check);

    // 2. Queue engine configuration
    let queue_check = match config.queue_engine {
        QueueEngine::Local => PreflightCheck {
            name: "Queue engine".to_string(),
            status: "ok",
            detail: "local (dev only)".to_string(),
        },
        QueueEngine::ServiceBus => {
            if servicebus_client.is_some() {
                PreflightCheck {
                    name: "Queue engine".to_string(),
                    status: "ok",
                    detail: format!(
                        "servicebus (namespace: {})",
                        config.servicebus_namespace.as_deref().unwrap_or("unknown")
                    ),
                }
            } else {
                all_passed = false;
                PreflightCheck {
                    name: "Queue engine".to_string(),
                    status: "error",
                    detail: "servicebus configured but client not initialized".to_string(),
                }
            }
        }
    };
    checks.push(queue_check);

    // 3. Entra audience
    let entra_check = match &config.entra_audience {
        Some(audience) => PreflightCheck {
            name: "Entra audience".to_string(),
            status: "ok",
            detail: audience.clone(),
        },
        None => PreflightCheck {
            name: "Entra audience".to_string(),
            status: "warn",
            detail: "not set (API will reject bearer tokens)".to_string(),
        },
    };
    checks.push(entra_check);

    // 4. Connector config readiness (check known env vars)
    let connector_envs: HashMap<&str, Vec<&str>> = HashMap::from([
        ("rest", vec![]),       // rest connector requires no special env vars
        ("queue", vec![]),      // queue connector uses runtime config
        ("azure_blob", vec![]), // azure_blob connector uses runtime config
        (
            "sftp",
            vec![
                "EXECUTION_LEDGER_SFTP_HOST",
                "EXECUTION_LEDGER_SFTP_USERNAME",
            ],
        ),
        (
            "ftp",
            vec![
                "EXECUTION_LEDGER_FTP_HOST",
                "EXECUTION_LEDGER_FTP_USERNAME",
            ],
        ),
        ("erp", vec!["EXECUTION_LEDGER_ERP_ENDPOINT"]),
    ]);

    for (connector, required_vars) in &connector_envs {
        let missing: Vec<&str> = required_vars
            .iter()
            .filter(|var| std::env::var(var).is_err())
            .copied()
            .collect();

        if !missing.is_empty() {
            all_passed = false;
            checks.push(PreflightCheck {
                name: format!("Connector: {connector}"),
                status: "error",
                detail: format!("missing: {}", missing.join(", ")),
            });
            continue;
        }

        checks.push(PreflightCheck {
            name: format!("Connector: {connector}"),
            status: "ok",
            detail: "ready".to_string(),
        });

        let Some(kind) = (match *connector {
            "sftp" => Some(ConnectorKind::Sftp),
            "ftp" => Some(ConnectorKind::Ftp),
            _ => None,
        }) else {
            continue;
        };

        // Env vars present doesn't mean the endpoint is actually reachable —
        // a bad host, port, or credential only shows up here, not in the
        // presence check above. Attempt a real connect-and-authenticate (no
        // file transfer) so that's caught now, not at 5am on day one
        // against a live branch endpoint.
        let probe_step = ConnectorStep {
            id: format!("preflight-{connector}"),
            connector: kind,
            action: None,
            external_id: None,
            input: Value::Null,
            config: json!({}),
            input_schema: None,
            output_schema: None,
        };
        match test_connectivity(kind, &probe_step).await {
            Ok(result) => checks.push(PreflightCheck {
                name: format!("Connector: {connector} (live)"),
                status: "ok",
                detail: format!("connected: {result}"),
            }),
            Err(error) => {
                all_passed = false;
                checks.push(PreflightCheck {
                    name: format!("Connector: {connector} (live)"),
                    status: "error",
                    detail: format!("configured but unreachable: {error}"),
                });
            }
        }
    }

    // 5. Drasi heartbeat freshness (see execution-ledger-drasi's
    // spawn_heartbeat_writer and the platform_health.drasi-heartbeat-check
    // connector action). Informational only — a missing/expired heartbeat
    // in a local/dev environment where Drasi isn't running is expected, not
    // a preflight blocker; production readiness for this signal is what the
    // scheduled drasi-heartbeat-check job (see connector-packs/hello-world-pack)
    // actually gates, not this one-shot check.
    match db::kv_get(pool, "drasi:heartbeat").await {
        Ok(Some(_)) => checks.push(PreflightCheck {
            name: "Drasi heartbeat".to_string(),
            status: "ok",
            detail: "fresh (execution-ledger-drasi is running and writing heartbeats)".to_string(),
        }),
        Ok(None) => checks.push(PreflightCheck {
            name: "Drasi heartbeat".to_string(),
            status: "warn",
            detail: "missing or expired — execution-ledger-drasi may not be running yet"
                .to_string(),
        }),
        Err(error) => checks.push(PreflightCheck {
            name: "Drasi heartbeat".to_string(),
            status: "warn",
            detail: format!("could not check: {error}"),
        }),
    }

    // 6. Environment mode
    checks.push(PreflightCheck {
        name: "Environment".to_string(),
        status: "ok",
        detail: config.environment.clone(),
    });

    // Print results
    for check in &checks {
        let icon = match check.status {
            "ok" => "ok",
            "warn" => "warn",
            "error" => "error",
            _ => "??",
        };
        println!("[{icon}] {}: {}", check.name, check.detail);
    }

    println!();
    if all_passed {
        println!("Result: all checks passed. Ready to trigger jobs.");
    } else {
        println!("Result: some checks failed. Fix the errors above and re-run `preflight`.");
        bail!("preflight checks failed");
    }

    Ok(())
}

fn mask_database_url(url: &str) -> String {
    // Show the host and database name, mask the password
    if let Some(at_pos) = url.find('@')
        && let Some(slash_after) = url[at_pos..].find('/')
    {
        let host_db = &url[at_pos + slash_after..];
        return format!("...{host_db}");
    }
    // Fallback: just show the last 20 chars
    if url.len() > 20 {
        return format!("...{}", &url[url.len() - 20..]);
    }
    url.to_string()
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── build_job_intent() ─────────────────────────────────────────────────

    #[test]
    fn build_job_intent_constructs_valid_intent() -> anyhow::Result<()> {
        let run_id = Uuid::new_v4();
        let intent = build_job_intent(
            run_id,
            "test-job",
            "target.invoked",
            json!({"steps": []}),
            "corr-123",
        )?;
        assert_eq!(intent.id, run_id);
        assert_eq!(intent.target.as_deref(), Some("test-job"));
        assert_eq!(intent.event, "target.invoked");
        assert_eq!(intent.source, ExecutionLedgerIntentSource::Manual);
        assert_eq!(intent.data["correlationId"], "corr-123");
        Ok(())
    }

    #[test]
    fn build_job_intent_wraps_input_data_in_intent_envelope() -> anyhow::Result<()> {
        let run_id = Uuid::new_v4();
        let payload = json!({"steps": [{"id": "s1", "connector": "platform_health"}]});
        let intent = build_job_intent(run_id, "j", "e", payload.clone(), "c")?;
        assert_eq!(intent.data["input"], payload);
        Ok(())
    }

    #[test]
    fn build_job_intent_preserves_complex_input() -> anyhow::Result<()> {
        let run_id = Uuid::new_v4();
        let payload = json!({
            "replay": {"stepIds": ["s1", "s2"]},
            "metadata": {"owner": "ops"}
        });
        let intent = build_job_intent(run_id, "replay-job", "target.replay", payload.clone(), "c")?;
        assert_eq!(intent.data["input"], payload);
        Ok(())
    }

    #[test]
    fn build_job_intent_sets_timestamp() -> anyhow::Result<()> {
        let run_id = Uuid::new_v4();
        let before = chrono::Utc::now();
        let intent = build_job_intent(run_id, "j", "e", json!({}), "c")?;
        let after = chrono::Utc::now();
        assert!(intent.timestamp >= before);
        assert!(intent.timestamp <= after);
        Ok(())
    }

    // ── mask_database_url() ────────────────────────────────────────────────

    #[test]
    fn mask_database_url_hides_password_in_standard_url() {
        let url = "postgresql://user:secret123@host.example.com:5432/mydb?sslmode=require";
        let masked = mask_database_url(url);
        assert!(!masked.contains("secret123"));
        assert!(masked.starts_with("..."));
        assert!(masked.contains("/mydb"));
    }

    #[test]
    fn mask_database_url_handles_url_without_password() {
        let url = "postgresql://user@localhost:5432/mydb";
        let masked = mask_database_url(url);
        assert!(masked.starts_with("..."));
        assert!(masked.contains("/mydb"));
    }

    #[test]
    fn mask_database_url_falls_back_on_short_url() {
        let url = "localhost:5432";
        // Less than 20 chars, no @ sign → returned as-is
        let masked = mask_database_url(url);
        assert_eq!(masked, url);
    }

    #[test]
    fn mask_database_url_truncates_long_url_without_at_sign() {
        let url = "this-is-a-very-long-connection-string-that-exceeds-twenty-chars";
        let masked = mask_database_url(url);
        assert!(masked.starts_with("..."));
        assert_eq!(masked.len(), 23); // "..." + 20 chars
    }

    #[test]
    fn mask_database_url_handles_ip_address_host() {
        let url = "postgresql://app:pass@10.0.1.50:5432/production";
        let masked = mask_database_url(url);
        assert!(!masked.contains("pass"));
        assert!(masked.starts_with("..."));
        assert!(masked.contains("/production"));
    }

    // ── PreflightCheck struct ───────────────────────────────────────────────

    #[test]
    fn preflight_check_ok_status() {
        let check = PreflightCheck {
            name: "db".to_string(),
            status: "ok",
            detail: "connected".to_string(),
        };
        assert_eq!(check.status, "ok");
        assert_eq!(check.name, "db");
    }

    #[test]
    fn preflight_check_error_status() {
        let check = PreflightCheck {
            name: "queue".to_string(),
            status: "error",
            detail: "unreachable".to_string(),
        };
        assert_eq!(check.status, "error");
    }

    // ── print_usage() smoke test ────────────────────────────────────────────

    #[test]
    fn print_usage_does_not_panic() {
        // Smoke test: print_usage() should not panic or allocate excessively
        print_usage();
    }
}
