# Hello World Pack

## Purpose

Minimal validation pack for Execution Ledger. Exercises `platform_health` and `rest` connectors to confirm your environment is configured correctly.

## Use this pack to

1. Verify database connectivity and schema bootstrap
2. Validate that the worker can dequeue and process intents
3. Confirm REST connector execution and idempotency
4. Test the full trigger-to-completion lifecycle

## Prerequisites

- Execution Ledger API running and accessible
- Entra token with `jobs:write:own` or `platform:write` scope
- No additional connector configuration required

## Run it

```bash
# Via CLI
cargo run -p execution-ledger-cli -- job-launch <tenant-id> hello-world.health-and-echo '{"steps":[{"id":"health-check","connector":"platform_health"},{"id":"rest-echo","connector":"rest","config":{"url":"https://httpbin.org/get","method":"GET"}}]}'

# Via API
curl -X POST https://<api-host>/platform/jobs/trigger \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{"jobId":"hello-world.health-and-echo","data":{"steps":[{"id":"health-check","connector":"platform_health"},{"id":"rest-echo","connector":"rest","config":{"url":"https://httpbin.org/get","method":"GET"}}]}}'

# Via Tauri operator
# 1. Set job ID to "hello-world.health-and-echo"
# 2. Paste the steps payload into the Manual trigger JSON field
# 3. Click "Trigger job"
```

## Expected result

- `health-check` step completes with platform health status
- `rest-echo` step completes with httpbin.org response
- Run status transitions to `completed`
- Correlation ID is propagated to both steps

## Failure modes

| Symptom                                       | Likely cause                                    | Fix                                                        |
| --------------------------------------------- | ----------------------------------------------- | ---------------------------------------------------------- |
| `health-check` fails                          | Database unreachable or schema not bootstrapped | Check `EXECUTION_LEDGER_DATABASE_URL` and run `db-status`  |
| `rest-echo` fails with `InvalidConfiguration` | REST connector missing required config          | The hello-world pack requires no config; check worker logs |
| Run stays in `pending`                        | Worker not running or queue engine mismatch     | Verify worker process and `EXECUTION_LEDGER_QUEUE_ENGINE`  |

## Drasi degraded-mode backstop (`hello-world.drasi-heartbeat-check`)

`jobs/drasi-heartbeat-check.json` schedules the independent backstop for
silent Drasi degradation. Drasi cannot monitor its own absence, so this job
runs on the ordinary scheduler+worker path (unrelated to Drasi) and alerts
via `ms_graph_notify` when `/health` reports `status: "degraded"`, i.e.
the `drasi:heartbeat` KV entry `execution-ledger-drasi` writes every ~60s
has expired.

Register it on a schedule (e.g. every 5 minutes) so it actually runs
continuously rather than once:

```bash
cargo run -p execution-ledger-cli -- job-schedules-release \
  hello-world-drasi-heartbeat hello-world.drasi-heartbeat-check "*/5 * * * *"
```

Fill in `healthUrl` and the `alert` mailbox/distribution-list fields in
`jobs/drasi-heartbeat-check.json` before deploying; both are required
config, validated eagerly so a bad alert target is caught in testing
rather than during a real Drasi outage.
