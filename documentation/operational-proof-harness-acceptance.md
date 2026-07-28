# Operational-proof harness acceptance gates

This document defines machine-readable pass and fail gates for unattended operational-proof runs in Execution Ledger.

## Files

- `execution-ledger/ops/operational-proof-gates.json`: kill-switch gate configuration
- `execution-ledger/ops/operational-proof-snapshot.example.json`: expected metrics snapshot shape
- `execution-ledger/scripts/validate_operational_proof.py`: gate evaluator, exits non-zero on failure

## Gate model

The validator treats every failed gate as a kill-switch trigger when `kill_switch.enabled` is true.

Current hard-stop thresholds:

1. Error budget and failure rate
   - `error_budget.failure_rate` must be `<= 0.02`
   - `error_budget.burn_rate_1h` must be `<= 2.0`
   - `error_budget.burn_rate_6h` must be `<= 1.0`
   - `error_budget.total_requests` must be `>= 100`
1. Queue lag
   - `queue.intent_queue_lag_seconds_p95` must be `<= 120`
   - `queue.intent_queue_oldest_message_seconds` must be `<= 300`
1. Crash-loop and restart protection
   - Sum of `runtime.service_restarts.*` must be `<= 3`
   - Each individual service restart count must be `<= 2`
1. Security policy
   - `security.api_auth_mode` must be one of `["managed_identity"]`
   - `security.servicebus_auth_mode` must be one of `["managed_identity"]`
   - `security.secret_resolution_failures` must be `<= 0`
   - `security.missing_required_secrets` must be `<= 0`
1. SLO minimums for critical paths
   - `slos.platform_jobs_trigger_success_ratio` must be `>= 0.995`
   - `slos.platform_jobs_list_success_ratio` must be `>= 0.995`
   - `slos.mcp_tool_call_success_ratio` must be `>= 0.995`
   - `slos.platform_jobs_trigger_latency_p95_ms` must be `<= 1200`

Missing required metrics are treated as failures.

## Snapshot input format

Use JSON with these top-level objects:

- `error_budget`
- `queue`
- `runtime`
- `security`
- `slos`

Reference the exact field names in `execution-ledger/ops/operational-proof-snapshot.example.json`.

## Validator usage

```bash
python execution-ledger/scripts/validate_operational_proof.py --snapshot execution-ledger/ops/operational-proof-snapshot.example.json
```

Use a custom gate file:

```bash
python execution-ledger/scripts/validate_operational_proof.py --gates execution-ledger/ops/operational-proof-gates.json --snapshot <path-to-snapshot.json>
```

Exit behavior:

- Exit code `0`: all gates passed
- Exit code `1`: one or more gates failed, or kill-switch condition triggered

The script prints a JSON result payload with `status`, `kill_switch_triggered`, and per-gate failure details.

## Chaos testing plan template

Use this template for bounded game-day chaos runs against the Service Bus queue or PostgreSQL.

### Service Bus queue disruption

- **Hypothesis:** Disabling `execution-ledger-intents` for 3 minutes will not breach API 5xx above 1%, and worker recovery will catch up within 10 minutes after re-enable.
- **Fault:** Chaos Studio Service Bus queue state change (`Disabled` then `Active`) on one namespace and queue only.
- **Blast radius:** One namespace, one queue, one 15-minute window, no concurrent platform changes.
- **Abort conditions:** Stop run if API 5xx exceeds 1% for 2 minutes, queue dead-letter count grows continuously for 5 minutes, or on-call pages.
- **Rollback:** Immediately force queue state to `Active`, run `/platform/recovery/runs`, then `/platform/recovery/servicebus-poison` only if dead-letter growth persists.
- **Observability:** Capture API health, worker receive/retry metrics, run lease recovery counts, and dead-letter depth before, during, and after.

### PostgreSQL bounded restart or failover

```powershell
pwsh -File .\execution-ledger\scripts\Invoke-PostgresFlexibleServerFault.ps1 `
  -ResourceGroupName <rg> `
  -ServerName <server> `
  -FaultMode PlannedFailover `
  -TimeoutMinutes 20 `
  -PollIntervalSeconds 20 `
  -Apply
```

Validates command surface, checks pre-state, executes a bounded disruption, polls to `Ready`, and attempts a rollback restart if recovery does not complete in time.

## Unattended harness

Run the operational proof harness for baseline, soak, chaos, and collection stages:

```powershell
pwsh -File .\execution-ledger\scripts\Invoke-OperationalProofRun.ps1 `
  -SubscriptionId <subscription-id> `
  -Stages baseline,soak,chaos,collect `
  -Apply
```

- **Chaos stage** requires `-Apply`; without it the stage records as `Skipped`.
- **DR integrity** pass: `Invoke-DrIntegrityPass.ps1` validates baseline preconditions and fails closed when any invariant is `Fail` or `Blocked`.
- Exit code `0` means all selected stages succeeded; `1` means at least one stage failed.
