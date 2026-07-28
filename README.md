# Execution Ledger

Durable job orchestration for Azure. Step-level idempotency, replay, skip, and compensate controls. Tenant-scoped API with Entra ID auth. Connector packs for REST, queue, blob, SFTP, and ERP integrations. Built for operators who need to recover from failures without manual database surgery.

## Quickstart

```bash
azd provision && azd deploy
cargo run -p execution-ledger-cli -- preflight
cargo run -p execution-ledger-cli -- job-launch <tenant-id> hello-world.health-and-echo '{"steps":[{"id":"health","connector":"platform_health"}]}'
```

## What it does

Execution Ledger is a Rust multi-service backend that orchestrates jobs across external systems. You define jobs with connector steps (REST calls, queue operations, blob transfers, SFTP uploads, ERP integrations). The platform handles durability, retries, idempotency, and recovery.

| Capability | What it means |
|---|---|
| Step-level replay | Re-execute a single failed step without touching the rest of the job |
| Coordinated compensation | Undo committed steps in reverse order when a later step fails |
| Schema validation | Validate step input and output against JSON Schema before and after execution |
| Tenant isolation | One tenant cannot see or modify another tenant's data |
| Managed identity-first | No connection strings or access keys in production |
| Fail-closed connectors | Missing config fails the step, never silently degrades |
| Operational recovery APIs | Clear stale leases, drain poison queues, recover runs via API |
| Cron/timezone scheduling | Repeat jobs on cron expressions with timezone support |
| WAL-based change reactions | React to database state changes via Drasi, no polling |
| Per-replica concurrency | Workers process multiple intents in parallel, gated by configurable semaphore |

## Architecture

```
          ┌────────────────────┐
          │  Tauri Operator    │
          │  (Desktop Console) │
          └─────────┬──────────┘
                    │ (Rust CORS-free commands)
                    │
                ┌───▼─────────────────────┐
                │  Azure API Management   │
                └───┬─────────────────────┘
                    │
                    ▼
┌────────────────────────────────────────────────────────────────┐
│                  Container Apps Environment                      │
│                                                                 │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────────┐   │
│  │   API    │  │  Worker  │  │  Drasi   │  │  Scheduler   │   │
│  │(1-3 reps)│  │(KEDA 1-10│  │(WAL rdr, │  │ (singleton)  │   │
│  │          │  │  reps)   │  │ sgltn)   │  │              │   │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘  └──────┬───────┘   │
│       │             │             │               │            │
└───────┼─────────────┼─────────────┼───────────────┼────────────┘
        │             │             │               │
        │  enqueue    │  dequeue    │  WAL stream   │  enqueue
        ▼             ▲             │               │  (cron)
 ┌──────────────┐     │             │               │
 │  Service Bus │─────┘             │               │
 │  (Standard,  │                   │               │
 │  partitioned)│                   │               │
 │  lock: PT1M  │                   │               │
 │  poll: 5s    │                   │               │
 └──────────────┘                   │               │
                                    ▼               ▼
                             ┌──────────────────────────┐
                             │  PostgreSQL Flexible      │
                             │  Server                   │
                             └──────────┬───────────────┘
                                        │
                                        ▼
                             ┌──────────────────────────┐
                             │  Webhook (Drasi output)   │
                             │  job completed / failed   │
                             └──────────────────────────┘
```

**Component roles:**

- **Tauri Operator**: Desktop console for operators. Manages runs, schedules, and recovery actions. Communicates with API via Rust-backed Tauri commands (no browser CORS dependency). Supports schedule UI, run list/replay, skip/compensate, and recovery workflows.
- **API**: Accepts job triggers, exposes recovery endpoints. Enqueues intents to Service Bus.
- **Worker**: Dequeues intents from Service Bus (KEDA-scaled 1–10 replicas). Executes connector steps with bounded concurrency. Polling tuned to 5s for sub-10s latency.
- **Scheduler**: Singleton service. Claims due schedules, enqueues intents to Service Bus. Supports cron/timezone expressions.
- **Drasi**: WAL reader, singleton. Streams PostgreSQL changes and pushes webhook events for completed/failed jobs. Secondary reactive enqueue path.
- **Service Bus**: Message-driven intent queue. Partitioned, Standard tier. Message lock duration: PT1M (tuned for 1-minute job steps). Configured for KEDA scale triggers (queue depth threshold 30, max 5 replicas).
- **PostgreSQL**: Durable state store. Runs table with status tracking, steps table with step history and compensation state. Drasi reads logical replication slot.

**Data flow:**

1. Operator triggers job via Tauri UI → API enqueues intent to Service Bus
2. Worker dequeues intent → executes connector steps → writes completion to runs/steps table
3. Drasi reads WAL change → pushes webhook event
4. Scheduler claims due schedule → enqueues intent to Service Bus
5. On failure: Operator uses Tauri console to replay, skip, or compensate individual steps

## Repository layout

```
├── execution-ledger/         Rust workspace (crates, migrations, CLI)
│   ├── crates/
│   │   ├── execution-ledger-api/
│   │   ├── execution-ledger-worker/
│   │   ├── execution-ledger-drasi/
│   │   ├── execution-ledger-scheduler/
│   │   ├── execution-ledger-cli/
│   │   ├── execution-ledger-core/
│   │   ├── execution-ledger-domain/
│   │   └── execution-ledger-types/
│   └── README.md             Full documentation
├── infra/                    Azure Bicep infrastructure
├── azure.yaml                Azure Developer CLI project definition
├── documentation/            ADR, scenarios, gates, roadmap
├── execution-ledger/connector-packs/  Optional integration add-ons
```

## Getting started

Prerequisites: Rust 1.97+, Azure subscription, `azd` CLI.

