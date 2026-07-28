# DTU ↔ vCore Migration with LicenseType Preservation

## The Problem

Azure SQL Database supports two purchasing models:

1. **DTU** (Basic, Standard, Premium) — bundled compute + storage + backup
2. **vCore** (GeneralPurpose, BusinessCritical) — separate compute and storage with Azure Hybrid Benefit option

Switching between them requires a SKU change via ARM. The complication: when moving between models, Azure may return the current database resource without `licenseType` set — even if the database had Hybrid Benefit enabled. This is a known Azure SQL API gap, first documented for this pack's design during a review of [Bellhop](https://github.com/Azure/bellhop) (an archived, unrelated Microsoft autoscaling sample) issue #77 — cited here only as the source of the observation, not as a dependency or required reading.

If you restore a database after a DTU → vCore switch without explicitly setting `licenseType: "BasePrice"`, Azure will bill for the SQL Server license on every vCore database — silently adding significant cost.

## How Execution Ledger Handles This

The `sql-database-get` step always reads and returns `licenseType` in its `stateBlob`, regardless of whether it's currently set:

```json
{
  "action": "sql-database-get",
  "licenseType": "BasePrice",
  "stateBlob": {
    "sku": { "name": "S2", "tier": "Standard", "capacity": 50 },
    "licenseType": "BasePrice"
  }
}
```

The `sql-database-resize` step accepts `licenseType` explicitly in `targetSku.licenseType`. If the current value is absent from the API response, the step-level value overrides — so Hybrid Benefit is preserved across switches.

## Decision Tree

```
Start
  │
  ├─► What model is the current database?
  │     │
  │     ├─► DTU ──► Want to scale UP within DTU?
  │     │              Yes ──► sql-database-resize with same licenseType
  │     │              No ──► Want to switch to vCore?
  │     │                       Yes ──► capture licenseType first
  │     │                       No  ──► scale down within DTU tier
  │     │
  │     └─► vCore ─► Want to scale within vCore?
  │                    Yes ──► sql-database-resize, preserve licenseType
  │                    No ──► Want to switch to DTU?
  │                             Yes ──► capture licenseType first
  │                             No ──► scale within vCore tier
  │
  └─► Was Hybrid Benefit enabled? (licenseType = BasePrice)
        │
        ├─► Yes ──► ALWAYS carry licenseType forward in restore
        │            Even if target tier is different model
        └─► No / Unknown ──► Check billing blade in portal
                              before assuming LicenseIncluded
```

## SKU Mapping Reference

### Standard (DTU) → GeneralPurpose (vCore)

| DTU Standard | vCore GeneralPurpose |
|--------------|-------------------|
| S0 (10 DTU) | GP_Gen5_1 |
| S1 (20 DTU) | GP_Gen5_2 |
| S2 (50 DTU) | GP_Gen5_4 |
| S3 (100 DTU) | GP_Gen5_8 |

### Premium (DTU) → BusinessCritical (vCore)

| DTU Premium | vCore BusinessCritical |
|-------------|---------------------|
| P1 (125 DTU) | BC_Gen5_2 |
| P2 (250 DTU) | BC_Gen5_4 |
| P4 (500 DTU) | BC_Gen5_8 |

Note: BusinessCritical vCore SKUs are not direct DTU Premium equivalents — BusinessCritical uses local SSD storage and has different HA characteristics. Verify the target tier matches your SLA requirements before switching.
