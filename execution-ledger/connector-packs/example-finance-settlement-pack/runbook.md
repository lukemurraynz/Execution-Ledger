# Example finance settlement pack runbook

## Intent

This pack demonstrates how to package a customer-specific workflow contract outside the base runtime. It orchestrates a file-to-queue settlement flow: download a CSV from SFTP, archive it to blob storage, transform via ERP, and publish to Service Bus.

**Step-to-step chaining**: the `sftp` connector's `download` action reads a remote file and returns its content in the step's own output (base64-encoded) — it never writes to the worker's local filesystem. The `archive-original-file` step demonstrates real chaining: its `azure_blob` config has no explicit `body`, so the connector automatically reads the prior step's output (`ctx.upstream_output`), decodes it, and uploads the original file bytes. This is the only connector pair with automatic content-forwarding today — the ERP step below does not automatically receive the settlement file; it sends its own explicit JSON payload. If your ERP integration needs the raw file content, pass it explicitly in that step's `data.body` (e.g. templated from a prior step's output by your job-orchestration tooling) rather than relying on implicit chaining.

## Included job template

- `jobs/finance.settlement.file-to-queue.json`

## Required environment variables

Before running this pack, ensure these variables are set in App Configuration or Container Apps:

| Variable | Used by | Notes |
|----------|---------|-------|
| `EXECUTION_LEDGER_SFTP_HOST` | SFTP connector | SFTP server hostname |
| `EXECUTION_LEDGER_SFTP_USERNAME` | SFTP connector | SSH username |
| `EXECUTION_LEDGER_SFTP_PASSWORD` | SFTP connector | SSH password (or use `EXECUTION_LEDGER_SFTP_PRIVATE_KEY_PATH` for key auth) |
| `EXECUTION_LEDGER_BLOB_DIR` | Blob connector | Local root directory for the archive step's filesystem provider (this example doesn't set a per-run `url`, so it uses the filesystem provider — see the `azure_blob` connector's module docs for the Azure Blob Storage URL-based path, which needs a per-run blob URL your job-launch tooling computes) |
| `CONNECTOR_ERP_URL` | ERP connector | ERP transform endpoint URL |
| `EXECUTION_LEDGER_SERVICEBUS_NAMESPACE` | Queue connector | Service Bus namespace (managed identity must have send rights) |

## Preflight checks before first run

1. Confirm all required pack config from `pack.json` is present in App Configuration.
1. Confirm target queue (`settlement-intents`) exists and the worker's managed identity has `Azure Service Bus Data Sender` role.
1. Confirm SFTP endpoint is reachable from the worker Container App (network, firewall, private endpoint).
1. Confirm ERP endpoint allows the expected request shape (POST with JSON body).
1. Confirm the worker has outbound network access to all three targets.

## Launch from CLI

Run from `execution-ledger/`:

```bash
# Minimal launch (platform health check only — useful for validating connectivity)
cargo run -p execution-ledger-cli -- job-launch <tenant-id> finance.settlement.file-to-queue '{"steps":[{"id":"platform-health-gate","connector":"platform_health"}]}'

# Full launch with all steps (requires external config to be set)
cargo run -p execution-ledger-cli -- job-launch <tenant-id> finance.settlement.file-to-queue '{"steps":[{"id":"ingest-settlement-file","connector":"sftp","action":"download","externalId":"/inbound/settlement/current.csv","config":{"host":"${EXECUTION_LEDGER_SFTP_HOST}","username":"${EXECUTION_LEDGER_SFTP_USERNAME}","remotePath":"/inbound/settlement/current.csv"}},{"id":"archive-original-file","connector":"azure_blob","config":{"container":"settlement-archive","blobName":"current.csv"}},{"id":"transform-ledger-records","connector":"erp","externalId":"erp-settlement-transformer","data":{"method":"POST","body":{"template":"settlement-v1"}},"config":{"url":"${CONNECTOR_ERP_URL}"}},{"id":"publish-settlement-intent","connector":"queue","externalId":"settlement-intents","data":{"priority":"high"},"config":{"provider":"servicebus","queueName":"settlement-intents","namespace":"${EXECUTION_LEDGER_SERVICEBUS_NAMESPACE}"}},{"id":"platform-health-gate","connector":"platform_health","data":{"check":"before-settlement-commit"}}]}'
```

## Launch from API

```bash
curl -X POST https://<apim-gateway>/platform/jobs/trigger \
  -H "Authorization: Bearer <token-with-jobs:write:own-scope>" \
  -H "Content-Type: application/json" \
  -d '{
    "tenantId": "<tenant-id>",
    "jobId": "finance.settlement.file-to-queue",
    "steps": [
      {"id": "ingest-settlement-file", "connector": "sftp", "action": "download", "externalId": "/inbound/settlement/current.csv", "config": {"remotePath": "/inbound/settlement/current.csv"}},
      {"id": "archive-original-file", "connector": "azure_blob", "config": {"container": "settlement-archive", "blobName": "current.csv"}},
      {"id": "transform-ledger-records", "connector": "erp", "externalId": "erp-settlement-transformer", "data": {"method": "POST", "body": {"template": "settlement-v1"}}},
      {"id": "publish-settlement-intent", "connector": "queue", "externalId": "settlement-intents", "data": {"priority": "high"}, "config": {"provider": "servicebus", "queueName": "settlement-intents"}},
      {"id": "platform-health-gate", "connector": "platform_health"}
    ]
  }'
```

