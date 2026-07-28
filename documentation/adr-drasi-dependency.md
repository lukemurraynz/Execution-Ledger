# ADR: Drasi for PostgreSQL WAL Change Streaming

## Status

Accepted

## Context

Execution Ledger needs to react to PostgreSQL data changes in real time rather than relying on timer-based polling. The requirement is: when a tracked row changes in the database, the worker should detect the change and enqueue an intent for processing.

Polling introduces latency, wastes compute on unchanged data, and creates scaling pressure at high table volumes. WAL (Write-Ahead Log) streaming provides change detection at the source without query overhead.

Drasi is an open-source reactive engine that provides:
- `drasi-source-postgres`: tails PostgreSQL logical replication slots and emits change events
- `drasi-reaction-http`: delivers change events to HTTP endpoints (the worker)

The dependency is pinned to version `0.2.7` via a normal `[workspace.dependencies]` entry resolved from crates.io.

> **2026-07-28 update:** This crate was vendored at `vendor/drasi-source-postgres/` (with a `[patch.crates-io]` override) from initial adoption until this date, on the theory that it might need local patches ahead of upstream releases. It never did. The vendored copy was byte-for-byte identical to the published crate the entire time, confirmed by diffing the vendored `Cargo.toml` against `Cargo.toml.orig` (only Cargo's own registry-normalization differences, no functional change). The vendoring was removed and replaced with a plain crates.io dependency; see "Consequences" below for the reasoning.

## Decision

Use Drasi as the change-streaming layer for PostgreSQL WAL events, taken as a plain crates.io dependency (`drasi-source-postgres = "0.2.7"`) rather than a vendored fork.

The `execution-ledger-drasi` service embeds `drasi-source-postgres` directly and processes change events via `drasi-reaction-http`, pushing completed/failed job-run events to a configured webhook. It runs as a singleton Container App, independent of the worker replicas.

## Consequences

### Positive

- Real-time change detection without polling overhead
- Change events arrive at the worker with minimal latency
- Integrates cleanly with the existing Tokio-based worker architecture
- WAL-based detection is deterministic: every committed change is captured exactly once

### Negative

- Replication slot conflicts occur in multi-revision worker scenarios (two replicas competing for the same slot)
- Drasi project maturity is uncertain: long-term maintenance and community support are unverified
- Adds a runtime dependency that is not part of the standard Azure managed-service surface
- Pinned to an exact version (`0.2.7`) rather than tracking a semver range, so upstream fixes and improvements require a deliberate version bump

### Mitigations

- **Replication slot conflicts:** The Drasi service runs as a singleton Container App (`minReplicas: 1, maxReplicas: 1`). PostgreSQL replication slots are inherently single-consumer, so the architecture constrains Drasi to one replica by design. This was split from the worker in the scaling refactor: previously one worker replica held the slot while others skipped Drasi startup. Now the Drasi reader runs independently, and worker replicas are pure intent processors.
- **Dependency maintenance:** Track Drasi release notes and bump the pinned version deliberately; Dependabot/Renovate already covers routine crates.io version bumps (`.github/dependabot.yml`), so this no longer requires a manual vendor-sync step.
- **Fallback plan:** If Drasi is abandoned or becomes unmaintained, the fallback path is direct WAL tailing via `pg_logical` or a custom WAL reader using `pq-sys`. This would require replacing the `drasi-source-postgres` crate with a purpose-built WAL consumer but would not change the worker's intent-processing architecture.
- **If a local patch becomes necessary:** Re-introduce vendoring at that point, via `cargo vendor` plus a `[patch.crates-io]` path override, scoped to the specific patch needed, with the patch itself documented in `vendor/README.md` (reason, date, diff against the upstream tag). Don't vendor speculatively; vendor when there's an actual diff to carry.

## Fallback architecture

If Drasi is replaced:

1. Implement a WAL tailing module using `pq-sys` or `tokio-postgres` logical replication commands
2. Parse WAL change events into the same `ChangeNotification` shape the worker currently expects
3. Keep the worker's intent-enqueue and step-execution logic unchanged
4. Remove the `drasi-source-postgres`, `drasi-reaction-http`, and `drasi-reaction-log` dependencies from `Cargo.toml`

The worker architecture is designed so the change-detection layer is swappable. The intent-processing, connector execution, and recovery logic do not depend on Drasi internals.

## Operational setup

Azure PostgreSQL Flexible Server requires explicit configuration for logical replication. `execution_ledger_core::db::assert_logical_replication_enabled` checks `SHOW wal_level` on startup and fails fast if not configured.

### Server parameters

```bash
az postgres flexible-server parameter set \
  --resource-group <rg> --server-name <server> \
  --name wal_level --value logical

az postgres flexible-server restart \
  --resource-group <rg> --server-name <server>

az postgres flexible-server parameter set \
  --resource-group <rg> --server-name <server> \
  --name max_replication_slots --value 10

az postgres flexible-server parameter set \
  --resource-group <rg> --server-name <server> \
  --name max_wal_senders --value 10
```

### Database setup

Run once per environment as a role with `REPLICATION` privilege:

```sql
ALTER ROLE EXECUTION_LEDGER_worker WITH REPLICATION;
ALTER TABLE EXECUTION_LEDGER_jobs_runs REPLICA IDENTITY FULL;
```

`REPLICA IDENTITY FULL` ensures `UPDATE`/`DELETE` change events carry the full old row. `execution-ledger-drasi` creates the publication itself on startup (`ensure_publication_exists`).

### Networking

Replication runs over port 5432 — no additional ports. However, long-lived replication connections are sensitive to idle connection drops. If the server sits behind Azure Firewall or an NVA with TCP idle timeouts, raise the timeout above the WAL sender's expected idle period or enable TCP keepalives.

### Compute sizing

The Rust Drasi service has a smaller memory footprint than the Node equivalent it replaced. Container Apps CPU/memory can start at the platform minimum (0.25 vCPU / 0.5 Gi) and scale up only if Drasi's in-memory continuous-query state demands it.

### Checklist

- [ ] `wal_level=logical` set and server restarted
- [ ] `max_replication_slots` and `max_wal_senders` raised
- [ ] `EXECUTION_LEDGER_worker` role granted `REPLICATION`
- [ ] `EXECUTION_LEDGER_jobs_runs` set to `REPLICA IDENTITY FULL`
- [ ] Firewall/VNet rule confirmed for the Drasi service's subnet
- [ ] Idle timeout on network path raised above WAL sender idle period
- [ ] Container Apps CPU/memory reassessed for Rust binary; Drasi capped at single replica

## References

- `execution-ledger/README.md` — Runtime behavior note (Drasi WAL reader)
- `execution-ledger/Cargo.toml` — `[workspace.dependencies]` entry pinning `drasi-source-postgres` to `0.2.7`
