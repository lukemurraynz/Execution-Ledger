# Azure SQL Scale Pack: Operational Runbook

## Overview

Scales Azure SQL Database (DTU and vCore) and SQL Elastic Pool resources on cron schedules or Drasi metric triggers. Authenticated via managed identity. Preserves LicenseType (Azure Hybrid Benefit) across model switches.

## RBAC Requirements

Each connector pack gets its own UAMI with only the roles it needs. This limits blast radius: if a step is exploited, the attacker can only touch the resources the pack's UAMI has RBAC access to.

The azure-sql-scale UAMI needs these roles at **resource group scope**, not subscription:

| Role                     | Why                                             |
| ------------------------ | ----------------------------------------------- |
| `SQL Server Contributor` | PATCH to database and elastic pool resources    |
| `Reader`                 | Read current SKU and license type before resize |

Do NOT grant at subscription level. Do NOT reuse a shared worker MSI; use a dedicated UAMI per pack.

## Managed Identity Setup

Create a **dedicated UAMI for this pack** with only the roles SQL scale operations need. Do not reuse a shared worker MSI.

### Step 1: Create the UAMI

```bash
az identity create \
  --name azure-sql-scale-mi \
  --resource-group <rg> \
  --subscription <sub>

AZURE_SQL_MI_CLIENT_ID=$(az identity show \
  --name azure-sql-scale-mi \
  --resource-group <rg> \
  --query clientId -o tsv)
```

### Step 2: Assign only SQL-level roles

| Role                     | Scope     | Why                                             |
| ------------------------ | --------- | ----------------------------------------------- |
| `SQL Server Contributor` | Target RG | PATCH database and elastic pool resources       |
| `Reader`                 | Target RG | Read current SKU and license type before resize |

Grant at RG scope only, not subscription. Do NOT use `SQL Server Contributor` at subscription level.

```bash
az role assignment create \
  --assignee "$AZURE_SQL_MI_CLIENT_ID" \
  --role "SQL Server Contributor" \
  --scope "/subscriptions/<sub>/resourceGroups/<rg>"

az role assignment create \
  --assignee "$AZURE_SQL_MI_CLIENT_ID" \
  --role "Reader" \
  --scope "/subscriptions/<sub>/resourceGroups/<rg>"
```

### Step 3: Store client ID in App Configuration

```bash
az appconfig kv set \
  --name <appconfig-name> \
  --key execution-ledger:AZURE_SQL_MI_CLIENT_ID \
  --value "$AZURE_SQL_MI_CLIENT_ID" \
  --subscription <sub>
```

### Step 4: Reference in pack.json

```json
{
  "requiredConfig": {
    "azure_sql": [
      "AZURE_SUBSCRIPTION_ID",
      "AZURE_RESOURCE_GROUP",
      "AZURE_SQL_MI_CLIENT_ID"
    ]
  }
}
```

