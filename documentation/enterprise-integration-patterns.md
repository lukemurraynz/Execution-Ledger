# Enterprise Integration Patterns in Execution Ledger

Maps the classic enterprise integration patterns (Hohpe and Woolf) to Execution Ledger's architecture and connector model. Each entry describes how the pattern is implemented, not just what it is.

## Messaging Patterns

### Guaranteed Delivery
**How:** Every intent is enqueued to Service Bus (or local Postgres queue) before the worker processes it. Service Bus Standard tier provides at-least-once delivery with persistence. The worker acks only after step execution commits to the database.

**Where:** `execution-ledger-core/src/queue.rs` — `send_intent` enqueues; worker claims with peek-lock and completes on success.

### Dead Letter Channel
**How:** Intents that exceed retry limits move to the Service Bus dead-letter queue (or local poison queue). Recovery APIs drain, inspect, and reprocess dead-lettered messages without manual database surgery.

**Where:** `/platform/recovery/servicebus-poison`, `/platform/recovery/local-poison` endpoints. Worker retry loop with backoff in `execution-ledger-worker/src/main.rs`.

### Idempotent Receiver
**How:** Every connector step carries an idempotency key derived from `run_id + step_id`. Replaying a step re-executes only that step; other steps in the run are untouched. The key is stored in `EXECUTION_LEDGER_steps_runs` and checked before execution.

**Where:** Worker step execution loop, `parse_replay_step_ids` in `execution-ledger-worker/src/main.rs`.

### Correlation Identifier
**How:** A correlation ID is set at run creation and propagated into every step execution context and output. Trace a single request across API → Service Bus → Worker → Scheduler → Webhook in Log Analytics with one KQL query.

**Where:** Run metadata in `EXECUTION_LEDGER_jobs_runs`, step output in `EXECUTION_LEDGER_steps_runs`.

### Publish-Subscribe Channel
**How:** Service Bus topics and subscriptions enable fan-out. A single trigger can publish to a topic; multiple subscriptions deliver to different downstream workers or systems. The `queue` connector can send to topics as well as queues.

**Where:** Service Bus namespace configuration in `infra/modules/data.bicep`. Queue connector can target topics.

### Claim Check
**How:** Large step payloads can be offloaded to blob or filesystem storage. The intent carries a reference to the offloaded payload, not the payload itself. `EXECUTION_LEDGER_OFFLOAD_DIR` configures the offload target.

**Where:** `execution-ledger-core` offload module, `EXECUTION_LEDGER_OFFLOAD_INLINE_MAX_BYTES` threshold in App Configuration.

### Message Filter
**How:** JSON Schema validation at every step boundary acts as a content filter. Steps with `inputSchema` reject invalid payloads before execution. Steps with `outputSchema` reject the connector's output if it does not match the expected shape.

**Where:** `connectors/schema.rs` — `validate_input` and `validate_output` run in `execute_step()`.

## Routing Patterns

### Content-Based Router
**How:** The `transform` connector reads upstream step output, inspects values through dot-notation field mapping, and routes to different downstream connectors or URLs based on field content.

**Example:** After a payment validation step, route approved payments to `erp` connector and declined payments to `rest` connector with a notification URL.

**Where:** `connectors/transform.rs` — `map` field with dot-notation source paths.

### Routing Slip
**How:** Every job definition is a routing slip. The `steps` array specifies the sequence of connectors, each with its own configuration. The worker executes steps in order, with each step's output becoming the next step's `previous_output`.

**Example:**
```json
{"steps": [
  {"id": "fetch", "connector": "azure_blob"},
  {"id": "validate", "connector": "rest", "inputSchema": {...}},
  {"id": "transform", "connector": "transform", "config": {"map": {...}}},
  {"id": "deliver", "connector": "sftp"}
]}
```

**Where:** Job definition contract in `execution-ledger-types`.

### Scatter-Gather
**How:** A `queue` connector step publishes to a Service Bus topic; multiple subscriber workers or downstream systems process the event independently. A subsequent step (triggered by a Drasi WAL change or scheduled check) gathers results.

**Where:** `queue` connector with topic target, combined with Drasi WAL streaming for result gathering.

## Transformation Patterns

### Message Translator
**How:** The `transform` connector reshapes JSON payloads between steps. Specify a `map` object with dot-notation source paths and destination paths. Schema validation runs before and after transformation.

**Example:**
```json
{"map": {
  "customer.name": "source.customer_name",
  "order.total": "source.order_amount",
  "order.id": "source.txn_id"
}}
```

**Where:** `connectors/transform.rs`.

### Canonical Data Model
**How:** Each connector pack defines its own input and output schema. The `transform` connector converts between external system formats and the internal canonical shape. JSON Schema validation at every boundary enforces the contract.

**Where:** `pack.json` in each connector pack defines its schema contract.

### Envelope Wrapper
**How:** Every intent carries metadata (tenant ID, correlation ID, timestamp, run ID) alongside the payload. The worker enriches step outputs with execution context. Webhook events carry the full envelope.

