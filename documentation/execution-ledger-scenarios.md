# Execution Ledger Scenario Catalog

## Purpose

This document maps where Execution Ledger fits, what scenario types it supports well, and when it is not the right primary platform.

Use this with `execution-ledger/README.md` for architecture and runtime details.

## Where Execution Ledger fits

Execution Ledger is best used as a durable execution and control plane for business workflows.

It is strongest when you need:

- replayable and idempotent step execution
- tenant and platform scoped control APIs
- operational recovery actions instead of manual database repair
- evidence-driven reliability checks before production promotion

Execution Ledger is not a model training platform, lakehouse, or analytics warehouse. It should orchestrate those systems, not replace them.

## Platform fit in a modern AI stack

| Layer | Primary systems | Execution Ledger role |
|---|---|---|
| Intelligence layer | Foundry, Azure OpenAI, Copilot Studio, model endpoints | Orchestrates calls, retries, and business-side compensation paths |
| Data and state layer | PostgreSQL, blob/object storage, queues | Consumes change or schedule triggers and coordinates durable work |
| Control and operations layer | API, worker, scheduler, Tauri operator | Native control plane with replay, skip, compensate, and recovery |
| Governance and proof layer | SLO, soak, chaos, DR, capacity/cost checks | Produces operational evidence for go/no-go decisions |

## Scenario catalog

| Scenario | Typical trigger mode | Common connector boundaries | Why Execution Ledger fits | Key controls to enable | Success signals |
|---|---|---|---|---|---|
| Legacy integration modernization | Schedule and event | `rest`, `queue`, `blob`, `sftp`, `erp` | Replaces script sprawl with durable run state and replay semantics | Step idempotency, run correlation, poison handling | Lower manual rework, faster controlled recovery |
| Change-driven orchestration | Postgres change stream (Drasi) | `rest`, `queue`, `blob` | Reacts to real state change instead of timer polling | Replication health, replay boundaries, backpressure controls | Reduced polling load, lower trigger latency |
| High-volume back-office processing | Queue-driven (`servicebus`) | `queue`, `erp`, `rest` | Works with intent queue and worker scaling model | KEDA queue scaling, dead-letter drain, lease recovery | Stable lag under burst, predictable throughput |
| Scheduled policy and compliance jobs | Cron/timezone scheduler | `rest`, `erp`, `blob` | Built-in scheduler and deterministic run path | Schedule lease recovery, bounded terminal-state GC | On-time execution, clear audit trail |
| Incident-safe operational actions | Operator-initiated API or Tauri action | Replay/skip/compensate and recovery endpoints | Exposes bounded runbook actions as platform operations | Scope-based auth, action audit fields, approval workflow | Lower MTTR, fewer manual DB interventions |
| AI inference workflow orchestration | Event or schedule | `rest` (model endpoint), `blob`, `queue` | Provides durable execution around AI calls and downstream system updates | Payload offload, retry policy, compensation design | Better completion reliability and traceability |
| RAG refresh and index update pipelines | Change-driven and scheduled | `blob`, `rest`, `queue` | Coordinates ingestion, embedding, indexing, and publish steps | Idempotent document keys, lag alerts, replay targeting | Faster refresh with fewer duplicate writes |
| Internal copilot action execution | MCP endpoint call | Platform job and recovery actions | Supports bounded agent actions through protected MCP surface | `mcp:execute` scope, tenant and platform separation | Safe automation with controlled blast radius |

## Additional enterprise scenarios