## Failure handling

### Step 1 (SFTP) fails: connection refused

**Symptom:** `DependencyFailed { dependency: "sftp", source: "connection refused" }`

**Cause:** SFTP server unreachable from worker environment.

**Fix:**
1. Check `EXECUTION_LEDGER_SFTP_HOST` and port are correct.
1. Verify network path: worker → VNet → private endpoint / public endpoint.
1. Test connectivity: `kubectl exec` into a worker pod and run `nc -zv <host> 22`.
1. Once resolved, replay the step: `cargo run -p execution-ledger-cli -- job-replay <tenant-id> <run-id> ingest-settlement-file`

### Step 1 (SFTP) fails: authentication failed

**Symptom:** `DependencyFailed { dependency: "sftp", source: "sftp authentication failed" }`

**Cause:** Invalid credentials or key configuration.

**Fix:**
1. Verify `EXECUTION_LEDGER_SFTP_USERNAME` and `EXECUTION_LEDGER_SFTP_PASSWORD` (or `EXECUTION_LEDGER_SFTP_PRIVATE_KEY_PATH`) are correct.
1. If using key auth, confirm the private key is mounted or accessible from the worker.
1. Replay the step after fixing credentials.

### Step 2 (Blob archive) fails: container not found

**Symptom:** `DependencyFailed { dependency: "azure_storage_blob", source: "..." }`

**Cause:** The `settlement-archive` container doesn't exist, or the worker's managed identity lacks `Storage Blob Data Contributor`.

**Fix:**
1. Create the container if it doesn't exist.
1. Confirm the managed identity has the `Storage Blob Data Contributor` role on the storage account.
1. Replay the step: `cargo run -p execution-ledger-cli -- job-replay <tenant-id> <run-id> archive-original-file`

### Step 3 (ERP) fails: non-2xx response

**Symptom:** `DependencyFailed { dependency: "reqwest", source: "erp request failed with status 422: ..." }`

**Cause:** ERP endpoint rejected the request payload.

**Fix:**
1. Inspect the step output: `GET /platform/jobs/{id}/steps`. The `response` field contains the ERP error body.
1. Adjust the step `data.body` to match the ERP's expected schema.
1. Replay only the failed step: `cargo run -p execution-ledger-cli -- job-replay <tenant-id> <run-id> transform-ledger-records`

### Step 4 (Queue) fails: Service Bus namespace not found

**Symptom:** `InvalidConfiguration { name: "EXECUTION_LEDGER_SERVICEBUS_NAMESPACE" }`

**Cause:** Service Bus namespace not configured or not reachable.

**Fix:**
1. Confirm `EXECUTION_LEDGER_SERVICEBUS_NAMESPACE` is set in App Configuration.
1. Confirm the worker's managed identity has `Azure Service Bus Data Sender` role.
1. Replay the step after fixing configuration.

### Step 4 (Queue) fails: connection string rejected

**Symptom:** `InvalidConfiguration { name: "EXECUTION_LEDGER_SERVICEBUS_CONNECTION_STRING (not allowed; managed identity only)" }`

**Cause:** Connection string was provided in config or environment. This is explicitly rejected.

**Fix:**
1. Remove any `EXECUTION_LEDGER_SERVICEBUS_CONNECTION_STRING` from environment and config.
1. Ensure the worker is running with a managed identity that has Service Bus send rights.
1. Replay the step.

### Stale lease (worker crash mid-run)

**Symptom:** Run appears stuck in "running" state with no step progress.

**Fix:**
```bash
# Release stale leases
cargo run -p execution-ledger-cli -- recovery-runs

# Or via API
curl -X POST https://<apim-gateway>/platform/recovery/runs?limit=100 \
  -H "Authorization: Bearer <token-with-platform:write-scope>"
```

### Poison queue (unrecoverable message)

**Symptom:** Intent appears in poison/dead-letter queue after multiple failures.

**Fix:**
```bash
# Drain poison messages
cargo run -p execution-ledger-cli -- recovery-servicebus-poison

# Or via API
curl -X POST https://<apim-gateway>/platform/recovery/servicebus-poison?limit=50 \
  -H "Authorization: Bearer <token-with-platform:write-scope>"
```

## Replay vs. re-trigger

- **Replay a step:** Use when a specific step failed but the run's earlier steps succeeded. Re-executes only the targeted step. Use `job-replay` or `POST /platform/jobs/{id}/replay`.
- **Re-trigger the run:** Use when the entire run needs to restart from scratch. Use `job-launch` or `POST /platform/jobs/trigger` with a new payload.
- **Compensate:** Use when downstream systems need rollback after a partial success. Use `job-compensate` or `POST /platform/jobs/{id}/compensate`.

## Monitoring

- Step status via `GET /platform/jobs/{id}/steps`: shows each step's state, idempotency key, and output.
- Correlation tracing: every step output includes a `correlationId`. Use this in Log Analytics to trace the full execution path across API, worker, and connector calls.
- Run listing via `GET /platform/jobs?limit=50`: shows recent runs with tenant, status, and timestamps.