**Where:** Intent struct in `execution-ledger-types`, webhook payload in `execution-ledger-drasi`.

## Transaction Patterns

### Process Manager / Saga
**How:** Execution Ledger's replay, skip, and compensate controls implement the saga pattern. Long-running workflows execute steps sequentially. If a later step fails, the operator can replay the failed step (forward recovery) or compensate completed steps in reverse order (backward recovery).

**Where:** Operator endpoints: `POST /platform/jobs/{id}/replay`, `POST /platform/jobs/{id}/skip`, `POST /platform/jobs/{id}/compensate`.

### Compensating Transaction
**How:** When compensation is triggered, the worker queries completed steps in reverse order and invokes each connector's `compensate()` function. Read-only connectors are no-ops. REST connectors call a configured `compensateUrl`. Infrastructure connectors (`azure_compute`, `azure_sql`) restore state from `stateBlob` captured before the mutation.

**Where:** `connectors/mod.rs` — `compensate()` trait method. Each connector implements its own compensation logic.

### Reservation / Checkpoint
**How:** Steps capture state before mutation (`stateBlob`). A prior step (e.g., `vm-get`) reads current state into the pipeline. A later step (`vmss-resize`) captures the pre-resize state. If compensation is needed, the original state is restored from the saved blob.

**Where:** `connectors/azure_compute.rs` — `vm-save-state` action, state blob propagation through `ctx.previous_output`.

## System Management Patterns

### Control Bus
**How:** Platform-scoped endpoints (`/platform/*`) form a control bus separate from the tenant-scoped data plane. Recovery actions (drain poison, release stale leases, run GC) execute through this bus, not through tenant APIs.

**Where:** API route separation: tenant routes (`/jobs/*`) vs platform routes (`/platform/recovery/*`, `/platform/gc`).

### Wire Tap
**How:** Drasi reads PostgreSQL WAL changes for tracked tables and pushes completed/failed job-run events to a configured webhook. This provides an observation channel without modifying the data plane. Correlation IDs enable tracing across the tap.

**Where:** `execution-ledger-drasi` service, WAL replication slot, webhook configuration.

### Scheduler / Polling Consumer
**How:** The scheduler claims due schedules with `FOR UPDATE SKIP LOCKED`, enqueues intents directly to Service Bus, and updates `next_run` from cron expressions. As a secondary path, Drasi monitors schedule records via the WAL for reactive enqueue. For non-WAL scenarios, the worker can poll Service Bus or local Postgres queue.

**Where:** `execution-ledger-scheduler` (cron evaluation), `execution-ledger-worker` (queue consumption), `execution-ledger-drasi` (WAL streaming).

### Test Message
**How:** The `platform_health` connector validates platform connectivity and readiness without affecting production data. Use it as a smoke test step in any job definition. The preflight CLI command runs platform health checks before first job trigger.

**Where:** `connectors/platform_health.rs`, `cargo run -p execution-ledger-cli -- preflight`.

## Integration Styles

Execution Ledger supports four integration styles natively:

| Style | How | Connectors |
|---|---|---|
| **File Transfer** | Read/write files via SFTP or blob storage on a schedule or event trigger | `sftp`, `azure_blob` |
| **Shared Database** | All services share PostgreSQL with tenant-scoped queries. Drasi streams WAL changes. | `execution-ledger-core/src/db.rs` |
| **Remote Procedure Call** | REST connector calls external APIs with managed identity. Transform connector reshapes payloads. | `rest`, `transform` |
| **Messaging** | Service Bus queues for point-to-point, topics for pub/sub. Queue connector sends/receives. | `queue` |

## Pattern Composition Example: Order Fulfillment

Combining multiple patterns into a single job:

```
Order received (webhook)
  → Content-Based Router: route to region-specific warehouse
  → Message Translator: transform order to warehouse format
  → Guaranteed Delivery: enqueue to warehouse Service Bus queue
  → Claim Check: offload large order payload to blob
  → Compensating Transaction: if warehouse rejects, refund payment via REST
  → Wire Tap: Drasi emits fulfillment-complete webhook event
  → Correlation Identifier: trace entire flow with single correlation ID
```

## Azure SRE Agent Integration