```bash
# 1. Clone and deploy infrastructure
azd provision
azd deploy

# 2. Run preflight checks
cargo run -p execution-ledger-cli -- preflight

# 3. Launch a test job
cargo run -p execution-ledger-cli -- job-launch <tenant-id> hello-world.health-and-echo \
  '{"steps":[{"id":"health","connector":"platform_health"}]}'

# 4. Check status
cargo run -p execution-ledger-cli -- db-status
```

Run locally (no Azure):

```bash
# Set local config
$env:EXECUTION_LEDGER_DATABASE_URL = "postgres://localhost/execution_ledger"
$env:EXECUTION_LEDGER_ENV = "local"
$env:EXECUTION_LEDGER_QUEUE_ENGINE = "local"

# Start services
cargo run -p execution-ledger-api
cargo run -p execution-ledger-worker
cargo run -p execution-ledger-scheduler
cargo run -p execution-ledger-drasi
```

## Key configuration

Runtime config comes from Azure App Configuration. Bootstrap env vars are minimal:

| Variable | Purpose |
|---|---|
| `EXECUTION_LEDGER_DATABASE_URL` | PostgreSQL connection string |
| `EXECUTION_LEDGER_ENV` | Environment label (`local`, `prod`, etc.) |
| `EXECUTION_LEDGER_APP_CONFIG_ENDPOINT` | App Configuration endpoint (production only) |
| `EXECUTION_LEDGER_ENTRA_AUDIENCE` | Expected `aud` claim on JWT tokens (API only) |

Tunable via App Configuration (no redeploy needed):

| Key | Default | What it does |
|---|---|---|
| `EXECUTION_LEDGER_QUEUE_ENGINE` | `servicebus` | `local` or `servicebus` |
| `EXECUTION_LEDGER_WORKER_MAX_CONCURRENT_INTENTS` | `5` | Concurrent intents per worker replica |
| `EXECUTION_LEDGER_PG_MAX_CONNECTIONS` | `7` | PG pool size per service |

## Scaling

Worker replicas scale via KEDA on Service Bus queue depth (default 1–10 replicas). Each replica runs up to 5 concurrent intents (configurable). At 10 replicas and default concurrency, the system handles ~50 concurrent intent executions.

Drasi is a singleton (WAL replication slots are inherently single-consumer). The scheduler is a singleton but can scale if cron evaluation becomes a bottleneck.

Full scaling details and PG connection math in [`execution-ledger/README.md`](execution-ledger/README.md#scaling-model).

## Connectors

| Connector | What it does |
|---|---|
| `platform_health` | Validates platform connectivity and readiness |
| `rest` | Executes HTTP requests to external APIs |
| `queue` | Sends or receives messages from queue systems |
| `azure_blob` | Reads or writes objects in blob storage |
| `sftp` | Transfers files over SFTP |
| `erp` | Executes ERP integration steps |
| `transform` | Reshapes JSON between steps with dot-notation field mapping and schema validation |
| `azure_compute` | Resizes VMs, VMSS, and App Service Plans via ARM API (save-then-restore compensation) |
| `azure_sql` | Resizes SQL Databases and Elastic Pools via ARM API (save-then-restore compensation) |

Add custom connectors as opt-in packs under `execution-ledger/connector-packs/`. See [`execution-ledger/connector-packs/README.md`](execution-ledger/connector-packs/README.md).

## When to use

Execution Ledger fits when you need durable, auditable job orchestration with built-in recovery controls. Compare with alternatives:

| | Execution Ledger | Azure Durable Functions | Temporal |
|---|---|---|---|
| Step-level replay | Targeted | Rewind | Continue-as-new |
| Tenant isolation | Query-level | Function-level | Namespace-level |
| Self-hosted control plane | Container Apps | Serverless | Kubernetes |
| Operational recovery APIs | Built-in | Manual | Manual |
| Managed identity-first | Yes | Yes | No |
| Learning curve | Medium | Low | High |

Use Execution Ledger for multi-tenant workloads needing step-level replay, tenant isolation, and built-in operational recovery. Use Durable Functions for Azure-native serverless workflows. Use Temporal for complex long-running workflows with a team comfortable with operational overhead.

## Documentation

- [`execution-ledger/README.md`](execution-ledger/README.md): full architecture, scaling model, connector reference, security posture
- [`documentation/execution-ledger-scenarios.md`](documentation/execution-ledger-scenarios.md): where Execution Ledger fits, scenario catalog, adoption patterns
- [`documentation/adr-drasi-dependency.md`](documentation/adr-drasi-dependency.md): architectural decision record for Drasi WAL streaming, including operational setup
- [`documentation/adr-apim-gateway.md`](documentation/adr-apim-gateway.md): architectural decision record for Azure API Management as the external gateway and policy boundary
- [`documentation/adr-tenant-isolation.md`](documentation/adr-tenant-isolation.md): architectural decision record for query-level tenant isolation
- [`documentation/adr-servicebus-queue-engine.md`](documentation/adr-servicebus-queue-engine.md): architectural decision record for Service Bus with KEDA autoscaling as the production queue engine
- [`documentation/operational-proof-harness-acceptance.md`](documentation/operational-proof-harness-acceptance.md): acceptance gates, chaos testing plan, unattended harness usage
- [`documentation/enterprise-integration-patterns.md`](documentation/enterprise-integration-patterns.md): classic integration patterns mapped to Execution Ledger, plus Azure SRE Agent integration
- [`documentation/execution-ledger-gaps-and-add-on-decision-matrix.md`](documentation/execution-ledger-gaps-and-add-on-decision-matrix.md): prioritized capability gaps and add-on roadmap
- [`infra/`](infra/): Bicep infrastructure and deployment parameters
- [`connector-packs/`](connector-packs/): adding custom connector integrations
