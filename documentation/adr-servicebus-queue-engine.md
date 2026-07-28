# ADR: Azure Service Bus with KEDA Autoscaling as the Production Queue Engine

## Status

Accepted

## Context

The worker needs to dequeue intents (job-step execution requests) produced by the API and scheduler, and it needs to scale horizontally as intent volume grows. Two things have to be true of whatever queueing mechanism is chosen:

- Multiple worker replicas must be able to compete for intents without double-processing one, without a coordination service beyond what's already in the architecture.
- The system must be usable with zero Azure dependencies for local development and CI (see "Try it locally (no Azure account needed)" in the README). A cloud-only queue would break that.

Execution Ledger implements both a local, in-database queue (Postgres `FOR UPDATE SKIP LOCKED` against an intents table) and a `servicebus`-backed engine, selected at runtime by `EXECUTION_LEDGER_QUEUE_ENGINE` (`local` default, `servicebus` for Azure deployments). Both share the same worker-side intent-processing logic (`execution-ledger-worker::engines`); only how an intent is received differs.

The scaling question is specifically about the production path: given Service Bus is already the chosen managed queue, how should worker replica count track queue depth, and what Service Bus SKU/configuration does that require.

## Decision

Use Azure Service Bus **Standard** SKU with **partitioning enabled** (`infra/modules/messaging.bicep`) as the production queue engine, and scale worker replicas via **KEDA's native `azure-servicebus` scaler** (configured in `infra/modules/app.bicep`'s `workerScaleSettings`), targeting a fixed message count per replica (`workerScaleQueueMessageCount`) rather than a custom polling-based autoscaler.

The queue (`execution-ledger-intents`) is configured with `maxDeliveryCount: 10`, a `PT1M` lock duration, a 14-day message TTL, and dead-lettering on expiration. KEDA's scale rule authenticates via system-assigned managed identity (`identity: 'system'`), not a Service Bus connection string, consistent with the platform's managed-identity-first posture. When `EXECUTION_LEDGER_QUEUE_ENGINE=local`, `workerScaleSettings` falls back to a fixed `1–2` replica range with no KEDA rule, since the local queue is a devcontainer/CI construct, not a scaling target.

## Consequences

### Positive

- KEDA's `azure-servicebus` scaler is a maintained, first-party scale trigger. There's no custom queue-depth polling code to write or operate.
- Partitioned Standard SKU keeps costs low relative to Premium while still providing horizontal broker distribution; Premium remains an explicit upgrade path (see the scaling model's "500+ jobs/sec" tier) rather than a default.
- Managed-identity authentication for the scale rule means no Service Bus connection string exists anywhere in the deployment, consistent with every other Azure service integration in this platform.
- Dead-lettering on expiration, combined with the existing `recovery-servicebus-poison` API/CLI surface, gives an operational recovery path for messages that exceed delivery attempts, without needing a custom retry-tracking table.
- The `local`/`servicebus` engine split means the *entire* worker intent-processing path (connector execution, idempotency, compensation) is exercised identically in a devcontainer with zero Azure dependency, and only the queue transport swaps in production.

### Negative

- Standard SKU has lower throughput ceilings and fewer availability guarantees than Premium. The documented ceiling (~200 intents/sec at 10 replicas x ~20/sec) is a Standard-tier estimate, not a hard platform limit, and heavy workloads will need the Premium upgrade path.
- KEDA's `messageCount`-per-replica target is a coarse scaling signal. It reacts to queue depth, not to actual per-message processing cost, so a workload with highly variable per-intent execution time (some connectors much slower than others) may over- or under-provision replicas relative to real load.
- Partitioning trades strict FIFO ordering for throughput and availability. Service Bus partitioned queues do not guarantee cross-partition message order. Job templates that assume strict intent ordering across concurrent runs need to account for this (steps *within* one intent still execute sequentially, unaffected by partitioning).
- No dead-letter depth alerting ships by default. The Bicep module has an explicit inline comment flagging this as an operational gap the deployer must close (`az monitor` alert on the `DeadletteredMessages` metric).

### Mitigations

- **Throughput ceiling:** Documented explicitly in the scaling model's tiered guidance, with the concrete next step (upgrade to Premium, raise `workerScaleMaxReplicas`, tune `EXECUTION_LEDGER_WORKER_MAX_CONCURRENT_INTENTS`) rather than left implicit.
- **Ordering:** Not mitigated at the queue layer by design. Job templates that need strict ordering should express that as sequential steps within a single job (see the "no fan-out" constraint on `ctx.upstream_output`), not rely on cross-run message order.
- **Missing dead-letter alerting:** Flagged inline in the Bicep source itself as a required follow-up per deployment, alongside the existing manual recovery path (`/platform/recovery/servicebus-poison?limit=N`).

## Alternatives considered

- **Azure Storage Queues.** Cheaper, but no native partitioning/session model and a comparatively limited KEDA scaler feature set next to Service Bus's. Rejected because Service Bus's dead-lettering, delivery-count tracking, and lock-duration semantics map directly onto the recovery model (`recovery-servicebus-poison`, lease-based reprocessing) this platform already relies on.
- **A custom polling-based autoscaler** (a sidecar or scheduled function querying queue depth and calling the Container Apps API to set replica count). Rejected in favor of KEDA's built-in scaler: it's already the scaling primitive Container Apps ships with, avoids a second component to operate, and needs no custom polling loop or additional identity/permission wiring beyond the one KEDA already uses.
- **Service Bus Premium by default.** Rejected as the default SKU on cost grounds. Premium is called out explicitly as the scale-up path once the Standard-tier ceiling is reached, not the starting point for a platform meant to be cheap to run at low volume (see the README's "Cost estimate" section).

## References

- `execution-ledger/README.md`: "Scaling model", "How each service scales", "Worker scaling mechanics"
- `infra/modules/messaging.bicep`: Service Bus namespace, partitioned queue configuration
- `infra/modules/app.bicep`: `workerScaleSettings`, KEDA `azure-servicebus` scale rule
- `execution-ledger/crates/execution-ledger-worker/src/engines/`: `local` and `servicebus` engine implementations
- `execution-ledger/crates/execution-ledger-core/src/queue.rs`: `QueueEngine` selection