[Azure SRE Agent](https://learn.microsoft.com/en-us/azure/sre-agent/overview) automates incident triage, scheduled operations, and site reliability workflows. Execution Ledger complements it as the durable execution engine behind SRE Agent's response plans. Where SRE Agent diagnoses and proposes, Execution Ledger executes and recovers.

### Integration Pattern 1: SRE Agent Triggers Execution Ledger Recovery

SRE Agent detects an incident, diagnoses root cause, and proposes mitigation. The mitigation is a multi-step Execution Ledger job with durability, replay, and compensation guarantees.

```
Alert fires (Azure Monitor, PagerDuty, ServiceNow)
  → SRE Agent correlates signals across App Insights, Log Analytics, Grafana
  → SRE Agent identifies root cause and proposes mitigation
  → SRE Agent calls Execution Ledger API (HTTP trigger or MCP) to launch recovery job
  → Execution Ledger executes recovery steps with idempotency and retry
  → Execution Ledger reports status back to SRE Agent
  → SRE Agent updates incident ticket with recovery evidence
```

**Why this split:** SRE Agent owns investigation and diagnosis. Execution Ledger owns durable multi-step execution with replay, skip, compensate, and audit trail. SRE Agent's response plans define *what* to do; Execution Ledger provides *how* to do it reliably.

**Concrete example — VM scale recovery:**
1. SRE Agent detects CPU saturation on a VMSS via Azure Monitor
2. SRE Agent proposes: scale out VMSS from 2 to 5 instances
3. Human approves in SRE Agent
4. SRE Agent calls Execution Ledger MCP: `POST /platform/jobs/trigger` with `azure_compute` job
5. Execution Ledger: get-current-SKU → scale-out → verify-instance-count → if verify fails, compensate back to 2 instances
6. Execution Ledger reports completion via webhook
7. SRE Agent updates the incident: "VMSS scaled to 5 instances, verified healthy"

### Integration Pattern 2: Execution Ledger MCP as an SRE Agent Tool

Execution Ledger exposes operator actions through its MCP endpoint (`/api/mcp/server`). SRE Agent connects to this as an MCP server and discovers available tools with scope annotations.

```
SRE Agent
  → Connects to Execution Ledger MCP server
  → Discovers tools: trigger-job, replay-step, skip-step, compensate-run, check-status
  → Each tool carries readOnlyHint, destructiveHint, idempotentHint annotations
  → SRE Agent uses tools during investigation and remediation
  → All MCP actions are scoped to mcp:execute and logged with correlation IDs
```

**Why MCP over REST:** Tool discovery means SRE Agent automatically learns what Execution Ledger can do without hardcoded API paths. Scope annotations (`destructiveHint: true`) help SRE Agent's permission gate decide whether an action needs human approval.

### Integration Pattern 3: Execution Ledger Drasi Webhook → SRE Agent HTTP Trigger

Execution Ledger's Drasi service emits completed and failed job-run events to a webhook. SRE Agent receives these via HTTP trigger and automatically investigates failures.

```
Execution Ledger job completes/fails
  → Drasi reads WAL change from PostgreSQL
  → Drasi pushes webhook event to configured URL
  → SRE Agent HTTP trigger receives the event
  → SRE Agent investigates: queries logs, checks deployment history, correlates signals
  → If failure: SRE Agent proposes replay or compensation of the failed job
  → If success: SRE Agent records the evidence for compliance
```

### Integration Pattern 4: SRE Agent Approval Gate + Execution Ledger Execution

Complex remediations need human approval before execution. SRE Agent handles the approval workflow; Execution Ledger handles the execution.

```
1. Incident fires
2. SRE Agent investigates, proposes multi-step remediation
3. SRE Agent presents proposal to on-call engineer with:
   - What steps will execute
   - What the compensation plan is
   - Estimated blast radius
   - Rollback path
4. Human approves in SRE Agent (or rejects/modifies)
5. SRE Agent triggers Execution Ledger job
6. Execution Ledger executes with full durability:
   - Each step is idempotent
   - Failed steps can be replayed individually
   - Committed steps can be compensated in reverse
   - Full audit trail with correlation ID
7. Execution Ledger reports completion
8. SRE Agent closes the incident with evidence attached
```

### Integration Pattern 5: SRE Agent Scheduled Tasks + Execution Ledger Scheduled Jobs

SRE Agent and Execution Ledger both have schedulers. Use them for different concerns:

| Concern | Use SRE Agent | Use Execution Ledger |
|---|---|---|
| Health checks and compliance sweeps | Yes — investigation and reporting | No |
| Multi-step orchestration with recovery | No | Yes — durable execution with replay/compensate |
| Incident detection and diagnosis | Yes — root cause analysis | No |
| VM/VMSS cost-optimization schedules | No | Yes — save-then-restore compensation |
| Backup verification workflows | No | Yes — multi-step: read blob → verify → alert |
| Certificate lifecycle orchestration | No | Yes — generate → submit → deploy → verify → compensate |
| Proactive topology scans | Yes — "what changed?" queries | No |
| DR runbook automation | Yes — approval gate | Yes — step execution with audit trail |

**Combined pattern:** SRE Agent runs a scheduled health check. It detects a degraded component. It proposes remediation: trigger an Execution Ledger job. Human approves. Execution Ledger executes the remediation. SRE Agent verifies recovery.

### Decision Guide: When to Use Which

| If you need... | Use... |
|---|---|
| Incident detection, diagnosis, root cause analysis | Azure SRE Agent |
| Durable multi-step execution with replay and compensation | Execution Ledger |
| Human approval workflows for operational actions | SRE Agent (approval gate) → Execution Ledger (execution) |
| Automated investigation of job failures | SRE Agent (consuming Drasi webhooks) |
| Audit trail for operational actions | Execution Ledger (correlation IDs, step history) |
| Cost optimization schedules with rollback | Execution Ledger (azure_compute connector + compensation) |
| Natural-language operational queries ("why is this slow?") | Azure SRE Agent |

### Architecture Diagram

```
                    ┌─────────────────────────┐
                    │    Azure SRE Agent       │
                    │                          │
                    │  ┌─────────────────────┐ │
                    │  │  Response Plans     │ │
                    │  │  Scheduled Tasks    │ │
                    │  │  HTTP Triggers      │ │
                    │  │  MCP Clients        │ │
                    │  └─────────┬───────────┘ │
                    │            │             │
                    │  ┌─────────▼───────────┐ │
                    │  │  Permission Gate    │ │
                    │  │  (Human Approval)   │ │
                    │  └─────────┬───────────┘ │
                    └────────────┼─────────────┘
                                 │
              ┌──────────────────┼──────────────────┐
              │                  │                  │
              ▼                  ▼                  ▼
    ┌─────────────────┐ ┌──────────────┐ ┌──────────────────┐
    │  MCP Protocol   │ │  HTTP/REST   │ │  Webhook (Drasi) │
    │  (tool calls)   │ │  (job trigger)│ │  (job events)    │
    └────────┬────────┘ └──────┬───────┘ └────────┬─────────┘
             │                 │                   │
             └─────────────────┼───────────────────┘
                               │
                               ▼
                    ┌──────────────────────┐
                    │   Execution Ledger   │
                    │                      │
                    │  API → SB → Worker   │
                    │  Scheduler → Drasi   │
                    │  Replay / Skip /     │
                    │  Compensate / Audit  │
                    └──────────────────────┘
```

## Execution Ledger in the Agentic AI World

Execution Ledger occupies a specific role in the agentic ecosystem: the **durable execution layer** that AI agents call through MCP to trigger, monitor, and recover multi-step workflows.

### The MCP Tool Surface

Execution Ledger exposes operator actions as MCP tools with scope-aware discovery. An AI agent connecting to the MCP endpoint only sees tools its Entra ID token authorizes.

| Tool | Scope required | Annotations | What it does |
|---|---|---|---|
| `execution_ledger.health` | none (public) | read-only | Platform health check |
| `execution_ledger.jobs.get` | `jobs:read:own` or `jobs:read:any` | read-only | Get a single job run by ID (tenant-scoped) |
| `execution_ledger.jobs.list` | `jobs:read:own` or `jobs:read:any` | read-only | List job runs with optional status filter (tenant-scoped) |
| `execution_ledger.jobs.get_steps` | `jobs:read:own` or `jobs:read:any` | read-only | Get step-level execution details for a run |
| `execution_ledger.schedules.list` | `platform:read` | read-only | List all configured schedules |
| `execution_ledger.recovery.runs` | `platform:write` | destructive | Recover stale run leases |
| `execution_ledger.schedules.enable` | `platform:write` | read-write | Enable a schedule |
| `execution_ledger.schedules.disable` | `platform:write` | read-write | Disable a schedule |

Every tool carries `readOnlyHint`, `destructiveHint`, and `idempotentHint` annotations. An AI agent's permission gate can use these to decide whether an action needs human approval — a destructive tool like `recovery.runs` should require confirmation before execution.

### Integration Pattern 6: AI Agent → MCP → Execution Ledger

Any MCP-compatible AI agent (GitHub Copilot, Claude Desktop, Copilot Studio, Azure AI Foundry agent, custom agent) can connect to Execution Ledger as an MCP server.

```
AI Agent (Copilot / Foundry / Claude / Custom)
  → Connects to Execution Ledger MCP server
  → Calls tools/list → receives scope-filtered tool descriptors
  → Calls tools/call with appropriate arguments
  → Execution Ledger validates scope, executes, returns result
  → All actions are audited with correlation IDs
```

**Why MCP over REST:** Tool discovery means the agent learns what it can do without hardcoded API paths. Scope filtering means the agent only sees tools it is authorized to call. Annotations inform the agent's permission gate about risk.

**Concrete example — Copilot troubleshooting a failed job:**
1. User: "Why did job abc-123 fail?"
2. Copilot calls `execution_ledger.jobs.get` with `runId: "abc-123"`
3. Copilot sees status: `failed`, current attempt: 3/3
4. Copilot calls `execution_ledger.jobs.get_steps` to inspect step-level failures
5. Copilot identifies step 2 ("transform") failed with schema validation error
6. Copilot explains the failure to the user and proposes: replay step 2 with corrected input
7. User approves
8. Copilot calls Execution Ledger API to replay the step (or the user triggers it manually)

### Integration Pattern 7: Agent-to-Orchestrator Handoff

When an AI agent needs to execute a complex multi-step workflow with durability guarantees, it hands off to Execution Ledger rather than executing steps itself.

**Why hand off:** AI agents are good at diagnosis and decision-making. They are not good at durable execution with retry, idempotency, compensation, and audit. Execution Ledger provides those guarantees.

```
User asks AI agent to "onboard this new tenant"
  → Agent diagnoses: needs API product, database schema, storage container, DNS record
  → Agent constructs an Execution Ledger job definition with 4 connector steps
  → Agent triggers the job via MCP or REST
  → Agent polls job status via execution_ledger.jobs.get
  → If any step fails: agent can replay the failed step or compensate completed steps
  → Agent reports final status to user with full audit trail
```

### Integration Pattern 8: Multi-Agent Task Delegation

In a Foundry or Copilot Studio setup with multiple specialized agents, Execution Ledger serves as the shared execution bus.

```
Orchestrator Agent
  → Receives "remediate security finding on VMSS-01"
  → Delegates diagnosis to Security Agent (read-only: analyze NSG, check patches)
  → Security Agent reports: "NSG rule allows port 22 from 0.0.0.0/0"
  → Orchestrator constructs Execution Ledger job:
      Step 1: get-current-nsg (azure_compute connector)
      Step 2: remove-port-22-rule (REST connector to ARM API)
      Step 3: verify-nsg (azure_compute connector)
      Compensate: restore-original-nsg from stateBlob
  → Orchestrator triggers job via MCP
  → Orchestrator polls status, reports to user
  → All actions are in Execution Ledger's audit trail, not scattered across agent logs
```

### Pattern Summary: Where Agents End and Execution Begins

| Concern | AI Agent | Execution Ledger |
|---|---|---|
| Natural language understanding | Yes | No |
| Diagnosis and root cause analysis | Yes | No |
| Constructing multi-step workflow definitions | Yes (generates job JSON) | No (validates and executes) |
| Durable step execution with retry | No | Yes |
| Step-level idempotency and replay | No | Yes |
| Coordinated compensation (undo) | No | Yes |
| Audit trail with correlation IDs | No | Yes |
| Human approval for destructive actions | Agent's permission gate | MCP scope enforcement |

## Microsoft Fabric Integration

Microsoft Fabric provides Data Factory pipelines, notebooks, Eventstreams, and Data Activator. Execution Ledger orchestrates these as durable, replayable connector steps through Fabric's REST API.

### Why Orchestrate Fabric Workloads Through Execution Ledger

Fabric Data Factory pipelines have built-in retry and scheduling. Execution Ledger adds:

- **Step-level replay across Fabric items.** Replay a single failed pipeline step without rerunning the entire chain.
- **Cross-system compensation.** If a Fabric pipeline succeeds but a downstream ERP call fails, compensate the Fabric pipeline's effects.
- **Unified audit trail.** Correlation IDs span Fabric pipeline runs, REST API calls, and blob storage operations.
- **Tenant-scoped orchestration.** One Execution Ledger tenant triggers Fabric pipelines in workspace A; another tenant cannot see or modify those jobs.
- **MCP access for AI agents.** AI agents query Fabric pipeline status and trigger reruns through Execution Ledger's MCP tools.

### Fabric REST API Surface (Relevant to Execution Ledger)

| Operation | Endpoint | Execution Ledger Connector |
|---|---|---|
| Run pipeline on demand | `POST /v1/workspaces/{id}/items/{id}/jobs/instances?jobType=Pipeline` | `rest` connector with managed identity |
| Get pipeline job status | `GET /v1/workspaces/{id}/items/{id}/jobs/instances/{jobId}` | `rest` connector (poll step) |
| Cancel pipeline job | `POST /v1/workspaces/{id}/items/{id}/jobs/instances/{jobId}/cancel` | `rest` connector (compensation step) |
| Schedule pipeline | `POST /v1/workspaces/{id}/items/{id}/jobs/DefaultJob/schedules` | Execution Ledger scheduler (alternative to Fabric's built-in scheduler) |
| Query activity runs | `POST /v1/workspaces/{id}/datapipelines/pipelineruns/{jobId}/queryactivityruns` | `rest` connector (verification step) |

All Fabric REST API calls use Entra ID bearer tokens — the same auth model as Execution Ledger. The `rest` connector uses `ManagedIdentityCredential` for `https://api.fabric.microsoft.com/.default`.

### Integration Scenario 1: Multi-Pipeline Medallion Orchestration

Orchestrate a bronze → silver → gold Lakehouse pipeline chain with replay and compensation.

```
Execution Ledger job:
  Step 1: Trigger Fabric bronze pipeline (REST → POST jobs/instances)
  Step 2: Poll bronze pipeline status (REST → GET jobs/instances/{jobId})
  Step 3: If bronze succeeded, trigger silver pipeline (REST)
  Step 4: Poll silver pipeline status (REST)
  Step 5: If silver succeeded, trigger gold pipeline (REST)
  Step 6: Verify gold table row count (REST → OneLake or Warehouse SQL endpoint)
  Compensate: If verification fails, log failure for investigation
  (Data pipelines are not directly compensatable, but downstream effects can be)
```

### Integration Scenario 2: Fabric Eventstream → Execution Ledger → Multi-System Fan-Out

Fabric Eventstream captures a business event. Execution Ledger receives it and orchestrates actions across Fabric and non-Fabric systems.

```
Fabric Eventstream detects "new customer signed up"
  → Eventstream pushes to Execution Ledger webhook endpoint
  → Execution Ledger triggers job:
      Step 1: Provision Fabric workspace for customer (REST → Fabric Admin API)
      Step 2: Add customer to ERP (ERP connector)
      Step 3: Send welcome email (REST connector → SendGrid/Graph API)
      Step 4: Create initial Lakehouse with shortcut (REST → Fabric API)
      Compensate: If ERP step fails, deprovision Fabric workspace
```

### Integration Scenario 3: Execution Ledger Scheduler Replacing Fabric Pipeline Schedules

Fabric pipelines support cron schedules natively. Use Execution Ledger's scheduler when you need cross-system orchestration that Fabric's scheduler cannot express.

**Use Fabric's built-in scheduler when:** The pipeline only affects Fabric items, retry is handled by Fabric, and you do not need cross-system compensation.

**Use Execution Ledger's scheduler when:** The pipeline is part of a multi-system workflow (Fabric + ERP + SFTP + REST), you need step-level replay across systems, or you need compensation that involves non-Fabric resources.

### Integration Scenario 4: AI Agent Queries Fabric Pipeline Status via Execution Ledger MCP

An AI agent wants to know why a Fabric pipeline failed. It does not have direct Fabric API access. It calls Execution Ledger's MCP tools.

```
AI Agent: "Why did the gold pipeline fail last night?"
  → Agent calls execution_ledger.jobs.list (filter: status=failed)
  → Agent finds run abc-456: "fabric-gold-pipeline"
  → Agent calls execution_ledger.jobs.get_steps(runId: abc-456)
  → Step 3 ("verify-gold-table") shows: Fabric pipeline completed but row count is zero
  → Agent suggests: "The bronze pipeline may have produced no new data. Check the source system."
```

### Integration Scenario 5: Fabric Data Activator → Execution Ledger Recovery

Fabric Data Activator detects a condition (e.g., warehouse refresh latency exceeds threshold) and triggers an Execution Ledger recovery job.

```
Fabric Data Activator rule: "warehouse refresh duration > 30 minutes"
  → Activator sends webhook to Execution Ledger API
  → Execution Ledger triggers recovery job:
      Step 1: Check warehouse health (REST → Fabric API)
      Step 2: If unhealthy, pause downstream pipelines (REST → Fabric Job Scheduler API)
      Step 3: Notify on-call (REST → PagerDuty/Teams webhook)
      Step 4: Wait for manual resolution (operator skip or manual trigger)
  → Operator resolves the warehouse issue
  → Operator replays step 4 → job resumes downstream pipelines
```

### Decision Guide: Execution Ledger vs Fabric Built-in Orchestration

| If you need... | Use... |
|---|---|
| Schedule a single Fabric pipeline | Fabric built-in scheduler |
| Orchestrate Fabric + non-Fabric systems in one workflow | Execution Ledger |
| Step-level replay across pipeline boundaries | Execution Ledger |
| Cross-system compensation (undo Fabric + ERP + REST) | Execution Ledger |
| AI agents to query or trigger Fabric jobs | Execution Ledger MCP |
| Event-driven trigger from Fabric Eventstream to multi-system | Execution Ledger webhook + job |
| Fabric Data Activator alert → complex multi-step remediation | Execution Ledger recovery job |

### Drasi and Fabric Data Activator: How They Relate

Drasi and Fabric Data Activator both detect change and trigger actions, but they operate at fundamentally different layers. They complement each other; they do not replace each other.

**Drasi (in Execution Ledger):**
- **What it watches:** PostgreSQL WAL (Write-Ahead Log) — raw database-level INSERT, UPDATE, DELETE operations
- **How it detects:** Logical replication slot tails the WAL stream; every committed change is captured exactly once
- **What it emits:** Webhook events containing the changed row data (old and new values for UPDATEs)
- **Where it runs:** Container App alongside Execution Ledger, singleton (one replication slot)
- **Latency model:** Near-real-time (WAL shipping delay, typically sub-second)

**Fabric Data Activator:**
- **What it watches:** Fabric data — Eventstream events, KQL query results, Power BI measure values, pipeline run outputs
- **How it detects:** Evaluates conditions on data: threshold comparisons, pattern matching, aggregations over time windows
- **What it triggers:** Email, Teams notification, Fabric pipeline/notebook/dataflow/spark job execution, custom Power Automate flow
- **Where it runs:** Fabric SaaS platform, no infrastructure to manage
- **Latency model:** Configurable window and step size (e.g., "average over 5 minutes, check every 5 minutes")

**The core distinction:** Drasi captures *that* a database row changed. Activator evaluates *whether* data meets a condition.

#### When to Use Which

| Concern | Drasi (EL) | Fabric Activator |
|---|---|---|
| "A job run just completed" | Yes — WAL change on `EXECUTION_LEDGER_jobs_runs` | No — not connected to PG |
| "A job run just failed" | Yes — status column changed to `failed` | No |
| "Gold table row count dropped below 100K" | No — Drasi watches PG, not Fabric tables | Yes — KQL query on Warehouse |
| "Average warehouse refresh time exceeded 30 min over the last hour" | No | Yes — aggregation over time window |
| "A specific row was updated in my operational DB" | Yes — exact row, exact columns changed | No — not connected to operational DB |
| "Send a Teams message when this Power BI KPI goes red" | No | Yes — native Teams action |

#### Integration Pattern: Drasi → Fabric Eventstream → Activator

Drasi and Activator chain together when you need database-level change detection feeding into Fabric-native condition evaluation:

```
PostgreSQL row change on EXECUTION_LEDGER_jobs_runs
  → Drasi WAL reader detects the change
  → Drasi pushes webhook event {jobId, status, completedAt, ...}
  → Fabric Eventstream receives the webhook as a custom endpoint
  → Eventstream routes to Activator
  → Activator evaluates: "job failure count > 5 in the last hour?"
  → If yes: trigger Fabric pipeline for investigation, send Teams notification
```

This pattern gives you Drasi's exactly-once database change capture combined with Activator's time-windowed condition evaluation and Fabric-native actions.

#### Integration Pattern: Activator → Execution Ledger → Drasi (Full Loop)

The reverse direction: Activator detects a Fabric condition, triggers EL remediation, and Drasi captures the remediation outcome.

```
Fabric Activator: "Warehouse refresh latency > 30 minutes for 3 consecutive windows"
  → Activator runs a Fabric pipeline
  → Fabric pipeline calls Execution Ledger API (REST connector, managed identity)
  → EL triggers recovery job: check warehouse health → pause downstream → notify on-call
  → EL job state changes in PostgreSQL
  → Drasi detects the state change in WAL
  → Drasi pushes webhook: "recovery job abc-789 completed"
  → Downstream system (or another Activator rule) consumes the webhook
```

#### Not a Competition

Drasi and Fabric Activator are not competing alternatives. Drasi is a database change-streaming layer. Activator is a data-condition evaluation and action engine. In a platform that uses both Execution Ledger and Microsoft Fabric, you would typically use both — Drasi for operational DB change detection, Activator for Fabric data monitoring — with Eventstreams as the bridge between them.

### Drasi Expansion: What More Drasi Unlocks

The current Drasi configuration in Execution Ledger already provides a secondary reactive enqueue path alongside the scheduler's direct enqueue. Expanding it further would unlock a new class of reactive, event-driven scenarios — moving Execution Ledger from "durable job orchestrator" to "reactive platform backbone."

#### Current Drasi Footprint

Drasi currently runs two Cypher continuous queries against two PostgreSQL tables, with two HTTP reactions:

| Query | Watches | Cypher Condition | Reaction | What It Does |
|---|---|---|---|---|
| `completed-job-runs` | `EXECUTION_LEDGER_jobs_runs` | `r.status = 'completed' OR r.status = 'failed'` | HTTP webhook | Pushes job outcomes to configured webhook |
| `due-schedules` | `EXECUTION_LEDGER_schedules` | `s.next_run_at <= datetime() AND s.enabled = true` | HTTP to API internal endpoint | Enqueues intents reactively when schedules are due — no polling |

The `due-schedules` query provides a secondary reactive enqueue path. The scheduler directly enqueues intents during its tick; Drasi detects the WAL change from the scheduler's `next_run` update and can also enqueue reactively via the API's internal endpoint. This dual path means scheduled jobs are never missed — the scheduler catches them on tick, and Drasi catches them on WAL change.

#### What Drasi Expansion Would Unlock

Each expansion is additive. You can adopt them incrementally.

##### Phase 1: More Tables, More Queries

Add Cypher queries against additional tables to detect operational conditions reactively:

| New Query | Watches Table | Condition | Triggered Action |
|---|---|---|---|
| **Step-level failure burst** | `EXECUTION_LEDGER_steps_runs` | Step status = `failed` AND count in window > threshold | Trigger operator alert job |
| **Stale run detection** | `EXECUTION_LEDGER_jobs_runs` | `status = 'running' AND started_at < now() - interval '30 minutes'` | Trigger stale lease recovery job |
| **Tenant state change** | `EXECUTION_LEDGER_tenants` | `active` column changed | Trigger provisioning or deprovisioning job |
| **Connector config drift** | `EXECUTION_LEDGER_connector_config` | Config hash changed | Trigger config validation and preflight check |
| **Schedule health** | `EXECUTION_LEDGER_schedules` | Schedule has not fired in 2x its cron interval | Trigger schedule recovery job |
| **Queue backlog threshold** | Local intent queue table | Queue depth > threshold for > 5 minutes | Trigger worker scale-out or alert |

**New scenario:** "A connector config changes. Drasi detects it. Drasi pushes to a reaction. The reaction triggers an Execution Ledger preflight job that validates the new config before any production job uses it. If validation fails, the job compensates by reverting the config."

##### Phase 2: More Reaction Types

Drasi natively supports multiple reaction types beyond HTTP. Adding them would enable richer integration patterns:

| Reaction Type | What It Enables | Concrete Scenario |
|---|---|---|
| **Event Grid** (`drasi-reaction-eventgrid`) | Fan-out to multiple subscribers; native Azure integration | Completed job event → Event Grid → Service Bus topic → multiple downstream subscribers (audit, billing, monitoring) |
| **Service Bus** (`drasi-reaction-servicebus`) | Queue events for guaranteed processing; topic for fan-out | Failed job event → Service Bus queue → dedicated failure-analysis worker |
| **SignalR** (`drasi-reaction-signalr`) | Real-time push to connected clients | Job status changes → push to Tauri operator UI in real time, no polling |
| **Debug/Log** (`drasi-reaction-debug`) | Structured logging for audit | Every Drasi-detected change → structured log with correlation ID for compliance archive |

**New scenario:** "A job fails. Drasi detects it. Drasi pushes to Event Grid. Event Grid routes to three subscribers: (1) PagerDuty for on-call alerting, (2) a Service Bus queue for automated retry analysis, (3) a Fabric Eventstream for Activator condition evaluation. One database change fans out to three systems."

##### Phase 3: Multi-Source Drasi

Drasi supports PostgreSQL, SQL Server, and Cosmos DB sources. Currently Execution Ledger only uses PostgreSQL (its own database). Adding external sources would let EL react to changes in other operational databases:

| External Source | What EL Would Detect | Triggered Action |
|---|---|---|
| **SQL Server (ERP)** | New purchase order created | Trigger order fulfillment job |
| **SQL Server (CRM)** | Customer status changed to "churned" | Trigger offboarding job (revoke access, archive data, notify) |
| **Cosmos DB (inventory)** | Stock level below reorder point | Trigger procurement job |
| **PostgreSQL (another service)** | User provisioning request inserted | Trigger identity lifecycle job |

**New scenario:** "A customer status changes in the CRM's SQL Server database. Drasi detects the change via a SQL Server source. Drasi pushes to an EL webhook reaction. EL triggers an offboarding job: revoke Entra ID access, archive blob data, notify account manager, update billing system. All without polling the CRM."

##### Phase 4: Bidirectional Drasi (Feedback Loop)

Drasi detects a change, triggers an EL job, the EL job writes results back to the database, and Drasi detects that result — creating a feedback loop for autonomous operations:

```
External DB change → Drasi detects → EL job triggered → EL writes result to PG
  → Drasi detects PG result → EL compensation job triggered if result is error
  → Drasi detects compensation result → notification sent
```

**New scenario — Autonomous VM scaling:** "CPU metrics are written to an operational PostgreSQL table by an external monitoring agent. Drasi detects a row where CPU > 90%. EL triggers a scale-out job (azure_compute connector). EL writes the result: 'scaled from 2 to 5 instances'. Drasi detects the result. If the scale-out succeeded, Drasi triggers a verification job (wait 5 minutes, check CPU again). If the scale-out failed, Drasi triggers an alert job. The entire loop is reactive, not scheduled."

##### Phase 5: Drasi as a Connector Type

Make Drasi a first-class connector that steps can use, not just a background service:

| Hypothetical Connector Step | What It Does | Use In a Pipeline |
|---|---|---|
| `drasi.wait_for_change` | Pauses the job until a specific database change occurs (table + condition) | "Execute steps 1-3, then wait for the ERP to confirm the order was processed before continuing to step 4" |
| `drasi.query_condition` | Runs a continuous query, proceeds when the condition is met (with timeout) | "Wait up to 10 minutes for the gold table row count to exceed 1000, then proceed; if timeout, compensate" |

**New scenario — Cross-system transaction:** "Step 1: Create purchase order in ERP (REST). Step 2: `drasi.wait_for_change` on ERP's SQL Server — wait for the PO to be approved (status column changes). Timeout: 30 minutes. Step 3: If approved, trigger payment (REST). If timeout, cancel the PO (compensate step 1). Drasi bridges the asynchronous gap between systems without polling."

#### Expansion Roadmap Summary

| Phase | What changes | New scenarios unlocked | Effort |
|---|---|---|---|
| **Current** | 2 queries, 2 reactions, 1 source (PG) | Reactive scheduling, job outcome webhooks | Done |
| **Phase 1** | More Cypher queries on existing Postgres tables | Stale run detection, tenant lifecycle, config drift, step-level alerting | Low — config changes only |
| **Phase 2** | Additional reaction types (Event Grid, Service Bus, SignalR) | Fan-out, real-time UI, audit logging | Medium — new Drasi reaction crate dependencies |
| **Phase 3** | External Drasi sources (SQL Server, Cosmos DB) | Cross-system change detection, ERP/CRM reactivity | Medium-High — new source crate deps, network config |
| **Phase 4** | Bidirectional feedback loops | Autonomous remediation, closed-loop scaling | High — state machine logic, cycle detection |
| **Phase 5** | Drasi as a connector type | Cross-system async transactions, event-driven pipelines | High — new connector trait, job suspension/resumption |

The key insight: Drasi is already providing a secondary reactive enqueue path and operational awareness queries beyond the scheduler's direct enqueue. Each expansion phase is incremental and additive. You can stop at any phase based on the scenarios you need.
