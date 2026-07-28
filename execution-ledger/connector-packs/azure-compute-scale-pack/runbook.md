# Azure Compute Scale operator runbook

## Overview

The `azure-compute-scale` pack provides scheduled and metric-reactive autoscaling for Azure compute resources via the ARM API, authenticated with managed identity.

## Jobs

| Job | Trigger | Action |
|-----|---------|--------|
| `azure-compute-scale.weekend-scale-down` | Cron (Fri 8pm) | Resize VMSS to `Standard_B2s`, 1 replica |
| `azure-compute-scale.weekend-scale-up` | Cron (Mon 7am) | Resize VMSS to `Standard_D8s_v3`, 3 replicas |
| `azure-compute-scale.metric-reactive-scale-down` | Drasi (Event Hub) | Resize VMSS to `Standard_B2s`, 1 replica |
| `azure-compute-scale.metric-reactive-scale-up` | Drasi (Event Hub) | Resize VMSS to `Standard_D8s_v3`, 5 replicas |
| `azure-compute-scale.emergency-scale-up` | Ops (manual) | Resize VMSS to `Standard_D16s_v3`, 10 replicas |

## RBAC Requirements

This pack needs **one UAMI per deployment scope** (e.g., one for prod, one for staging). Create the UAMI and assign roles before deploying the pack.

### Step 1 — Create the UAMI

```bash
# Create a dedicated UAMI for azure-compute-scale
az identity create \
  --name azure-compute-scale-mi \
  --resource-group <rg> \
  --subscription <sub>

AZURE_COMPUTE_MI_CLIENT_ID=$(az identity show \
  --name azure-compute-scale-mi \
  --resource-group <rg> \
  --query clientId -o tsv)
```

### Step 2 — Assign only the roles this pack needs

| Role | Scope | Why |
|------|-------|-----|
| `Virtual Machine Scale Set Contributor` | Target RG | Resize VMSS and change SKU/capacity |
| `Reader` | Target RG | Read instance view before scale |

```bash
az role assignment create \
  --assignee "$AZURE_COMPUTE_MI_CLIENT_ID" \
  --role "Virtual Machine Scale Set Contributor" \
  --scope "/subscriptions/<sub>/resourceGroups/<rg>"

az role assignment create \
  --assignee "$AZURE_COMPUTE_MI_CLIENT_ID" \
  --role "Reader" \
  --scope "/subscriptions/<sub>/resourceGroups/<rg>"
```

### Step 3 — Store client ID in App Configuration

```bash
az appconfig kv set \
  --name <appconfig-name> \
  --key execution-ledger:AZURE_COMPUTE_MI_CLIENT_ID \
  --value "$AZURE_COMPUTE_MI_CLIENT_ID" \
  --subscription <sub>
```

### Step 4 — Reference in pack.json

```json
{
  "requiredConfig": {
    "azure_compute": [
      "AZURE_SUBSCRIPTION_ID",
      "AZURE_RESOURCE_GROUP",
      "AZURE_COMPUTE_MI_CLIENT_ID"
    ]
  }
}
```

