# Execution Ledger connector packs

Connector packs are optional add-ons that define customer-specific workflow payloads and connector configuration contracts without changing core runtime code.

## Quick start

1. Copy `example-finance-settlement-pack/` to `connector-packs/<your-pack-id>/`.
1. Edit `pack.json`: set `id`, `description`, `owner`, and `requiredConfig`.
1. Add job templates under `jobs/`: each file is a complete step payload for `job-launch` or `/platform/jobs/trigger`.
1. Write `runbook.md`: operator notes, preflight checks, failure handling, and replay guidance.
1. Validate by launching from CLI or API with real (or mocked) connector config.

## Folder contract

Each pack should follow this structure:

```text
connector-packs/
  <pack-id>/
    pack.json
    jobs/
      <job-id>.json
    runbook.md
```

- `pack.json`: metadata, ownership, required environment/config contract.
- `jobs/*.json`: job payload templates (including `steps`) suitable for `job-launch` or `/platform/jobs/trigger`.
- `runbook.md`: operator notes, failure modes, replay and compensation guidance.

## pack.json schema

```json
{
  "id": "my-pack",
  "version": "0.1.0",
  "owner": "team-name",
  "description": "What this pack does.",
  "jobs": ["my-pack.job-name"],
  "requiredConfig": {
    "sftp": ["EXECUTION_LEDGER_SFTP_HOST", "EXECUTION_LEDGER_SFTP_USERNAME"],
    "queue.servicebus": ["EXECUTION_LEDGER_SERVICEBUS_NAMESPACE"]
  },
  "notes": [
    "Service Bus connection strings are not supported. Managed identity only."
  ]
}
```

The `requiredConfig` map lists environment variables or App Configuration keys that must be present before the pack's jobs can run. The key is the connector type (with optional qualifier); the value is an array of required variable names.

## Runtime behavior

Execution Ledger does not auto-discover packs from this folder.

Packs are source-controlled templates for:

1. onboarding new customer scenarios
1. validating connector readiness before go-live
1. running known-good payloads from API, CLI, or Tauri operator

## Current scaffold

Use `example-finance-settlement-pack` as the baseline pattern for new packs.

## Adding a new connector

If none of the existing connectors (`platform_health`, `rest`, `ms_graph_notify`, `queue`, `azure_blob`, `sftp`, `ftp`, `erp`, `azure_compute`, `azure_sql`, `transform`, `pgp`, `blob_archive`, `http_download`, `batch_window`) fit your integration, you can add a new connector to the core runtime.

### Step 1: Add the variant

In `crates/execution-ledger-domain/src/connectors/mod.rs`, add a variant to `ConnectorKind`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorKind {
    PlatformHealth,
    Rest,
    Queue,
    AzureBlob,
    MsGraphNotify,
    Sftp,
    Erp,
    AzureCompute,
    AzureSql,
    Transform,
    Pgp,
    BlobArchive,
    HttpDownload,
    Ftp,
    BatchWindow,
    MyNewConnector, // <-- add here
}
```

### Step 2: Create the module

Create `crates/execution-ledger-domain/src/connectors/my_new_connector.rs`:

```rust
use execution_ledger_core::ExecutionLedgerError;
use serde_json::{Value, json};

use crate::connectors::{ConnectorExecutionContext, ConnectorStep};
use crate::connectors::helpers::step_config_string;

