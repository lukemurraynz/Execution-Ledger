# Drasi Integration for D365 DMF Export Polling

Execution Ledger already runs a Drasi WAL reader (`execution-ledger-drasi`) that streams changes from `EXECUTION_LEDGER_jobs_runs` and pushes completed/failed run events to a webhook, plus a secondary reactive path for due schedules (see the main README's "Scenario-based usage" and "Drasi (WAL reader)" sections). This pack is the concrete case that capability was built for: change-driven orchestration instead of a blocking poll loop.

## The problem this avoids

A DMF export can take anywhere from seconds to most of an hour to finish, and F&O only exposes its progress through a status-check call you poll yourself (`GetExecutionSummaryStatus`) — there's no native push notification. Execution Ledger's worker has no in-step blocking primitive for that kind of wait, by design: a step that blocks for up to an hour would hold one of a worker replica's bounded concurrent-intent slots (default 5) for the entire wait, starving other work.

`d365-dmf.export-poll` (see `jobs/export-poll.json`) is built to run instead as a **short, scheduled job** — one status check per launch, gated by an `output_schema` that only accepts `Succeeded`/`PartiallySucceeded` (see the job's `_comment` for why `NotRun` failing the step is intentional, not a bug).

## What "improve this with Drasi" means concretely

Without Drasi, the only way to chain "poll succeeded" → "run the transfer job" is a **fixed-delay guess**: something outside Execution Ledger watches the scheduler tick outcomes and waits an arbitrary extra interval before trying `export-transfer`, or an operator triggers it manually. That's exactly the kind of timer-polling gap the platform's own README calls out as the thing Drasi is for ("Replace timer polling with change-driven orchestration by streaming WAL changes from PostgreSQL through Drasi").

With Drasi, `export-poll` completing successfully **is itself the WAL event** — no new control table, no new connector, no new polling loop. The pattern:

```
┌────────────────────────────────────────────────────────┐
│  execution-ledger-scheduler                             │
│  → cron tick every 2 min → launches d365-dmf.export-poll │
└──────────────────────┬───────────────────────────────────┘
                       │ one status check per launch
                       ▼
┌────────────────────────────────────────────────────────┐
│  d365-dmf.export-poll run                                │
│  → output_schema gates on Succeeded/PartiallySucceeded    │
│  → NotRun/Failed: this run fails closed, next tick retries│
│  → Succeeded: this run completes                          │
└──────────────────────┬───────────────────────────────────┘
                       │ run status flips to 'completed' in
                       │ EXECUTION_LEDGER_jobs_runs (WAL)
                       ▼
┌────────────────────────────────────────────────────────┐
│  execution-ledger-drasi (WAL reader, already running)    │
│  → continuous query: completed runs where job_id =        │
│    'd365-dmf.export-poll'                                 │
└──────────────────────┬───────────────────────────────────┘
                       │
                       ▼
┌────────────────────────────────────────────────────────┐
│  Reaction: POST /platform/jobs/trigger                    │
│  → jobId: d365-dmf.export-transfer                         │
│  → executionId carried through from the completed run's    │
│    stored output                                           │
└────────────────────────────────────────────────────────┘
```

This gets `export-transfer` started within one WAL-notification latency of the export actually finishing, instead of waiting out a fixed polling interval on top of the poll interval — the same "<1 minute reaction time... without any polling loops" property the `azure-compute-scale-pack/docs/drasi-sources.md` pattern documents for metric-reactive scaling.

## Drasi continuous query sketch

```yaml
apiVersion: v1
kind: Reaction
metadata:
  name: d365-dmf-export-completed
spec:
  trigger:
    kind: Continuous
    query: |
      SELECT r.id as runId, r.job_id as jobId
      FROM EXECUTION_LEDGER_jobs_runs r
      WHERE r.job_id = 'd365-dmf.export-poll'
        AND r.status = 'completed'
        AND r.updated_at > DATEADD(minute, -5, GetCurrentTimestamp())
  actions:
    - kind: HttpPost
      endpoint: https://<apim-gateway>/execution-ledger/platform/jobs/trigger
      headers:
        Authorization: Bearer <token>
      body:
        jobId: d365-dmf.export-transfer
        tenantId: <tenant-id>
```

Adjust the query to your actual `EXECUTION_LEDGER_jobs_runs` schema and Drasi source configuration — this sketch follows the same shape as the existing `azure-compute-scale-pack`'s Postgres-source reaction, substituting the jobs-runs table Drasi already reads for that pack's ad hoc control table.

## Why not a dedicated control table (the azure-compute-scale-pack pattern)?

`azure-compute-scale-pack` uses a small ops-owned Postgres table that Drasi polls directly, because that pack needs an operator to write an override value (`scale_override.mode`) with **no existing table already carrying that signal**. This pack doesn't need one: `EXECUTION_LEDGER_jobs_runs` already carries exactly the signal we need (did the poll job succeed), and there is currently no connector that lets a job step write to an arbitrary Postgres control table anyway (`kv_set`/`kv_get` exist only as CLI-internal helpers today, not as a connector or API route) — so reusing the run-status table Drasi already watches is both simpler and the only option available without adding new platform surface.

If a future need arises to distinguish `NotRun` (still running) from `Failed` (genuinely broken) at the Drasi layer — today both fail `export-poll`'s `output_schema` the same way — that would be the point where a dedicated status column or control table becomes worth the added surface, not before.

## A more direct option, if this environment has it: Drasi's native Dataverse source

Drasi ships a first-party [Dataverse source](https://drasi.io/drasi-kubernetes/how-to-guides/configure-sources/configure-dataverse-source/) that watches Dataverse tables via Dataverse's own change-tracking (CDC) mechanism — no webhook, no polling, delta tokens survive restarts. If it applies here, it's strictly better than the scheduled-poll pattern above: no `export-poll` job at all, just a continuous query reacting directly to the row change.

**It does not apply as-is, and this matters:** DMF is a Dynamics 365 **Finance & Operations** capability, backed by F&O's own OData endpoint (`https://<org>.operations.dynamics.com/data/...` — the exact `PartURL` values this pack calls). The Drasi Dataverse source connects to the **Dataverse Web API** (`https://<org>.api.crm.dynamics.com`) — the backend for Dynamics 365 Customer Engagement / Power Platform apps. These are two different data platforms that both ship under the "Dynamics 365" name; F&O entities are not Dataverse tables by default.

**When it *would* apply:** if this F&O environment has [Dataverse virtual tables for Finance and Operations](https://learn.microsoft.com/dynamics365/fin-ops-core/dev-itpro/data-entities/virtual-entities-dataverse) configured — a real, separately-provisioned Microsoft integration that surfaces F&O entities into Dataverse — and the DMF execution/status data (or a proxy for it) is exposed through that bridge, then point Drasi's Dataverse source at the relevant virtual table instead of running `export-poll` on a schedule. Confirm with whoever owns the D365 environment whether that bridge exists before assuming it does; don't wire this up speculatively against an endpoint that isn't there.