> **Why separate UAMIs?** A compromised connector step running under a broad MSI can affect any resource in the subscription. The `azure-compute-scale` UAMI has only `Virtual Machine Scale Set Contributor` + `Reader` on its target RG — it cannot touch SQL resources, storage accounts, or other tenants. See the [connector-packs README](../../README.md#user-assigned-msi-and-least-privilege-rbac) for the full pattern.

## Preflight Checks

Before enabling any scale job:

1. **MSI auth works**: Run the `platform_health` connector and verify the worker can reach Azure IMDS (`http://169.254.169.254/metadata/instance`).
1. **RBAC is sufficient**: Attempt a dry-run ARM GET call to the target resource and confirm 200 OK.
1. **SKU is valid**: The target SKU name must be a valid Azure VM size for the resource's location.
1. **Desired capacity is within limits**: Check subscription cores quota before scaling to large SKUs.

## Failure Handling

### Step fails with `InvalidConfiguration`
The connector config is wrong: missing `subscriptionId`, `resourceGroup`, or target name. Fix the job JSON and re-trigger. The step is idempotent so re-triggering is safe.

### Step fails with `DependencyFailed` (ARM returns non-2xx)
Common causes:
- SKU name is not valid in the region → use `az vm list-sizes --location <loc>` to find valid names.
- RBAC role missing → grant the role and replay the step.
- Resource does not exist → verify the resource name and re-create if needed.

### Step succeeds but ARM async operation fails
ARM compute operations are often async. The connector polls the `Azure-AsyncOperation` header until completion or timeout. If the async operation fails, the step returns the async operation's error: check `response.provisioningState` in the step output.

## Replay and Skip

- **Replay**: `job-replay <tenant-id> <run-id> <step-id>` re-executes only the named step. Safe to replay a scale step: the ARM API is idempotent for resize.
- **Skip**: `job-skip <tenant-id> <run-id> <reason>` marks the step as skipped without executing. Use this when a schedule fires but ops has already manually scaled.
- **Compensate**: `job-compensate <tenant-id> <run-id>` rolls back by re-applying the previous SKU. The previous SKU is captured automatically from the prior `vmss-get`/`appservice-get` step via step-to-step data flow (`stateBlob` in the resize step's output). No manual `previousSku` field is needed — just include a get-step before every resize in the job definition.

## Weekend Scale-Down Procedure

1. Verify no critical batch jobs are running on the schedule.
1. Confirm the scale-down job JSON has the correct target SKU and capacity.
1. Check the cron schedule in scheduler: `SELECT * FROM EXECUTION_LEDGER_schedules WHERE job_id LIKE '%azure-compute-scale%'`.
1. Monitor the scale-down run: `GET /platform/jobs?jobId=azure-compute-scale.weekend-scale-down`.
1. After scale-down completes, verify in Azure Portal or `az vmss get-instance-view` that instances were reduced.

## Drasi reactive override procedure

When Drasi fires a reactive scale job while a cron job is pending or recently completed:

1. The reactive job replays or triggers alongside the schedule.
1. The previous SKU is captured automatically in the resize step's `stateBlob` (propagated from the prior get-step via `previous_output`), so compensation can restore it without manual wiring.
1. Use `job-skip` on the cron job's scale step if the reactive job already handled it.
1. The reactive job's run is correlated via `correlationId` propagated to the ARM API call.

## Observability

All scale steps emit:

```json
{
  "stepId": "scale-up-vmss",
  "connector": "azure_compute",
  "action": "vmss-resize",
  "subscriptionId": "...",
  "resourceGroup": "...",
  "vmssName": "...",
  "targetSku": { "name": "...", "tier": "..." },
  "capacity": { "desiredCapacity": 3 },
  "previousSku": "Standard_B2s",
  "responseStatus": 200,
  "provisioningState": "Updating",
  "correlationId": "..."
}
```

Query Log Analytics for scale events:
```kql
ExecutionLedger_CL
| where CorrelationId_s == "<correlation-id>"
| project TimeGenerated, StepId_s, Action_s, TargetSku_s, ProvisioningState_s
```

## Oscillation Detection

Oscillation: a resource that scales up, then immediately scales down again within a short window, wasting money and compute churn.

### KQL: Count scale events per resource in rolling hour

```kql
// Alert when VMSS scales more than 3 times in 1 hour — possible oscillation
ExecutionLedger_CL
| where StepId_s contains "scale"
  and Action_s == "vmss-resize"
| summarize ScaleEvents=count() by bin(TimeGenerated, 1h), ResourceGroup_s, VmssName_s
| where ScaleEvents > 3
| project TimeGenerated, ResourceGroup_s, VmssName_s, ScaleEvents
```

### KQL: Cooldown check for scale events in last 15 minutes

```kql
// Is this resource in cooldown? (no scale if last scale was < 15 min ago)
ExecutionLedger_CL
| where StepId_s contains "scale"
  and Action_s == "vmss-resize"
| where TimeGenerated > ago(15m)
| summarize LastScale=max(TimeGenerated) by VmssName_s
| where LastScale > ago(15m)
```

### KQL: Time-since-last-scale (for hysteresis decisions)

```kql
ExecutionLedger_CL
| where Action_s == "vmss-resize"
| summarize LastScaleTime=max(TimeGenerated) by VmssName_s, ResourceGroup_s
| project VmssName_s, ResourceGroup_s, LastScaleTime, MinutesSince=datetime_diff("minute", now(), LastScaleTime)
| order by MinutesSince asc
```

## Scale cooldown pattern

Before triggering a reactive scale-down, check the last scale event. If it was < 15 minutes ago, skip the step and emit a `cooldown_skipped` outcome:

```json
{
  "id": "cooldown-check",
  "connector": "platform_health",
  "action": "query",
  "config": {
    "logAnalyticsWorkspaceId": "${WORKSPACE_ID}",
    "kql": "let last = ExecutionLedger_CL | where StepId_s contains 'scale' | summarize LastScale=max(TimeGenerated) by VmssName_s; last | where VmssName_s == 'vmss-prod' and LastScale > ago(15m)"
  }
}
```

If the cooldown check returns no rows (scale was > 15 min ago), the step succeeds and the scale step runs. If rows exist, the check step emits a skip signal and halts the run.

## Azure Monitor metric alert to Event Hub pipeline

Configure Azure Monitor to stream alert events to Event Hub when metric conditions are met:

```bash
az monitor alert create \
  --resource-group <rg> \
  --alert-name "LowCpuScaleDown" \
  --description "CPU < 5% for 5 minutes" \
  --condition "avg Percentage CPU < 5" \
  --time 5 \
  --evaluation-frequency 1 \
  --severity 3 \
  --event-hub "<namespace>/<hub-name>"
```

Drasi continuous query watches the Event Hub for sustained low-CPU events and triggers the scale-down job when conditions hold.

## RBAC quick reference

| Role | Scope | Required for |
|------|-------|-------------|
| Reader | Subscription | Finding resources by tag |
| Virtual Machine Scale Set Contributor | Resource Group | VMSS resize |
| Web Plan Contributor | Resource Group | App Service Plan resize |
| Log Analytics Reader | Workspace | KQL oscillation checks |