| Enterprise scenario | Typical trigger mode | Common connector boundaries | Why Execution Ledger fits | Key controls to enable | Success signals |
|---|---|---|---|---|---|
| Identity lifecycle orchestration (joiner/mover/leaver) | Event and schedule | `rest`, `queue`, `erp` | Coordinates multi-system provisioning and deprovisioning with explicit replay controls | Idempotent user/action keys, compensation for partial provisioning, approval logs | Reduced provisioning errors and faster deprovision completion |
| Procurement-to-pay automation | Schedule and event | `erp`, `rest`, `queue`, `blob` | Links invoice, approval, and payment workflows across platforms with durable state | Correlation IDs per invoice, failure routing, deadline alerts | Lower cycle time and reduced manual intervention |
| Order-to-cash exception handling | Event | `erp`, `rest`, `queue` | Handles retries and compensation when downstream systems fail or partially apply state | Step replay targeting, dead-letter handling, run-level audit | Reduced stuck orders and faster exception closure |
| Treasury or payment file processing | Schedule and file-drop event | `sftp`, `erp`, `queue`, `platform_health` | Supports file ingestion, transformation, and downstream dispatch with auditability | File uniqueness checks, signature/format validation, compensation path | Lower settlement failure rates and fewer duplicate sends |
| Regulatory report assembly and submission | Schedule | `blob`, `rest`, `erp`, `sftp` | Orchestrates deterministic report build pipelines with retry-safe submission | Versioned artifacts, approval checkpoints, immutable correlation trail | On-time submission and reduced audit findings |
| Data residency and boundary routing workflows | Event and schedule | `rest`, `queue`, `blob` | Enforces region or policy-specific routing at step boundaries | Region-tag checks, policy gates, explicit reject paths | Fewer policy violations and cleaner compliance evidence |
| Enterprise incident response automation | Operator and event | Platform recovery routes + `queue` | Runs standardized containment and recovery actions with explicit confirmations | Scoped operator auth, runbook-driven action sequencing, action audit | Lower MTTR and fewer unsafe manual actions |
| M&A system consolidation cutovers | Planned schedule + controlled events | `rest`, `erp`, `sftp`, `queue` | Provides repeatable migration waves with checkpointed outcomes and rollback hooks | Cutover checkpoints, replay windows, compensation strategy | Reduced cutover risk and predictable rollback capability |
| Vendor/partner onboarding pipelines | Manual trigger + event | `rest`, `sftp`, `erp`, `blob` | Standardizes partner setup and validation workflows with traceability | Contract validation steps, mandatory preflight, timeout/failure policy | Faster onboarding and fewer downstream integration failures |
| Security operations enrichment workflows | Event | `rest`, `queue`, `blob` | Chains enrichment, scoring, and escalation with durable retry/compensation semantics | Dedup keys per incident, latency SLOs, poison queue controls | Higher enrichment completion and faster triage |
| Retail promotion and inventory sync | Event and schedule | `erp`, `rest`, `queue`, `blob` | Coordinates catalog, pricing, and stock updates across channels with replay-safe correction paths | SKU-level idempotency, stale-data guards, compensation for downstream mismatch | Fewer stockout/oversell incidents and faster promo rollout |
| Retail returns and refund orchestration | Event | `rest`, `erp`, `queue` | Manages multi-step return, refund, and restock flows with explicit failure and retry behavior | Correlated return IDs, approval gates for high-value refunds, dead-letter policy | Lower refund error rates and shorter return resolution time |
| Manufacturing work-order and MES/ERP synchronization | Event and schedule | `erp`, `queue`, `rest`, `sftp` | Tracks production state changes and propagates them reliably across planning and execution systems | Work-order idempotency keys, cutover guards, replay/compensate playbooks | Reduced production data drift and fewer manual reconciliations |
| Manufacturing quality and traceability event pipelines | Event | `queue`, `blob`, `rest` | Captures and routes quality signals and traceability events with durable audit paths | Batch/lot correlation, immutable event capture, poison queue recovery | Faster quality incident containment and stronger compliance evidence |
| Azure VM and VMSS cost-optimization schedules | Cron/timezone scheduler | `azure_compute` | Resizes compute resources on a schedule (scale down weekends, scale up weekdays) with save-then-restore compensation | Pre-change state capture, compensation restore, schedule lease recovery | Predictable cost reduction, safe rollback on downstream failures |
| Azure SQL tier and elastic-pool migration | Cron/timezone scheduler or operator-initiated | `azure_sql` | Resizes SQL databases and elastic pools on a schedule or as one-off migrations (DTU→vCore, tier changes) | Pre-change state capture, compensation restore, correlation ID per migration | Safer migrations with automated rollback, audit trail per database |
| Multi-step webhook relay with transformation | Event (webhook) | `rest`, `transform`, `queue` | Receives a webhook payload, validates and transforms it, then fans out to multiple downstream systems with retry guarantees | Schema validation per step, idempotency on downstream calls, correlation propagation | Reliable webhook processing with fewer dropped events |
| Scheduled backup and artifact verification | Cron/timezone scheduler | `blob`, `rest` | Reads backup artifacts from blob storage, verifies integrity or freshness, and alerts on failures via REST | Checksum validation step, staleness thresholds, retry on transient access failures | Automated backup verification replacing manual spot checks |
| Certificate lifecycle orchestration | Schedule and event | `rest`, `blob`, `queue` | Generates CSR, submits to CA, deploys certificate to Key Vault or App Service, verifies binding, and compensates on verification failure | Step idempotency, compensation path for failed bindings, expiry alerts via scheduler | Fewer expired-certificate incidents, repeatable rotation process |
| Compliance evidence collection and assembly | Cron/timezone scheduler | `blob`, `rest`, `queue`, `transform` | Collects evidence from multiple systems on a schedule, assembles a structured report, stores it in blob, and notifies stakeholders | Versioned artifacts, approval checkpoints, immutable correlation trail | Automated compliance reporting replacing manual evidence gathering |
| Multi-tenant API product provisioning | Tenant-onboarding event or operator-initiated | `rest` | Provisions API products in APIM for new tenants: create product, add APIs, assign subscription keys, configure rate limits | Tenant-scoped provisioning, compensation path for partial provisioning, idempotent product keys | Faster tenant onboarding with consistent API governance |
| DR runbook automation | Operator-initiated or scheduled | `azure_compute`, `azure_sql`, `rest` | Runs standardized disaster recovery steps: validate replication health, trigger planned failover, verify endpoint reachability, collect evidence | Step replay for failed DR steps, correlation ID across all DR actions, audit trail | Repeatable DR execution replacing manual runbooks |
| Fabric multi-pipeline medallion orchestration | Schedule or event | `rest` | Triggers Fabric bronze → silver → gold Data Factory pipelines in sequence, polls completion, verifies output row counts | Step replay for individual pipeline triggers, correlation ID spanning all Fabric jobs, compensation path for partial success | Reliable medallion processing with cross-pipeline visibility |
| Fabric Eventstream → multi-system fan-out | Event (webhook from Fabric) | `rest`, `queue`, `erp`, `blob` | Receives Fabric Eventstream webhook, orchestrates actions across Fabric (workspace provisioning) and non-Fabric systems (ERP, email, blob) | Step idempotency per event, compensation for partial provisioning, correlation across Fabric and external systems | Unified event processing spanning Fabric and enterprise systems |
| Fabric Data Activator → automated remediation | Event (Activator webhook) | `rest`, `azure_compute` | Fabric Activator detects a threshold breach (warehouse refresh latency, pipeline failure rate), triggers Execution Ledger recovery job | Step replay, correlation ID, operator approval gate for destructive actions | Automated SRE response to Fabric health signals |