/// My new connector.
///
/// ## Configuration
///
/// | Key | Required | Default | Notes |
/// |-----|----------|---------|-------|
/// | `apiKey` | yes | N/A | API key for the external service |
/// | `endpoint` | no | `https://api.example.com` | Base URL |
///
/// ## Fail-closed behavior
///
/// - Returns `InvalidConfiguration` if `apiKey` is missing.
/// - Returns `DependencyFailed` if the API call fails.
pub(crate) async fn execute(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let api_key = step_config_string(step, "apiKey")
        .ok_or(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.my_new_connector.apiKey".to_string(),
        })?;

    // ... implement your connector logic here ...

    Ok(json!({
        "stepId": step.id,
        "connector": "my_new_connector",
        "accepted": true,
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}
```

### Step 3: Wire the dispatch

In `connectors/mod.rs`, add the module declaration and match arm:

```rust
mod my_new_connector;

// In execute_step:
match step.connector {
    // ... existing arms ...
    ConnectorKind::MyNewConnector => my_new_connector::execute(ctx, step).await,
}
```

### Step 4: Document it

Add a doc comment to the module explaining:

- What the connector does
- Required and optional configuration keys
- Fail-closed behavior (what happens when config is missing or the call fails)
- Any security considerations (header denylisting, path traversal, timeout)

### Step 5: Add to a pack

Create a job template in your connector pack that uses the new connector:

```json
{
  "id": "my-step",
  "connector": "my_new_connector",
  "config": {
    "apiKey": "<from-env MY_API_KEY>",
    "endpoint": "https://api.example.com"
  },
  "data": {
    "action": "process",
    "payload": {}
  }
}
```

## Connector design principles

All connectors in Execution Ledger follow these rules:

1. **Fail closed.** If required config is missing, return `InvalidConfiguration`. Never silently skip or use a default that could produce incorrect behavior.
1. **Real execution paths.** Connectors contain actual implementation logic, not stubs. If a connector can't reach its target, it fails the step explicitly.
1. **Idempotency.** Every step execution is governed by an idempotency key (derived from run ID + step ID). Replaying the same step produces the same outcome.
1. **Correlation propagation.** The run-level correlation ID is available in `ConnectorExecutionContext` and should be included in downstream calls, logs, and error responses.
1. **Managed identity where possible.** For Azure services (Service Bus, Blob Storage, Key Vault), use managed identity. Connection strings are explicitly rejected in production.
1. **Timeout discipline.** External calls must have bounded timeouts. The REST connector caps at 120 seconds; the SFTP connector caps at 60 seconds. Configure your connector's timeout accordingly.

## User-assigned MSI and least-privilege RBAC

Execution Ledger connectors use `ManagedIdentityCredential` from `azure_identity`. This supports both system-assigned and user-assigned managed identities. For production workloads, use **one user-assigned MSI (UAMI) per connector pack** with only the RBAC roles that pack requires. This limits the blast radius if a connector is compromised.

### How it works

Each Azure connector reads `AZURE_MANAGED_IDENTITY_CLIENT_ID` at token acquisition time. Set this to the client ID of the UAMI you want that connector pack to use. The worker reads the env var from App Configuration at runtime, with no code changes needed when you switch UAMIs.

### App Configuration key naming

Use a pack-specific prefix so each connector pack can reference its own UAMI:

| Pack                  | Env var                      | App Configuration key                         |
| --------------------- | ---------------------------- | --------------------------------------------- |
| `azure-compute-scale` | `AZURE_COMPUTE_MI_CLIENT_ID` | `execution-ledger:AZURE_COMPUTE_MI_CLIENT_ID` |
| `azure-sql-scale`     | `AZURE_SQL_MI_CLIENT_ID`     | `execution-ledger:AZURE_SQL_MI_CLIENT_ID`     |

Set these as non-secret values in App Configuration. Only the client ID (a GUID) is exposed, not the actual credential.

### Per-pack UAMI setup

**1. Create the UAMI:**

```bash
az identity create \
  --name "<pack>-mi" \
  --resource-group <rg> \
  --subscription <sub>

MI_CLIENT_ID=$(az identity show \
  --name "<pack>-mi" \
  --resource-group <rg> \
  --query clientId -o tsv)
```

**2. Assign only the roles that pack needs:**

```bash
# azure-compute-scale: VMSS resize only
az role assignment create \
  --assignee "$MI_CLIENT_ID" \
  --role "Virtual Machine Scale Set Contributor" \
  --scope "/subscriptions/<sub>/resourceGroups/<rg>"

# azure-sql-scale: SQL resize only
az role assignment create \
  --assignee "$MI_CLIENT_ID" \
  --role "SQL Server Contributor" \
  --scope "/subscriptions/<sub>/resourceGroups/<rg>"
```

**3. Store client ID in App Configuration:**

```bash
az appconfig kv set \
  --name <appconfig-name> \
  --key execution-ledger:AZURE_<PACK>_MI_CLIENT_ID \
  --value "$MI_CLIENT_ID" \
  --subscription <sub>
```

**4. Reference it in `pack.json` `requiredConfig`:**

```json
{
  "requiredConfig": {
    "azure_compute": [
      "AZURE_SUBSCRIPTION_ID",
      "AZURE_RESOURCE_GROUP",
      "AZURE_COMPUTE_MI_CLIENT_ID"
    ],
    "azure_sql": [
      "AZURE_SUBSCRIPTION_ID",
      "AZURE_RESOURCE_GROUP",
      "AZURE_SQL_MI_CLIENT_ID"
    ]
  }
}
```

### Why not one MSI for everything?

A single worker MSI with broad RBAC means every connector step has full access to all resources. If a step is exploited (e.g., via a malicious job payload or injection), the attacker gains subscription-level access. Per-pack UAMIs contain the blast radius to just the resources that connector pack touches.

### Fallback: system-assigned MSI

If `AZURE_MANAGED_IDENTITY_CLIENT_ID` is not set, `ManagedIdentityCredential` falls back to the system-assigned MSI of the Container Apps host. This is acceptable for dev/test. For production, use per-pack UAMIs.