> **Why separate UAMIs?** The `azure-compute-scale` UAMI cannot touch SQL resources; the `azure-sql-scale` UAMI cannot resize VMSS. A compromised step is limited to the RG and roles assigned to its specific UAMI. See the [connector-packs README](../../README.md#user-assigned-msi-and-least-privilege-rbac) for the full pattern.

## Required Environment Variables

| Variable                   | Source          | Notes                                                 |
| -------------------------- | --------------- | ----------------------------------------------------- |
| `AZURE_SUBSCRIPTION_ID`    | Platform config | Subscription containing the SQL Server                |
| `AZURE_RESOURCE_GROUP`     | Platform config | RG containing the SQL Server                          |
| `SQL_SERVER_NAME`          | Per-job         | Short server name (not FQDN)                          |
| `SQL_DATABASE_NAME`        | Per-job         | Target database name                                  |
| `SQL_RESTORE_SKU_NAME`     | Per-job         | SKU to restore on scale-up, e.g. `S2`                 |
| `SQL_RESTORE_SKU_TIER`     | Per-job         | Tier to restore, e.g. `Standard`                      |
| `SQL_RESTORE_CAPACITY`     | Per-job         | DTU count to restore                                  |
| `SQL_RESTORE_LICENSE_TYPE` | Per-job         | `"BasePrice"` (Hybrid Benefit) or `"LicenseIncluded"` |

## LicenseType: Hybrid Benefit handling

This is the most important configuration for SQL cost optimization.

### What is LicenseType?

- `LicenseType = "BasePrice"`: Customer has Azure Hybrid Benefit (existing SQL Server licenses). Azure does NOT charge for the SQL Server license.
- `LicenseType = "LicenseIncluded"`: SQL Server license is included in the database cost.

### Why it matters on DTU ↔ vCore switches

When you switch from DTU to vCore (or vice versa), Azure may not preserve `LicenseType` in the API response before the switch. If you don't capture and restore it explicitly, a database that had `BasePrice` (Hybrid Benefit) will silently be restored as `LicenseIncluded`, adding SQL Server license cost to every bill.

The `sql-database-get` step captures `licenseType` in its `stateBlob`. The reactive scale-down job includes `licenseType` in both `targetSku` and step `data` to ensure it is preserved across model switches.

### KQL: Check current LicenseType

```kql
AzureActivity
| where OperationName == "Update SQL Database"
  and Properties.subscriptionId == "<sub-id>"
| where Properties.licenseType == "BasePrice"
| project TimeGenerated, Resource, Properties.licenseType
| order by TimeGenerated desc
```

Or via ARM CLI:

```bash
az sql db show \
  --resource-group <rg> \
  --server <server> \
  --name <database> \
  --query "sku"
```

## Drasi Event Hub Pipeline

See `docs/drasi-sources.md` in the `azure-compute-scale-pack` for the full Event Hub → Drasi → execution-ledger reaction pipeline.

For SQL metric alerts, route Azure Monitor Metric Alert → Event Hub → Drasi continuous query → HTTP reaction → `azure-sql-scale.metric-reactive-sql-down`.

### Sample Drasi continuous query for SQL DTU utilization

```sql
SELECT e.data.ResourceId, e.data.MetricName, e.data.Time
FROM EventHubStream e
WHERE e.data.MetricName == 'dtu_consumption_percent'
  AND e.data.Value < 5
GROUP BY e.data.ResourceId, e.data.MetricName, TumblingWindow(minute, 30)
HAVING COUNT(*) >= 30
```

## Oscillation Detection

### KQL: Count scale events per database in rolling hour

```kql
ExecutionLedger_CL
| where Action_s == "sql-database-resize"
| summarize ScaleEvents=count() by bin(TimeGenerated, 1h), ServerName_s, DatabaseName_s
| where ScaleEvents > 2
| project TimeGenerated, ServerName_s, DatabaseName_s, ScaleEvents
```

### KQL: Cooldown; skip if scaled in last 15 minutes

```kql
ExecutionLedger_CL
| where Action_s == "sql-database-resize"
| where TimeGenerated > ago(15m)
| summarize LastScale=max(TimeGenerated) by DatabaseName_s
```

## SKU Reference

### DTU-based (Basic, Standard, Premium)

| Tier     | Lowest SKU | Highest SKU | Use case             |
| -------- | ---------- | ----------- | -------------------- |
| Basic    | B_BC_DTU5  | B_BC_DTU50  | Dev/test             |
| Standard | S0         | S12         | Prod bursty          |
| Premium  | P1         | P15         | Consistent high load |

### vCore-based (GeneralPurpose, BusinessCritical)

| Tier             | Family examples         | Use case                |
| ---------------- | ----------------------- | ----------------------- |
| GeneralPurpose   | GP_S_Gen5_1, GP_Gen5_80 | Most workloads          |
| BusinessCritical | BC_Gen5_2, BC_Gen5_64   | High IO, HA requirement |

### Elastic Pool DTU

Pool capacity unit = minimum 0, maximum configured by tier.

## Troubleshooting

### ARM returns 400 Bad Request on sku.capacity

For DTU databases, `sku.capacity` must be a valid DTU value for the target tier. S0 (Standard) minimum is 10 DTU. S1 = 20, S2 = 50, S3 = 100. Do not use vCore values (e.g. 2, 4, 8) for DTU targets.

### ARM returns 400 on sku.name for vCore targets

vCore SKU names include the family and generation: `GP_Gen5_4` means 4 Gen5 vCores in GeneralPurpose. `BC_Gen5_2` means 2 vCores in BusinessCritical. Do not use DTU names (S0, S1, P1) for vCore targets.

### Database stuck in "Updating" provisioningState

SQL Database scale operations can take 30–40 minutes. The connector polls ARM async operations for up to 5 minutes. If provisioningState stays "Updating" beyond that, query the database directly:

```bash
az sql db show \
  --resource-group <rg> \
  --server <server> \
  --name <database> \
  --query "provisioningState"
```

### LicenseType missing in scale-up step

If `licenseType` was not captured in the state blob, check the database ARM resource directly and set `SQL_RESTORE_LICENSE_TYPE` explicitly in the job configuration.
