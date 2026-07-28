# ADR: Azure API Management as the External Gateway and Policy Boundary

## Status

Accepted

## Context

`execution-ledger-api` already validates Microsoft Entra ID bearer tokens itself, at the middleware layer, before any route handler runs (see `execution-ledger-core::auth::EntraJwtValidator`). Tenant scoping and route-level scope checks also live in the API, so the API is safe to expose directly. The open question is whether it should be, or whether a dedicated gateway should sit in front of it.

A handful of cross-cutting concerns don't belong inside the orchestration runtime itself:

- Per-tenant rate limiting, so one noisy tenant can't starve others.
- CORS handling for browser-based callers (the Tauri operator app, or a future web console).
- Rejecting oversized request bodies before they reach the JWT validator or the database.
- Consistent, non-leaking error responses (masking backend stack traces/details on 401/429/5xx).
- A way to prove "this request actually came through the gateway," independent of tenant auth. Useful for defense in depth, and for keeping the Container Apps ingress non-public later if desired.

These are gateway-shaped problems, not orchestration-shaped problems. Building them into the Rust API would mean re-implementing a policy engine that Azure already provides as a managed service.

## Decision

Put Azure API Management (APIM) in front of `execution-ledger-api` as the preferred external entry point, enforced by a single policy document (`infra/modules/apim-policies/api-policy.xml`) applied at the API level. The policy runs, in order: correlation ID propagation, internal-header stripping, CORS, body-size limiting, JWT validation (defense in depth; the backend validates again independently), tenant-scoped rate limiting keyed on the APIM subscription, and gateway-secret injection. The secret is added to the request only after every prior check passes, then read by the API as proof the request came through APIM.

APIM deployment is a toggle, not a hard requirement: `infra/main.bicep`'s `apimDeploymentMode` parameter (`enabled` by default) conditionally deploys the `apim.bicep` module. When set to `disabled`, `azd` skips APIM and the Container Apps ingress URL is used directly. The API's own `EXECUTION_LEDGER_APIM_GATEWAY_SECRET` check only activates if a secret value is configured. With no secret configured, the API accepts direct calls as long as they carry a valid Entra token, so local development, CI, and disabled-APIM deployments stay unblocked without special-casing the auth code path.

## Consequences

### Positive

- Rate limiting, CORS, and body-size enforcement are centrally managed policy, not application code. Changing a limit is a policy edit and redeploy, not a Rust change.
- Defense in depth: JWT validation happens twice (gateway and backend), so a misconfigured or bypassed gateway policy doesn't silently drop backend auth.
- The gateway-secret check gives an explicit, testable signal that a request bypassed APIM (see `rejects_requests_missing_the_apim_gateway_secret` in `execution-ledger-api/src/main.rs`), which is useful both as a security control and as an operational alarm.
- Error responses are masked consistently at the edge, so backend implementation details (framework errors, stack traces, internal hostnames) never reach external callers.
- The toggle means the same Bicep/azd project serves both "I want a managed gateway" and "I want the simplest possible deployment" without a fork.

### Negative

- APIM is another Azure resource to provision, monitor, and pay for. It isn't free, and it adds a network hop to every request.
- Policy logic lives in XML (`api-policy.xml`), a different language and review discipline than the Rust codebase. Contributors need to understand APIM policy expressions to change gateway behavior.
- With `apimDeploymentMode: disabled`, the deployment loses rate limiting, CORS enforcement, body-size limiting, and the gateway-secret defense-in-depth layer. The API alone doesn't replace all of APIM's protections, only tenant auth and data scoping.
- The named-value placeholders in the policy (`{{cors-allowed-origin}}`, `{{rate-limit-calls-per-minute}}`, etc.) must stay in sync with the Bicep parameters that populate them; a drift between the two is a silent misconfiguration.

### Mitigations

- **Disabled-APIM deployments:** Documented as a reduced-protection mode, not a like-for-like substitute. Anyone disabling APIM in production should be deliberate about it and should evaluate whether they need equivalent protections elsewhere (a different gateway, or a Container Apps built-in control).
- **Policy/Bicep drift:** Named values are provisioned from the same Bicep parameters file (`main.parameters.json`) that configures the rest of the environment, so a single `azd provision` keeps them aligned; there is no separate manual step to forget.
- **Operational visibility:** APIM and the API both emit to the same Log Analytics workspace, so gateway-secret rejections and rate-limit hits are visible alongside application logs rather than in an isolated APIM-only view.

## Alternatives considered

- **No gateway, direct Container Apps ingress only.** Rejected as the default because it pushes rate limiting, CORS, and body-size enforcement into application code, and removes the "proof of transit" signal the gateway secret provides. Left available via `apimDeploymentMode: disabled` for deployments that have their own edge layer or don't need these controls.
- **Application-level rate limiting (in `execution-ledger-api` itself).** Rejected as the primary mechanism. It would require the API to track per-tenant counters (likely in Postgres or a cache), adding state and latency to every request for a concern that's naturally stateless at the edge.
- **A different Azure gateway (Front Door, Application Gateway).** Both operate at a different layer, mostly L7 routing and WAF, and neither provides the same first-class JWT-validation and per-subscription rate-limiting policy primitives that APIM does. APIM was chosen specifically for its policy expression language and native Entra JWT validation support.

## References

- `execution-ledger/README.md` — "Security posture", "Gateway and config bootstrap notes"
- `infra/modules/apim.bicep`, `infra/modules/apim-policies/api-policy.xml`
- `infra/main.bicep` — `apimDeploymentMode` parameter
- `execution-ledger/crates/execution-ledger-api/src/main.rs` — gateway-secret enforcement and tests
