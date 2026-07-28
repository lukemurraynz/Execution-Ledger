---
name: execution-ledger
description: >-
  Implementation and extension guide for this repository's Execution Ledger platform:
  adding connectors, wiring workloads, extending change-driven flows, applying Service Bus
  autoscaling, shipping customer-specific connector packs, and preserving the preferred
  Azure runtime path (APIM ingress plus App Configuration and Key Vault) without weakening
  fail-closed, idempotency, or managed-identity standards.
source: internal
last_verified: "2026-07-28"
---

# Execution Ledger skill

Use this skill when work changes Execution Ledger behavior, connector surfaces, orchestration flows, or platform operations in this repository.

## Overview

This skill keeps coding agents aligned to the production contract in this codebase:

1. real connector execution only
2. managed identity first for Azure messaging and service access
3. fail-closed configuration and secret resolution
4. deterministic replay and idempotent step semantics
5. reviewable operational evidence instead of simulated success
6. APIM as the primary external ingress and policy boundary
7. Azure App Configuration plus Key Vault as the runtime configuration source of truth

## Use when

Trigger this skill for requests such as:

- adding or changing connector-step behavior (`platform_health`, `rest`, `erp`, `ms_graph_notify`, `queue`, `azure_blob`, `sftp`, `azure_compute`, `azure_sql`, `transform`, `pgp`, `blob_archive`, `http_download`; verify this list against `crates/execution-ledger-domain/src/connectors/mod.rs`'s own module-overview doc comment before relying on it, since it is the fastest-changing part of this codebase)
- adding new workload scenarios in API, worker, scheduler, or Tauri operator
- extending change-driven orchestration (Drasi and event-driven triggers)
- changing Service Bus queue processing and autoscaling behavior
- changing Azure API Management routing, policies, or gateway-to-runtime integration
- changing Azure App Configuration, Key Vault references, or runtime bootstrap config loading
- introducing customer-specific integration packs that are outside the base runtime
- updating recovery operations, replay flows, or production proof automation

- adding or modifying Drasi sources, continuous queries, or reactions
- adding or modifying APIM policies, named values, or policy fragments

Do not use this skill for unrelated generic Rust refactors.

## Workflow

1. Identify the execution boundary to change.
   - runtime core: `crates/execution-ledger-core`
   - orchestration/business: `crates/execution-ledger-domain`
   - control plane and worker loops: `crates/execution-ledger-api`, `crates/execution-ledger-worker`, `crates/execution-ledger-scheduler`
   - deployment/runtime controls: `infra/`, `execution-ledger/scripts/`
   - operator workflows: `tauri-operator/`
2. Preserve invariants before editing:
   - no simulation flags or simulation-only execution paths
   - no SAS/connection-string auth in non-local Service Bus runtime paths
   - prefer official Azure Rust crates from https://crates.io/users/azure-sdk over custom SDK code; only implement custom protocol paths when required capability is unavailable in the published crates
   - verify Azure crate provenance and lifecycle before adoption (owner, source repo, maintenance track); treat legacy-track crates as non-default
   - add and remove Rust dependencies with `cargo add` / `cargo rm`; do not hand-edit `Cargo.toml` unless required by documented tooling limitation
   - no silent fallback or best-effort success on invalid config
   - no non-idempotent step execution without explicit guardrails
   - APIM remains the preferred externally exposed API gateway and policy boundary
   - non-secret runtime configuration remains in App Configuration; secrets remain in Key Vault
   - Container Apps keeps only bootstrap environment variables required to start configuration loading
3. Implement at the shared boundary instead of patching multiple callers.
4. Add or update focused tests for the changed invariant.
5. Update README and runbooks when behavior or operations change.
6. Validate changed crates and IaC paths with targeted checks.

## Preferred Azure runtime and gateway path

Execution Ledger defaults to this platform path unless the request explicitly requires an alternative:

1. bootstrap with minimal environment variables in Container Apps
2. load non-secret runtime config from App Configuration by managed identity at startup
3. resolve Key Vault references for secrets
4. fail closed when required configuration is missing or invalid
5. expose external API traffic through Azure API Management with explicit policy enforcement

The minimal bootstrap set should remain:

- `EXECUTION_LEDGER_DATABASE_URL`
- `EXECUTION_LEDGER_ENTRA_AUDIENCE`
- `EXECUTION_LEDGER_APP_CONFIG_ENDPOINT`
- `EXECUTION_LEDGER_ENV`

Optional local or override keys may be used only where already supported by source (`EXECUTION_LEDGER_APP_CONFIG_LABEL`, `EXECUTION_LEDGER_APP_CONFIG_JSON`).

Preferred IaC boundaries for this path:

- runtime bootstrap env and app identity wiring: `infra/modules/app.bicep`
- App Configuration and Key Vault provisioning: `infra/modules/configuration.bicep`
- APIM gateway and policy wiring: `infra/modules/apim.bicep`
- APIM policy XML (standalone files): `infra/modules/apim-policies/*.xml`
- composition and cross-module contracts: `infra/main.bicep`

## APIM policy development

APIM policies live as standalone `.xml` files under `infra/modules/apim-policies/`, NOT as inline strings in Bicep. This gives proper XML syntax highlighting, clean diffs, and avoids Bicep string-escaping issues.

### File layout

```
infra/modules/apim-policies/
├── api-policy.xml                  ← full API policy: inbound → backend → outbound → on-error
├── fragment-jwt-auth.xml           ← reusable JWT validation fragment
└── fragment-correlation-id.xml     ← reusable correlation ID fragment
```

### Loading pattern

In `infra/modules/apim.bicep`, policies are loaded at build time via `loadTextContent()`:

```bicep
var apiPolicyXml = loadTextContent('apim-policies/api-policy.xml')
var jwtAuthFragmentXml = loadTextContent('apim-policies/fragment-jwt-auth.xml')
```

The `{{named-value}}` APIM syntax passes through unchanged since `loadTextContent()` treats content as raw text, so no Bicep escaping is needed.

### Adding a new policy

1. Edit or add the relevant `.xml` file under `infra/modules/apim-policies/`
2. If adding a new fragment, add a `loadTextContent()` var and a `Microsoft.ApiManagement/service/policyFragments` child resource in `apim.bicep`
3. If the policy references new configurable values, add named values to the `namedValues` array and corresponding parameters to `apim.bicep`, `main.bicep`, and `main.parameters.json`
4. Policy ordering in `api-policy.xml` is deliberate. Do not reorder without understanding the defense-in-depth layering (correlation, then CORS, then body limit, then JWT, then rate limit, then gateway secret)

### When to use fragments vs. inline policies

- **Fragment**: reusable across multiple APIs or operations (JWT validation, correlation ID injection, rate limiting)
- **Inline in api-policy.xml**: API-specific logic (gateway secret injection, error masking for Execution Ledger's error shape)

## Drasi configuration

Drasi sources, queries, and reactions live as standalone config files under `crates/execution-ledger-drasi/config/`, NOT as hardcoded Rust constants. This lets operators modify change-detection behavior without recompiling the binary.

### File layout

```
crates/execution-ledger-drasi/config/
├── drasi.yaml                           ← sources + queries + reactions (YAML, Drasi-native schema)
└── queries/
    └── completed-job-runs.cypher        ← Cypher continuous query
```

### Loading pattern

In `src/config.rs`, config is loaded at runtime via `DrasiConfig::load_or_default()`:

```rust
let drasi_config = DrasiConfig::load_or_default()?;
```

Behavior:
- Tries `EXECUTION_LEDGER_DRASI_CONFIG_PATH` env var (or defaults to `config/drasi.yaml` relative to binary)
- Falls back silently to embedded defaults (matching the current production config) if the file is missing
- Fails only on invalid YAML syntax; missing files are not an error
- Cypher queries are resolved from `config/queries/` relative to the config file directory

### Adding a new query

1. Create `config/queries/<query-name>.cypher` with the Cypher query
2. Add a `queries:` entry in `config/drasi.yaml` referencing the new `.cypher` file
3. Add a corresponding `reactions:` entry in `config/drasi.yaml`
4. Wire the new query and reaction into `src/main.rs` following the existing builder pattern

### Adding a new reaction

1. Add a `reactions:` entry in `config/drasi.yaml` with `kind`, `id`, `query`, and reaction-specific fields
2. Add the reaction builder in `src/main.rs`: one `drasi.with_reaction(...)` call per reaction
3. For HTTP reactions, use `drasi_reaction_http::HttpReactionBuilder`
4. For new reaction types, add the corresponding crate dependency to `Cargo.toml`

### When to add a query

Add a Drasi continuous query when:
- A connector pack needs to react to database changes (e.g., scaling events, poison queue detection, schedule staleness)
- The existing `completed-job-runs` query doesn't cover the new change pattern
- A new table or condition needs monitoring

### Docker image

Config files are normalized to `/drasi-config/` in the builder stage, then copied into the runtime image:

```dockerfile
# Builder: normalize path regardless of crate directory structure
RUN mkdir -p /drasi-config && cp -r /workspace/crates/execution-ledger-drasi/config/* /drasi-config/

# Runtime: copy from stable path
COPY --chown=10001:10001 --from=builder /drasi-config/ config/
```

If new config subdirectories are added, the `cp -r` wildcard handles them automatically.

## Connector pack pattern

Use connector packs as optional add-ons, not as runtime forks:

- keep base runtime guarantees in core services
- add pack-specific connector logic in isolated modules and wiring points
- require a pack manifest with:
  - connector ids and required config keys
  - identity and RBAC prerequisites
  - replay/idempotency expectations
  - failure and recovery runbook links
- deploy packs as opt-in units per customer environment

## Connector development

### Scaffolding a new connector

Follow the pattern in `execution-ledger/connector-packs/README.md` (Steps 1-5):

1. Add variant to `ConnectorKind` in `crates/execution-ledger-domain/src/connectors/mod.rs`
2. Create `connectors/<name>.rs` with `pub(crate) async fn execute(ctx, step) -> Result<Value, ExecutionLedgerError>`
3. Add `mod <name>;` declaration and match arm in `execute_step`
4. Document config keys, required/optional fields, and fail-closed behavior in module doc comment
5. Add to a connector pack with job template and runbook

### Fail-closed contract

Every connector MUST enforce this at step start:

- If required config is missing → return `InvalidConfiguration` with the field name
- If external call fails → return `DependencyFailed` with the dependency name
- Never return a success response when the operation failed
- Never silently skip work or fall back to an unsafe default
- Never degrade to a weaker auth method (e.g. connection string when managed identity is required)

```rust
// CORRECT: fail closed
let url = step_config_string(step, "url")
    .ok_or(ExecutionLedgerError::InvalidConfiguration {
        name: "connector.rest.url".to_string(),
    })?;

// WRONG: silent fallback
let url = step_config_string(step, "url")
    .unwrap_or_else(|| "https://default.example.com".to_string());
```

### Idempotency

Every connector step is governed by an idempotency key derived from `run_id + step_id`. Connectors must:

- Use the idempotency key when calling external systems that support deduplication
- Return the same output for the same input when replayed
- Not perform non-repeatable side effects (send duplicate emails, create duplicate records) without idempotency guards
- Document any external idempotency requirements in the module doc comment

### Testing pattern

Each connector should have focused unit tests:

- Happy path: valid config produces expected output shape
- Missing config: returns `InvalidConfiguration` with correct field name
- Edge cases: empty input, null values, malformed paths (for transform connector)
- No test infrastructure or framework beyond `#[cfg(test)]` and `#[test]`

### Anti-patterns

Do NOT:

- add a connector that returns `json!({"accepted": true})` without actually executing the operation
- use environment variables for secrets at runtime (resolve from Key Vault via App Configuration)
- add a connector that requires manual setup steps not captured in the pack runbook
- create connector-specific config that bypasses the App Configuration + Key Vault bootstrap path
- add timeout-less external calls (REST caps at 120s, SFTP at 60s, set a reasonable cap for new connectors)

### The composability contract, now enforced in code: check here first

`ConnectorExecutionContext` carries `upstream_output: Option<&Value>` (the previous step's output), but **whether a connector actually reads it is a per-connector decision.** This used to be discoverable only by reading every connector's source; it's now enforced as an exhaustive match in `crates/execution-ledger-domain/src/connectors/contracts.rs::consumes_upstream_output(kind, action)`, an exhaustive match over `ConnectorKind` with no `_` wildcard arm, so adding a new `ConnectorKind` variant without updating it is a compile error. **Check this function first** instead of re-deriving the answer by reading connector source. As of this writing:

- **Do compose** (fall back to the previous step's `{content, contentEncoding: "base64"}` output when the step's own `input`/`config` is empty): `azure_blob`, `sftp` (`download` action), `sftp` (`upload` action, only when its own `input` is empty/null; non-empty `input` still uploads verbatim, unchanged), `pgp` (both actions), `http_download` (both actions; `upload` also checks `response.value`, the shape `rest`/`erp` steps produce, so it can chain directly after an `erp` call).
- **Don't compose.** Only ever read their own static `step.input`/`step.config`: `rest`/`erp`, `transform`.

`contracts.rs` also has two more classifications built the same way, both exhaustive matches with same-file cross-check tests:

- `compensation_behavior(kind, action) -> CompensationBehavior`: whether a step's `target.compensate` restores prior state (`RestoresState`, currently only `azure_compute`/`azure_sql` resize actions and `queue`'s `receive` action), does nothing because there's nothing to undo (`NoopReadOnly`), does nothing because the side effect can't be safely undone (`NoopIrreversible`, e.g. `ms_graph_notify`, any upload action), or calls a job-author-configured `compensateUrl` if one was set (`CallsConfiguredReverseEndpoint`, for `rest`/`erp`).
- `replay_safety(kind, action) -> ReplaySafety`: whether replaying a step is `Idempotent` or `NotSafeWithoutGuard` (re-executing repeats a real external action: `rest`/`erp`, `queue` `send`, `ms_graph_notify`, `sftp` `upload`). **This one is wired into a live guardrail, not just documentation.** See below.
- `credential_posture(kind) -> CredentialPosture`: `ManagedIdentityOnly`, `AcceptsCredentialConfig` (`sftp`, `pgp`), or `NoAuth` (connectors with no credential handling of their own, e.g. `rest`/`erp`, which explicitly denylist forwarding an `Authorization`/`Cookie` header; see `rest.rs`'s `DENYLISTED_HEADERS`).

**The replay-safety guardrail is live, not just a classification.** `execution-ledger-domain::connectors::contracts::unsafe_replay_steps(input_data, step_ids)` is the shared function both `execution-ledger-api`'s `POST /platform/jobs/{id}/replay` route and `execution-ledger-cli`'s `job-replay` command call before mutating anything: if any explicitly targeted step (non-empty `step_ids`) classifies as `NotSafeWithoutGuard`, the replay is rejected (API: `409` with an `unsafeSteps` JSON body; CLI: `bail!` with the step list) unless the caller passes `force: true` (API body field) / a 4th positional `force` argument (CLI). An empty `step_ids` (resume/retry of incomplete steps, not a forced re-run of completed ones) is not gated; there's no prior external action being repeated in that case.

**`execution-ledger-cli validate-job <path>`** is a static job-template linter. No database/config/Service Bus needed. It surfaces, before a job ever runs: (1) a step with empty `input`/`data` following a step whose connector doesn't chain, where that gap looks like an authoring mistake rather than deliberate (see `likely_missing_chain` below, the check that actually catches the `transform` trap at authoring time); (2) which steps would need `force` to replay; (3) which steps restore state on compensate, as a reminder to include a capture step. It accepts both job-template shapes that coexist in this repo's `connector-packs/`: flat `{"id", "steps": [...]}` and wrapped `{"jobId", "data": {"steps": [...]}}`.

**Don't use `!consumes_upstream_output(...)` directly to flag "did the author forget to chain"; use `likely_missing_chain(kind, action)` instead.** The first version of `validate-job` used the raw negation and immediately false-positived on every `blob_archive` job template in this repo: `blob_archive` deliberately never reads `ctx.upstream_output` (documented, correct behavior; it owns its own blob I/O), but a bare `!consumes_upstream_output` can't distinguish "shouldn't chain by design" from "should chain and doesn't." `likely_missing_chain` narrows this to the specific connectors where the gap is an actual, unresolved authoring trap (`transform`, `rest`, `erp`), confirmed by running it against all 19 real job templates already in `connector-packs/` before trusting it, not just synthetic test cases. If you add a similar "warn a human about this" check on top of a contracts.rs classification, sweep it against the real corpus before trusting the signal. A classification that's accurate in the abstract can still produce false positives once real, deliberately-atypical job templates hit it.

**`transform` is a trap.** Its module doc says it "reshapes data between steps," but `apply_map()` maps against `step.input` only, never `ctx.upstream_output`. A job template like `erp (get a URL) → transform (lift response.value into a url field) → next-step` will not work: `transform`'s output reshapes its own (likely empty) input, not the previous step's response. This looks like a pre-existing gap in `transform`, not a deliberate boundary (contrast with `blob_archive`, which deliberately owns its own blob I/O and documents why). If a new connector needs to consume a nested field from an upstream `rest`/`erp` response, have that connector check `ctx.upstream_output` directly for the shapes it needs (mirroring `http_download`'s `url` resolution) rather than routing through `transform`.

**If you add an upstream-output fallback to a connector that currently only reads its own `input` unconditionally**, gate it on emptiness. Don't make upstream output take priority over an explicit non-empty `input`, or you silently change behavior for every existing job template that relies on the current default:

```rust
let input_is_empty = matches!(&step.input, Value::Null)
    || matches!(&step.input, Value::Object(map) if map.is_empty());
if !input_is_empty {
    // existing behavior, unchanged: explicit input always wins
}
// only now fall back to ctx.upstream_output
```

### No fan-out — a real constraint, design job templates around it

`ctx.upstream_output` reflects **only the one step immediately before the current one** (see `crates/execution-ledger-worker/src/main.rs`: a single `upstream_output` variable is overwritten after every step completes). There is no reading two steps back, and no branching to two different consumers of the same output. There is also no automatic state passing across *separate* `job-launch` calls — a new run starts with no memory of a prior run's output.

Design implication for job templates: if a later step needs an earlier, non-adjacent step's output (a captured `executionId`, a writable SAS URL), it cannot read it automatically. Either reorder the steps so the real consumer is immediately after the producer, or have the job author/caller read the value from the earlier run's persisted step output (`jobs.get_steps`) and pass it explicitly into the next step's `config`/`input`, or the next job's `job-launch` payload. This is a deliberate trade for step-level replay, not a bug — but it caps how far a single declarative job template can express a multi-hop dependency chain.

### Verifying third-party crate APIs before writing code against them

`docs.rs` is unreachable via this environment's web-fetch tooling (confirmed: every attempt 404's, even for crates with normal published docs). Generic web summaries of a crate's API can also be subtly wrong — a fetched summary once suggested `Message::from_armor_single` exists on the `pgp` crate's `Message` type; it doesn't (that crate's `Message` has its own inherent `from_armor`/`from_bytes`/`from_reader`/`from_armor_file`; only `SignedPublicKey`/`SignedSecretKey` use the `Deserializable` trait's `from_armor_single`). Only reading the real vendored source caught this, after the wrong assumption had already produced a compile error.

The reliable path, confirmed working in this environment:

```bash
cd execution-ledger
cargo add <crate>@<version> -p execution-ledger-domain   # resolves + writes Cargo.toml
cargo fetch                                                # vendors real source locally
find "$USERPROFILE/.cargo/registry/src/index.crates.io-"*/<crate>-<version>/src -maxdepth 2
```

Then Grep the vendored source for `pub fn `, `pub async fn `, `impl … for` before writing a line against it. After confirming the API, move the dependency from the crate's own `Cargo.toml` into the workspace root's `[workspace.dependencies]` (then `<crate>.workspace = true` in the crate's own `Cargo.toml`) to match this repo's existing convention — `cargo add` only touches the local crate's manifest by default, it does not promote to the workspace table for you.

### Known platform gaps — design around these, don't assume they're solved

- **No connector or API route exposes the KV store.** `kv_get`/`kv_set` (`execution-ledger-core/src/db.rs`) are CLI-internal helpers only, not a connector or `/platform/*` route. A job step cannot persist arbitrary cross-run state today — use the run/step history itself (`jobs.get_steps`) as the source of truth for anything a later step or job needs, carried forward by the caller.
- **`azure_blob` has no read/download action** — only `execute()` (upload). Reading a blob into the pipeline goes through `blob_archive` (which owns its own blob I/O for its zip/unzip operations) or a purpose-built connector; there is no `azure_blob` counterpart to `sftp`'s `download` action.
- **Failed step records are written outside the main transaction.** The worker's main `tx` commits on success and auto-rollbacks on error. The failed-step INSERT uses `pool` directly (not `&mut *tx`) so failed step records survive the rollback and are visible via `GET /platform/jobs/{id}/steps`. If you see `(no step records)` for a run that logged `worker.intent.failed`, the run was failing before the fix was applied — re-trigger to get fresh step-level diagnostics.
- **`thiserror #[source]` does not append to `Display` automatically.** `#[error("dependency '{dependency}' failed")]` will NOT include the underlying error in `error.to_string()`. You must use `#[error("dependency '{dependency}' failed: {source}")]` explicitly if you need the chain visible in logs. The `source` field is available via `.source()` regardless; the `Display` output is what ends up in structured log events.

### Replacing a legacy blocking poll loop — check Drasi's existing WAL reactivity before adding new surface

If porting a workflow that polls an external system's status in a loop until done (a common shape when replacing e.g. a Logic App's `Until` action), don't default to building a new Postgres control table plus a write connector. `execution-ledger-drasi` already streams WAL changes from `EXECUTION_LEDGER_jobs_runs` and reacts to completed/failed runs (see the "Drasi configuration" section above for the mechanics). Often the cleanest port is:

1. Model the poll as its own **short, scheduled job** — one status check per launch, not an in-step retry loop (a long external wait would hold one of a worker replica's bounded concurrent-intent slots the whole time).
2. Gate the poll step with an `output_schema` that only accepts the "done" status values, so an "not ready yet" response fails the step (and the run) closed — no dedicated conditional-branch connector needed.
3. Let *that job run's own completion* in `EXECUTION_LEDGER_jobs_runs` be the WAL event a Drasi continuous query/reaction triggers off of, to launch the next stage. Zero new connector, zero new control table.

Reach for a dedicated control table (the pattern in `azure-compute-scale-pack`'s `scale_override`, if present) only when there's no existing signal to reuse — e.g. an operator-driven override with nothing already tracking it. And before assuming a Drasi source for an external system applies, confirm it actually talks to the backend in question: some systems share a marketing name across different platforms with different endpoints (e.g. Dynamics 365 Finance & Operations vs. Dynamics 365 Customer Engagement/Dataverse) — a source built for one does not apply to the other by default.

## Rust conventions for Execution Ledger

These are the essential Rust patterns for this codebase.

### Error handling

Use `thiserror` for domain errors. The error type is `execution_ledger_core::ExecutionLedgerError`:

```rust
// At connector boundaries, map errors into the domain type:
Err(ExecutionLedgerError::InvalidConfiguration {
    name: "connector.rest.url".to_string(),
})

Err(ExecutionLedgerError::DependencyFailed {
    dependency: "rest_api",
    source: Box::new(e),
})
```

- Use `?` for propagation. Do not `match` when no additional handling is needed.
- Never `unwrap()` or `panic!()` in production code. The workspace denies both.
- Never leak secrets, tokens, or connection strings in error messages.

### Async conventions

- Use Tokio. All services run on `tokio` multi-threaded runtime.
- Never block the async runtime with sync I/O, CPU work, or `std::thread::sleep`.
- Use `tokio::time::sleep` for async delays.
- Use `tokio::task::spawn_blocking` only for unavoidable sync work (e.g. SFTP via `ssh2`).
- Connector functions are `async` because they may call external APIs. Pure transforms can still be `async` for consistency.

### Naming

- Types: `PascalCase` (`ConnectorKind`, `ConnectorStep`)
- Functions: `snake_case` (`execute_step`, `step_config_string`)
- Constants: `SCREAMING_SNAKE_CASE` (`DEFAULT_TIMEOUT`)
- Modules: `snake_case` (`platform_health`, `transform`)
- Connector serde names: `snake_case` via `#[serde(rename_all = "snake_case")]`

### Imports

- Keep `use` at the top of the module. Never glob-import (`use foo::*`) except `use super::*` in test modules.
- Use `crate::` paths for items elsewhere in the crate.
- Merge imports into grouped `use` blocks.

### Serde patterns

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorStep {
    pub id: String,
    pub connector: ConnectorKind,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default, alias = "data")]
    pub input: Value,
    #[serde(default)]
    pub config: Value,
}
```

- Use `#[serde(default)]` for optional fields with safe defaults.
- Use `#[serde(rename_all = "camelCase")]` for JSON API structs.
- Use `alias` when accepting legacy field names.

#### ConnectorKind serde names — verify before writing operator payloads

`ConnectorKind` uses `#[serde(rename_all = "snake_case")]`. Canonical serde names (as of the last check — verify against `mod.rs` before writing step definitions):

| Enum variant | Serde name (in step JSON) |
|---|---|
| `PlatformHealth` | `"platform_health"` |
| `Rest` | `"rest"` |
| `Erp` | `"erp"` |
| `Queue` | `"queue"` |
| `AzureBlob` | `"azure_blob"` (also accepts `"blob"` via alias) |
| `Sftp` | `"sftp"` |
| `MsGraphNotify` | `"ms_graph_notify"` |
| `AzureCompute` | `"azure_compute"` |
| `AzureSql` | `"azure_sql"` |
| `Transform` | `"transform"` |
| `Pgp` | `"pgp"` |
| `BlobArchive` | `"blob_archive"` |
| `HttpDownload` | `"http_download"` |

**Pitfall:** `AzureBlob` serializes as `"azure_blob"`, not `"blob"`. Tauri/operator payloads that use `"blob"` will deserialize correctly because of the alias, but canonical step definitions should use `"azure_blob"`. Using an unregistered name produces a `dependency 'serde_json' failed` error at the intent layer — the run fails before any step is executed and no step records are written, making the failure hard to diagnose without error-source chaining in the worker log.

### Testing

- Place unit tests in `#[cfg(test)] mod tests` inside the source file.
- Use `#[test]` for synchronous tests, `#[tokio::test]` for async.
- Test behavior, not implementation: happy path, missing config error, edge cases.
- No test framework beyond stdlib. No mocking of internal types.
- **`expect()`/`unwrap()` are NOT acceptable anywhere, including test fixtures — verified directly: `cargo clippy -p execution-ledger-domain --tests` denies both inside `#[cfg(test)]` modules.** The workspace lint (`unwrap_used = "deny"`, `expect_used = "deny"` in root `Cargo.toml`) applies to test code with no carve-out. When a test needs a fallible call, give the test function a `Result` return type and propagate with `?`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolves_simple_path() {
        let input = json!({"orderId": "ABC"});
        assert_eq!(resolve_path(&input, "orderId"), Some(&json!("ABC")));
    }

    // When the test itself can fail (parsing, a Result-returning helper),
    // return Result and use `?` — not `.expect(...)`:
    #[test]
    fn round_trips_through_serialization() -> Result<(), Box<dyn std::error::Error>> {
        let input = json!({"orderId": "ABC"});
        let bytes = serde_json::to_vec(&input)?;
        let parsed: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(parsed, input);
        Ok(())
    }

    // ExecutionLedgerError doesn't convert into Box<dyn Error> everywhere for
    // free — map it through its Display first:
    #[tokio::test]
    async fn connector_call_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
        let result = some_connector_fn(/* ... */).await;
        let output = result.map_err(|e| e.to_string())?;
        assert_eq!(output.get("status"), Some(&json!("ok")));
        Ok(())
    }
}
```

Run `cargo clippy -p execution-ledger-domain --tests` (not just `cargo clippy` without `--tests`) before considering a connector done — the default clippy invocation does not lint test code, so a plain `cargo clippy` pass can look clean while the test module still fails under the workspace's actual denies.

### Verification commands

Before handing back changes, run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p execution-ledger-domain
```

`--all-targets` can't be dropped here — it's what makes clippy lint `#[cfg(test)]` code too (a plain `cargo clippy` with no target flags does not). Confirm test modules pass clippy specifically, not just `cargo check`/`cargo test` — this workspace's `unwrap_used`/`expect_used`/`panic` denies apply to test code with no carve-out, and `cargo test` alone won't surface a clippy-only violation.

## Anti-hallucination rule

For this skill, do not invent:

- environment variable names, queue names, table names, or API routes
- connector ids or payload contracts
- Azure role definition ids, scaler types, or Bicep parameter shapes
- APIM API IDs, named value IDs, policy header names, or App Configuration key names

Verify from repository source first, then Microsoft Learn for Azure platform details. If an identifier cannot be verified, stop and mark it `[VERIFY]` in the output.

## Final output contract

Return:

1. changed execution boundary and why
2. files changed with the invariant each file protects
3. verification commands run and results
4. any residual risk, `[VERIFY]` items, or follow-up work

## Performance tuning — measured latency bottlenecks

### Service Bus long-poll blocking

**Symptom**: Platform health checks take 60+ seconds on idle workers.

**Root cause**: Service Bus receiver long-poll blocks the worker thread for `SERVICEBUS_RECEIVE_WAIT_SECONDS`, delaying detection of new messages.

**Default tuning** (as of 2026-07-28):
- `SERVICEBUS_RECEIVE_WAIT_SECONDS = 5` (was 55)
- Impact: New messages detected within 5s instead of 55s
- Location: `crates/execution-ledger-core/src/queue.rs` line 10

**When to adjust**:
- Increase to 10+ if CPU polling overhead becomes visible (check `az containerapp metrics`)
- Decrease to 2 if you need sub-5s message latency for real-time scenarios
- Trade-off: lower poll interval = higher CPU, faster message detection

### Service Bus message lock duration cascading failures

**Symptom**: When worker replicas fill all slots with in-flight messages, new messages wait 5+ minutes before being redelivered, causing queue depth to grow unbounded.

**Root cause**: Message lock duration (`PT5M`) means crashed workers' messages aren't redelivered until the full lock period expires. If 5 replicas each hold 5 message slots (25 total), and all are blocked on long-running steps, the queue can back up.

**Default tuning** (as of 2026-07-28):
- `lockDuration = 'PT1M'` (was 'PT5M')
- Impact: Crashed worker messages redelivered within 1m instead of 5m, queue clears faster
- Location: `infra/modules/messaging.bicep` line 35
- Requires: `azd provision` to apply (queue must be recreated)

**When to adjust**:
- Increase to `PT2M` or `PT5M` if most steps legitimately take >1m to complete
- Keep at `PT1M` if steps are expected to complete in <1m (typical for REST/ERP/transforms)
- Watch for "message redelivered N times" events in logs — if frequent, increase lock duration

**Relationship to polling**:
- Both affect message pipeline latency, but independently
- Polling (5s) affects *idle* worker detection time
- Lock duration (1m) affects *crashed* worker recovery time
- Combined effect on healthy cluster: <10s e2e for health check (5s poll + queue processing time)

### KEDA scale-out thresholds

**Symptom**: Queue depth grows but workers don't scale; new jobs queue up instead of processing.

**Root cause**: KEDA scaler observes queue depth and scales when threshold is exceeded. Default threshold may not match your load pattern.

**Default tuning** (check in `infra/modules/container-apps.bicep`):
- Scale-out trigger: queue depth > 30 messages
- Min replicas: 1, Max replicas: 5
- Scale-down delay: 5 minutes

**When to adjust**:
- Lower threshold (e.g., 10) if you expect bursts and want aggressive early scale
- Raise threshold (e.g., 50) if small queue backlogs are normal and scaling overhead is costly
- Increase max replicas if you regularly see scale-outs hitting the ceiling

**Diagnosis**:
```bash
# Check current replica count
az containerapp replica list -n el-worker-prd -g rg-electionledgerprd

# Check queue depth and scaling metrics
az monitor metrics list --resource $(az containerapp show -n el-worker-prd -g rg-electionledgerprd --query id -o tsv) \
  --metric ProcessorCount
```

### Tauri schedule panel — polling frequency

The SchedulePanel auto-refreshes the active schedules list every 8 seconds to detect external changes (e.g., schedules created via API while the Tauri app is open).

**When to adjust**:
- Increase to 15-30s if network traffic or database load is a concern
- Decrease to 3-5s for real-time visibility
- Location: `tauri-operator/src/components/SchedulePanel.tsx` (useEffect refresh interval)

## Quality gate

Do not mark work complete until all checks pass:

- changed behavior executes through real code paths only
- failure handling remains explicit and fail-closed
- replay/idempotency semantics are preserved or intentionally updated with tests
- non-local Azure messaging/auth remains managed-identity based
- docs reflect the new operational behavior

## Currency

- last reviewed: 2026-07-28
- this session added: Performance tuning section (SB polling, lock duration, KEDA, Tauri refresh interval)
- primary sources:
  - `execution-ledger/README.md`
  - `execution-ledger/Cargo.toml` (workspace member/crate list and lint config — this is what caught the stale `execution-ledger-business` crate name previously in this skill; the crate is `execution-ledger-domain`, verified against `execution-ledger/crates/`)
  - `infra/main.bicep`
  - `infra/modules/app.bicep`
  - `infra/modules/configuration.bicep`
  - `infra/modules/apim.bicep`
  - `infra/modules/apim-policies/api-policy.xml`
  - `infra/modules/apim-policies/fragment-jwt-auth.xml`
  - `infra/modules/apim-policies/fragment-correlation-id.xml`
  - `execution-ledger/crates/execution-ledger-core/src/config.rs`
  - `execution-ledger/crates/execution-ledger-core/src/db.rs` (KV store is CLI-internal only, not a connector/route)
  - `execution-ledger/crates/execution-ledger-drasi/src/config.rs`
  - `execution-ledger/crates/execution-ledger-drasi/src/main.rs`
  - `execution-ledger/crates/execution-ledger-drasi/config/drasi.yaml`
  - `execution-ledger/crates/execution-ledger-drasi/config/queries/completed-job-runs.cypher`
  - `execution-ledger/crates/execution-ledger-domain/src/connectors/mod.rs` (connector list, `ConnectorKind`, dispatch pattern)
  - `execution-ledger/crates/execution-ledger-domain/src/connectors/contracts.rs` (`consumes_upstream_output`, `compensation_behavior`, `replay_safety`, `credential_posture`, `unsafe_replay_steps` — the enforced classifications, source of truth over re-deriving from individual connector reads)
  - `execution-ledger/crates/execution-ledger-domain/src/connectors/blob.rs`, `sftp.rs`, `rest.rs`, `transform.rs`, `pgp.rs`, `blob_archive.rs`, `http_download.rs` (what `contracts.rs`'s classifications are checked against)
  - `execution-ledger/crates/execution-ledger-worker/src/main.rs` (confirms `upstream_output` reflects only the immediately preceding step — no fan-out)
  - `execution-ledger/crates/execution-ledger-api/src/routes/jobs.rs::replay_job_run` (live replay-safety guardrail, API side)
  - `execution-ledger/crates/execution-ledger-cli/src/main.rs` (`job-replay` — same guardrail, CLI side)
  - `execution-ledger/connector-packs/README.md` and `connector-packs/*/pack.json`, `runbook.md` (pack scaffold contract)
