# Execution Ledger gaps and add-on decision matrix

## Purpose

This document lists practical capability gaps and add-on candidates so you can decide what to build next based on value, effort, and operational risk.

## Current priority matrix

| Rank | Pain point | Stage | Customer impact | Business impact | Effort | Confidence | Risk of doing nothing | Priority score | Priority class | Owner |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | Missing preflight for scope/config readiness | Environment and auth setup | H | H | M | H | H | 13.5 | Quick win | Platform Engineering |
| 2 | Limited guided recovery decisioning | Incident and recovery | H | H | M | M | H | 12.0 | Near-term | Operations + Product |
| 3 | Connector onboarding readiness friction | First workflow onboarding | M | H | M | M | M | 8.0 | Near-term | Integration Engineering |
| 4 | Degraded-mode visibility (Drasi skipped, queue active) | Scale and steady-state | M | H | M | M | H | 10.0 | Near-term | Runtime Engineering |
| 5 | Connector-pack governance drift risk | Governance and expansion | M | H | H | M | H | 6.0 | Strategic | Architecture + Platform Governance |

## Add-on candidates mapped to gaps

| Gap | Add-on candidate | What it adds | Estimated effort | Runtime support surface |
|---|---|---|---|---|
| Preflight readiness | `platform-preflight` add-on | One command/API call to validate env contract, queue engine config, and scope readiness before launch | M | CLI command + Tauri "Preflight check" action |
| Guided recovery | `ops-guided-recovery` add-on | Symptom-to-action guidance with bounded action choices and confirmations | M | Tauri runtime support panel |
| Connector readiness | `connector-readiness` add-on | Step payload and connector config validation before trigger | M | API pre-check route + Tauri trigger guard |
| Degraded-mode visibility | `runtime-state-signals` add-on | Explicit "queue active / Drasi skipped" state signal and operator alerting hooks | M | `/health` extension + Tauri status card |
| Pack governance | `pack-policy` add-on | Pack manifest lint, ownership metadata, required runbook checks, and CI gate | H | Repo CI + release checklist |

## Tauri runtime support expectations

Tauri should be treated as the operator runtime console, not only a demo shell.

Minimum runtime-support functions:

1. preflight readiness check before trigger operations
2. guided recovery path selection by symptom
3. one-click access to lease and poison recovery actions
4. explicit degraded-mode visibility and action recommendation

## Go/no-go criteria for each add-on

### Build now

- `platform-preflight`
- `ops-guided-recovery`

These reduce onboarding and incident risk immediately.

### Validate first, then build

- `connector-readiness`
- `runtime-state-signals`

Run a short validation cycle against real onboarding and incident data first.

### Strategic investment

- `pack-policy`

Build when two or more customer packs are active and governance overhead becomes visible.

## Decision checklist

Use this checklist per add-on:

1. Does it reduce failed first runs or MTTR in measurable terms?
2. Can it be operated through existing API/CLI/Tauri boundaries?
3. Does it preserve fail-closed behavior and managed identity posture?
4. Is there a clear owner and ongoing maintenance budget?

If the answer is "no" on ownership or measurable impact, defer the add-on.

