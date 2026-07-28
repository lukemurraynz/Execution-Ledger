# ADR: Query-Level Tenant Isolation

## Status

Accepted

## Context

Execution Ledger is a multi-tenant control plane: one deployment serves many tenants, and one tenant must never read, replay, skip, or compensate another tenant's job runs, steps, or schedules. Any Entra ID tenant can authenticate. The trust boundary is not "which Azure AD tenant issued the token" but "is this tenant marked `active` in `EXECUTION_LEDGER_tenants`" (see `execution-ledger-core::auth`'s module doc comment). That means the isolation guarantee has to be enforced somewhere between "token validated" and "row returned."

There are a few standard ways to build that boundary in a shared-database multi-tenant system:

- **Database-per-tenant.** Full physical isolation, but the runtime would need to provision, migrate, and connect to N databases, and connection pooling (already a constrained resource; see the scaling model's connection-count math) would multiply per tenant.
- **Schema-per-tenant.** Logical isolation within one database, but every migration and every query needs tenant-aware schema resolution, and the schema count grows unbounded with tenant count.
- **Row-level security (RLS).** Postgres-native, enforced by the database regardless of what the application code does, but it requires session-level tenant context (e.g. `SET LOCAL app.tenant_id`) to be set correctly on every connection checkout from the pool, and interacts with connection pooling and the `sqlx` query macros in ways that need careful verification.
- **Query-level scoping in application code.** Every table carries a `tenant_id` column, and every handler includes `AND tenant_id = $N` in its `WHERE` clause, with `tenant_id` sourced only from the validated JWT (`AuthenticatedTenant::tenant_id`), never from a request path or body parameter.

## Decision

Use query-level tenant isolation, enforced by convention and code review rather than by the database engine. Every tenant-owned table (`EXECUTION_LEDGER_jobs_runs`, `EXECUTION_LEDGER_steps_runs`, `EXECUTION_LEDGER_schedules`, etc.) carries a `tenant_id` column, and every route handler that reads or writes tenant data binds `authenticated.tenant_id`, extracted from the validated bearer token, into the query, never a client-supplied value. See `execution-ledger-api::routes::jobs::get_job_run` for the canonical pattern: `WHERE id = $1 AND tenant_id = $2`.

This keeps the runtime on a single shared schema, in a single database, with a single connection pool per service, which is also what the scaling model's connection-count budget (SKU `max_connections` divided across services and worker replicas) assumes.

## Consequences

### Positive

- One schema, one migration path, one connection pool per service. No per-tenant provisioning step, no schema-count growth, no N-times connection multiplication.
- The pattern is simple to audit: grep for queries against a tenant-owned table that lack a `tenant_id` predicate.
- No dependency on session-level state (like RLS's `SET LOCAL`) surviving correctly across `sqlx` pool checkouts. The tenant predicate is a plain bound parameter on the query the developer is already writing.
- Tenant onboarding is a row insert into `EXECUTION_LEDGER_tenants`, not an infrastructure operation.

### Negative

- **The isolation guarantee is only as strong as code review.** A handler that forgets the `tenant_id` predicate compiles, runs, and silently returns or mutates cross-tenant data. There is no database-level backstop that fails the query if the predicate is missing, the way RLS would.
- Every new route or query against a tenant-owned table is a place this can regress; the discipline doesn't scale automatically as the codebase grows or as more contributors touch route handlers.
- Existing tests catch this only if a test explicitly asserts cross-tenant access is rejected (as `execution-ledger-api`'s auth tests do for authentication, but isolation must be verified per-route).

### Mitigations

- **`tenant_id` never comes from the request.** It is populated exclusively from `AuthenticatedTenant::tenant_id`, itself derived only from the validated JWT's `tid` claim inside `EntraJwtValidator::validate`. There is no code path where a path parameter, query string, or body field can supply a tenant ID that reaches a query. This closes the most common variant of this bug class: a client simply asking for another tenant's ID.
- **Consistent handler shape.** New route handlers are expected to follow the same `WHERE ... AND tenant_id = $N` shape as existing ones (see CONTRIBUTING.md's guidance to follow established code conventions rather than introducing new ones); reviewers checking a new handler against this shape is the primary control.
- **Narrow blast radius via existing recovery/audit tooling.** Because every run and step carries `tenant_id`, a suspected leak is auditable after the fact from the same tables the recovery APIs already query. There is no separate audit log to build.

## Alternatives considered

- **Row-level security (RLS).** The strongest database-enforced option, and the natural next hardening step if query-level scoping proves insufficient at higher contributor counts or higher compliance bars. Deferred rather than rejected outright. It wasn't adopted initially because it requires verifying that `sqlx`'s connection pooling correctly applies `SET LOCAL app.tenant_id` on every checkout before any tenant-scoped query runs, and a wrong assumption there would produce a *worse* failure mode (silently unscoped queries) than the current explicit-parameter approach. Worth revisiting as a defense-in-depth layer on top of, not instead of, query-level scoping.
- **Schema-per-tenant or database-per-tenant.** Rejected for this platform's expected tenant counts and operational model. The connection-pool math in the scaling model already treats "per-service pool size times replica count" as the binding constraint; multiplying that by tenant count would make the documented SKU sizing guidance (`EXECUTION_LEDGER_PG_MAX_CONNECTIONS` per service) meaningless.

## References

- `execution-ledger/README.md`: "Security posture" (tenant-scoped data isolation), "Scaling model" (connection pool sizing)
- `execution-ledger/crates/execution-ledger-core/src/auth.rs`: module doc comment, `AuthenticatedTenant::tenant_id`, `EntraJwtValidator::validate`
- `execution-ledger/crates/execution-ledger-api/src/routes/jobs.rs`: `tenant_id`-scoped query pattern used across handlers
- `execution-ledger/CONTRIBUTING.md`: "Follow the existing code's conventions ... rather than introducing a new one"
