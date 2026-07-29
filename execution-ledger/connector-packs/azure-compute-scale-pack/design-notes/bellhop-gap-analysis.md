# Bellhop Gap Analysis: Execution Ledger azure-compute-scale Pack

> **Internal design rationale: not required reading to use this pack.** This document is a design-decision trail, not usage documentation: it records why the `azure-compute-scale` pack made specific choices by comparing them against a prior tool's issue tracker. For how to actually run this pack, see [`../runbook.md`](../runbook.md) and [`../docs/`](../docs/). This file lives outside `docs/` deliberately, so it doesn't read as part of the pack's user-facing documentation set.

This document records decisions made from analyzing the archived Bellhop repo (issues, PRs, scaler modules). [Bellhop](https://github.com/Azure/bellhop) was a Microsoft sample project for tag-driven Azure resource autoscaling, archived in 2021, with roughly 3 years of community feature requests before archival. It is not a dependency of Execution Ledger and no familiarity with it is needed to use this pack. This document exists only so the reasoning behind `azure-compute-scale`'s design choices is traceable for maintainers. Execution Ledger takes a different architectural approach: some Bellhop patterns translate directly, others don't.

## Resource Types

### ✅ Covered by this pack

| Resource                   | Bellhop support                                              | Execution Ledger                                   |
| -------------------------- | ------------------------------------------------------------ | -------------------------------------------------- |
| VMSS SKU + capacity        | Tag-based (`setState-SkuName`, `setState-Capacity`)          | `vmss-resize` action                               |
| App Service Plan           | Tag-based (`setState-Tier`, `WorkerSize`, `NumberOfWorkers`) | `appservice-resize` action                         |
| VM instance view           | Partial (no tag-based save/restore                           | `vm-get` action                                    |
| State capture before scale | No built-in save step                                        | `vmss-get` and `appservice-get` provide state blob |
| State restore (compensate) | Via `saveState-*` tags on resource                           | Via step replay with captured state blob           |

### ✅ Covered by azure-sql-scale-pack

| Resource                              | Bellhop approach                                                                      | Execution Ledger                                                                                                                 |
| ------------------------------------- | ------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| SQL Database SKU + capacity           | Tag + scaler module                                                                   | `sql-database-resize` action                                                                                                     |
| Elastic Pool SKU + capacity           | Tag + scaler module                                                                   | `sql-elastic-pool-resize` action                                                                                                 |
| State capture before scale            | No built-in save step                                                                 | `sql-database-get` and `sql-elastic-pool-get` provide state blob                                                                 |
| LicenseType (Hybrid Benefit)          | Bellhop issue #77: bug fix for DTU↔vCore switches                                     | Explicit `licenseType` in `sql-database-get` state blob + explicit `licenseType` in resize config                                |
| **Application Gateway v2**            | Full PUT required (no PATCH possible                                                  | ARM requires rebuilding full GW object from graph. See Bellhop issue #46. Complex; recommend separate connector pack.            |
| **AKS node pools**                    | Horizontal: scale to 0 system pool. Vertical: create new pool, migrate, tear down old | Not a connector; Kubernetes API surface, different persona. Recommend separate ops runbook.                                      |
| **API Management** (secondary region) | Bellhop #56, #9                                                                       | Secondary region scale is a different problem; each region is independent. Scope creep risk. Reject unless explicit requirement. |
| **IoT Hub**                           | Bellhop #58                                                                           | Different resource namespace. Reject unless explicit requirement.                                                                |
| **Event Grid trigger**                | Bellhop #57: emit events on every scale action                                        | Low effort: emit scale events to Event Hub from webhook or output step. High value: ops dashboards. Consider.                    |

## Triggers

### ✅ Covered

- Cron scheduler (timezone-aware): native to execution-ledger-scheduler
- Drasi continuous query via Event Hub: execution-ledger-worker reaction HTTP endpoint

### ❌ Not implemented

| Trigger                                                | Bellhop approach | Decision                                                                                                                                                                              |
| ------------------------------------------------------ | ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| \*\*Azure Monitor metric alert → direct queue/function | Bellhop #9       | For Event Hub → Drasi → EL path, Azure Monitor → Event Hub → Drasi is one extra hop. Azure Monitor alert rule → Event Hub → Drasi continuous query is a valid pattern. Document only. |
| \*\*Event Grid topic subscription on scale events      | Bellhop #57      | Document: scale events can be pushed to Event Grid. No connector needed; webhook delivery.                                                                                            |

## State Management

### Bellhop's approach (tag-based)

Bellhop wrote `saveState-*` tags **directly onto Azure resources** during scale-down. On scale-up, it read them back and restored. Pros: durable, self-contained. Cons: pollutes resource tags, requires write tags permission on resources.

### Execution Ledger's approach (automatic state blob propagation)

State captured by `vmss-get`/`appservice-get` is automatically propagated to the next step via `ctx.previous_output`. The resize step stores it as `stateBlob` in its output JSON so compensation can read it without manual wiring. Compensation calls `compensate()` which reads `stateBlob` from the persisted step output and invokes the reverse resize action.

**Tradeoff**: Bellhop's resource-tag state survives connector restarts. Execution Ledger's state blob is in step output. Both are durable (PG-backed). Execution Ledger's approach requires no tag mutation on target resources and the pipeline wiring is fully automatic, with no manual `previousSku` fields.

**Not implementing `save-state-to-resource-tag`**: adds write-tag RBAC requirement on production resources. Reject unless explicit requirement.

## Operational Patterns

### ✅ Covered

- Idempotency via step idempotency keys
- Replay/skip/compensate via execution-ledger API
- Correlation IDs on every ARM call
- MSI auth throughout
- Fail-closed connector config validation

### 🎯 Roadmap: Operational patterns from Bellhop issues

| Pattern                                                                    | Bellhop issue                         | Decision                                                                                                     |
| -------------------------------------------------------------------------- | ------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| **Oscillation detection** (scale-up/scale-down more than N times per hour) | Implicit in Bellhop, never formalized | Document as operational proof. Add KQL query in runbook.                                                     |
| **Custom tag prefix** (Bellhop's `tagPrefix`)                              | Bellhop #25                           | Config-only feature. Add `EXECUTION_LEDGER_AZURE_COMPUTE_TAG_PREFIX` env var pattern in runbook. Low effort. |
| **Daily keyword** in schedule (not just day-of-week)                       | Bellhop #8 (closed WON'T FIX)         | Cron handles this natively; daily = `0 20 * * *`. No change needed.                                          |
| **Custom role assignment names** (guid entropy)                            | Bellhop #68                           | IaC concern, not connector concern. Document RBAC minimum in runbook.                                        |

## Authentication / RBAC

### Bellhop's approach

Bellhop used a **single Contributor role** on the subscription plus Graph API queries to find tagged resources. This was scoped broadly.

### Execution Ledger's approach

MSI per-connector, least-privilege RBAC. Worker MSI needs `Reader` on subscription (to query resources) + `Virtual Machine Scale Set Contributor` on target RG (for resize).

**Bellhop issue #23** (PR #23): Bellhop moved to granular roles, AppConfigReader and StorageQueueDataMessageSender. Execution Ledger already enforces this. No action needed.

## Extensibility

### Bellhop: custom Docker scaler modules

Bellhop shipped a PowerShell module interface (`Update-Resource(graphData, tagData, direction`) and allowed custom scaler containers. Customers could extend without modifying core engine.

### Execution Ledger: connector packs + REST/ARM API extensibility

Execution Ledger's connector pack model is the equivalent extension point. No Docker container required for ARM-based connectors.

**Bellhop issue #26** (custom scalers outside the repo): Execution Ledger's REST connector covers any ARM resource without a dedicated connector. For non-ARM resources (SaaS APIs, custom endpoints), the `rest` connector is the extension path.

## What's intentionally out of scope

| Pattern                                        | Reason to reject                                                                                                                                  |
| ---------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| Scale to Spot instances (Bellhop #45)          | ARM API for Spot requires VM recreation. Risk of data loss. Execution Ledger doesn't have VMSS instance-level control for Spot placement. Reject. |
| MCR container publishing                       | Infrastructure concern, not orchestration. Execution Ledger ships MSI auth only, not containers. Out of scope.                                    |
| Deployment-time parameterization of tag names  | Config-time concern. `EXECUTION_LEDGER_*` env vars cover this. No action needed.                                                                  |
| Per-resource custom naming of role assignments | ARM template concern. Not a connector concern.                                                                                                    |
| Monthly schedule keyword                       | Cron handles this: `@monthly`. No action needed.                                                                                                  |

## Verification against Bellhop issues

| Bellhop issue                            | Status                                                                               |
| ---------------------------------------- | ------------------------------------------------------------------------------------ |
| #6 E2E test framework                    | Out of scope; ops proof scripts in Execution Ledger already cover chaos/DR proofing. |
| #9 Metric-driven scaling                 | Covered via Drasi + Event Hub path.                                                  |
| #10 AKS node pools                       | Out of scope (different API surface).                                                |
| #12 MCR publishing                       | Not applicable.                                                                      |
| #23 Role assignment least privilege      | Execution Ledger already MSI-only.                                                   |
| #24 Static role assignment guid          | Not applicable to Execution Ledger.                                                  |
| #25 Custom tag prefix                    | Add `EXECUTION_LEDGER_COMPUTE_TAG_PREFIX` pattern to runbook.                        |
| #26 Custom scalers                       | REST connector covers this.                                                          |
| #38 Role assignment cleanup on RG delete | Document RBAC concern in runbook.                                                    |
| #43 Monitoring workbook                  | Document KQL queries in runbook.                                                     |
| #45 Spot VM scaling                      | Reject; too risky without VM-level control.                                          |
| #46 Application Gateway                  | Low priority connector pack.                                                         |
| #47 Daily keyword + error isolation      | Cron handles daily. Error isolation already fail-closed.                             |
| #48 Unit tests                           | Covered by execution-ledger-core test suite.                                         |
| #57 Event Grid on scale events           | Webhook output step covers this.                                                     |
| #58 IoT Hub                              | Reject unless explicit requirement.                                                  |
| #68 Role assignment naming               | Not a connector concern.                                                             |
| #69 Advanced logging                     | Execution Ledger already emits correlation IDs, step state.                          |
| #70 Basic alerting                       | KQL queries in runbook.                                                              |
| #77 SQL LicenseType                      | SQL connector pack (future).                                                         |
| #80 Scaler container update              | Not applicable.                                                                      |
| #82 Devcontainers                        | Documentation-only concern.                                                          |
| #83 Devcontainers merged                 | Not applicable.                                                                      |
| #84 Timeout error                        | ARM async polling covers long-running operations.                                    |
| #85 E2E test clean                       | Out of scope; ops harness covers chaos.                                              |
| #86 Azure.Identity bump                  | Dependency maintenance concern. Execution Ledger uses MSI via azure_identity.        |
| #87–#91 Dependabot                       | Execution Ledger manages via Cargo.toml.                                             |
| #92 Archival notice                      | Bellhop archived. Execution Ledger is the successor intent.                          |