## AI-specific scenario examples

### AI scenario 1: customer ticket classification and routing

1. Trigger on new ticket write.
2. Call model endpoint via `rest` connector.
3. Persist classification result and confidence.
4. Route downstream queue intent for assignment.
5. Replay only failed enrichment step when needed.

Execution Ledger value is durability and controlled recovery around the model call, not model hosting.

### AI scenario 2: document pipeline for retrieval workloads

1. Trigger from data change or schedule.
2. Extract content and run enrichment.
3. Call embedding or indexing endpoint.
4. Persist stage outcomes with correlation ID.
5. Compensate or replay targeted failed steps.

Execution Ledger value is predictable orchestration and repeatability across large batch updates.

### AI scenario 3: bounded agent operations

1. Internal agent calls MCP operation.
2. Execution Ledger validates scope and route.
3. Workflow runs under normal replay and audit controls.
4. Operator can recover with platform controls if needed.

Execution Ledger value is controlled execution under clear auth and operational boundaries.

## APIM placement guidance

APIM should usually be placed in front of Execution Ledger for enterprise environments that need centralized API governance.

Use APIM when you need:

- centralized auth policy enforcement and token mediation
- subscription/rate limiting and consumer governance
- standardized API productization for internal platform consumers
- additional threat protection and policy-based request controls

Execution Ledger remains the execution and runtime control plane behind APIM.

Recommended split:

| Responsibility | APIM | Execution Ledger |
|---|---|---|
| Consumer access control, policies, quotas | Yes | No |
| Workflow trigger and run-state orchestration | No | Yes |
| Replay/skip/compensate and recovery operations | No | Yes |
| Tenant/platform runtime authorization checks | Shared (edge + service) | Yes |


## Where Execution Ledger is a weak fit

Execution Ledger should not be the first choice when the requirement is:

- interactive sub-second UI orchestration only
- large-scale model training lifecycle management
- real-time stream analytics where a streaming engine is the core requirement
- unconstrained ad hoc data exploration

In those cases, use specialized systems and integrate through connector boundaries.

## Adoption patterns

### Pattern A: migration first

Start with one high-friction legacy integration, then move adjacent flows after recovery and reliability confidence is proven.

### Pattern B: event-first modernization

Start with change-driven workflows where polling cost or delay is the main pain, then layer scheduled and queue paths.

### Pattern C: AI operations hardening

Start with AI workflows that already exist but lack replay, audit, or compensation controls, then fold them into a governed execution path.

### Pattern D: BizTalk/Logic Apps replacement

Replace script-and-connector sprawl with a single orchestration runtime:

1. API receives a business event (file received, webhook, ERP event)
2. Normalize to a canonical intent payload (`job_id`, trace metadata, tenant context)
3. Worker executes connector steps: protocol adaptation (SFTP, HTTPS, queue), transformation mapping, delivery with retry and backoff, final state commit
4. Result state and attempts are queryable in one runtime model

Example — outbound payment file processing replacing a Logic Apps + Function Apps chain:

1. API receives "payment batch ready" intent
2. Worker steps: fetch source data, build bank-specific file format, encrypt and sign, upload to SFTP or bank endpoint, poll for acknowledgement
3. Reliability: retry transient network failures, dead-letter unretryable payloads, retain output metadata and evidence pointers
4. Operations: query run and attempt history via API, replay failed intents after correction, schedule reconciliation jobs

## Minimum onboarding checklist per scenario

1. Define run trigger mode: schedule, queue, change-driven, or operator initiated.
2. Define connector-step contract and idempotency key shape.
3. Define required scopes and tenant/platform boundary rules.
4. Define failure policy: retry, dead-letter, replay target, compensation rule.
5. Define success and health metrics before go-live.
