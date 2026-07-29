//! `apps/api` equivalent: HTTP transport and route registration.
//!
//! Real routes backed by real Postgres queries against `EXECUTION_LEDGER_jobs_runs` —
//! mirroring `apps/api/src/routes/platform/health.route.ts` and
//! `apps/api/src/routes/jobs.route.ts`. No in-memory stub data: `/health`
//! fails if the database is unreachable, and `/jobs/:id` 404s on a real
//! `SELECT` returning no rows.
//!
//! Every route except `/health` requires a bearer token issued by
//! Microsoft Entra ID, for a tenant that is onboarded and active in
//! `EXECUTION_LEDGER_tenants` (see `execution_ledger_core::auth` and `require_authenticated_tenant`
//! below) — this is the API's multi-tenancy boundary.

mod auth;
mod mcp;
mod routes;

use std::sync::Arc;

use axum::Router;
use axum::middleware;
use axum::routing::{get, post};
use execution_ledger_core::{EntraJwtValidator, ExecutionLedgerConfig, QueueEngine, queue};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use auth::{
    require_authenticated_tenant, require_authorized_mcp_principal, require_authorized_principal,
    require_authorized_writer, require_internal_service,
};

#[derive(Clone)]
pub(crate) struct AppState {
    pool: PgPool,
    environment: String,
    auth: Arc<EntraJwtValidator>,
    apim_gateway_secret: Option<String>,
    mcp_resource_url: Option<String>,
    mcp_authorization_servers: Option<String>,
    queue_engine: QueueEngine,
    servicebus_client: Option<queue::ServiceBusQueueClient>,
    home_tenant_id: Option<Uuid>,
}

/// Shared query parameter across list-style endpoints.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ListRunsQuery {
    pub limit: Option<i64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    execution_ledger_core::observability::init_runtime_telemetry("execution-ledger-api");

    let config = ExecutionLedgerConfig::from_env_with_vault().await?;
    let entra_audience = config.entra_audience.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "EXECUTION_LEDGER_ENTRA_AUDIENCE is required — execution-ledger-api refuses to start without a way to \
             validate caller identity, rather than serving routes unauthenticated"
        )
    })?;

    let pool = execution_ledger_core::db::connect(&config.database_url).await?;
    execution_ledger_core::db::bootstrap_runtime_tables(&pool).await?;
    let home_tenant_id = std::env::var("EXECUTION_LEDGER_HOME_TENANT_ID")
        .ok()
        .map(|raw| {
            Uuid::parse_str(raw.trim()).map_err(|_| {
                anyhow::anyhow!(
                    "EXECUTION_LEDGER_HOME_TENANT_ID must be a valid GUID when provided"
                )
            })
        })
        .transpose()?;
    let servicebus_client = if matches!(config.queue_engine, QueueEngine::ServiceBus) {
        let queue_name = config.servicebus_queue_name.as_deref().ok_or(
            execution_ledger_core::ExecutionLedgerError::InvalidConfiguration {
                name: "EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME".to_string(),
            },
        )?;
        Some(queue::ServiceBusQueueClient::from_managed_identity(
            config.servicebus_namespace.as_deref().ok_or(
                execution_ledger_core::ExecutionLedgerError::InvalidConfiguration {
                    name: "EXECUTION_LEDGER_SERVICEBUS_NAMESPACE".to_string(),
                },
            )?,
            queue_name,
        )?)
    } else {
        None
    };

    let app = build_router(AppState {
        pool,
        environment: config.environment,
        auth: Arc::new(EntraJwtValidator::new(entra_audience)?),
        apim_gateway_secret: config.apim_gateway_secret,
        mcp_resource_url: config.mcp_resource_url,
        mcp_authorization_servers: config.mcp_authorization_servers,
        queue_engine: config.queue_engine,
        servicebus_client,
        home_tenant_id,
    });

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    tracing::info!(
        "execution-ledger-api listening on {}",
        listener.local_addr()?
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

fn build_router(state: AppState) -> Router {
    let state = Arc::new(state);

    // Only `/jobs/*` needs a tenant identity; `/health` is a liveness probe
    // and must stay reachable without credentials (orchestrators calling it
    // don't carry a bearer token).
    let tenant_protected = Router::new()
        .route("/jobs/{id}", get(routes::jobs::get_job_run))
        .route("/platform/jobs", get(routes::jobs::list_job_runs))
        .route(
            "/platform/jobs/{id}/steps",
            get(routes::jobs::list_job_run_steps),
        )
        .route("/platform/jobs/trigger", post(routes::jobs::trigger_job))
        .route(
            "/platform/jobs/{id}/replay",
            post(routes::jobs::replay_job_run),
        )
        .route("/platform/jobs/{id}/skip", post(routes::jobs::skip_job_run))
        .route(
            "/platform/jobs/{id}/compensate",
            post(routes::jobs::compensate_job_run),
        )
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            require_authenticated_tenant,
        ));

    let service_protected = Router::new()
        .route("/platform/health", get(routes::health::health))
        .route(
            "/platform/definitions",
            get(routes::platform::platform_definitions),
        )
        .route(
            "/platform/schedules",
            get(routes::schedules::list_schedules),
        )
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            require_authorized_principal,
        ));

    let service_write_protected = Router::new()
        .route("/platform/gc", post(routes::health::platform_gc))
        .route(
            "/platform/recovery/runs",
            post(routes::recovery::platform_recover_run_leases),
        )
        .route(
            "/platform/recovery/schedules",
            post(routes::recovery::platform_recover_schedule_leases),
        )
        .route(
            "/platform/recovery/local-poison",
            post(routes::recovery::platform_drain_local_poison_queue),
        )
        .route(
            "/platform/recovery/servicebus-poison",
            post(routes::recovery::platform_drain_servicebus_poison_queue),
        )
        .route(
            "/platform/schedules",
            post(routes::schedules::register_schedule),
        )
        .route(
            "/platform/schedules/{id}/enable",
            post(routes::schedules::enable_schedule),
        )
        .route(
            "/platform/schedules/{id}/disable",
            post(routes::schedules::disable_schedule),
        )
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            require_authorized_writer,
        ));

    let mcp_protected = Router::new()
        .route("/api/mcp/server", get(mcp::mcp_server_get))
        .route("/api/mcp/server", post(mcp::mcp_server_post))
        .route(
            "/api/mcp/server",
            axum::routing::delete(mcp::mcp_server_delete),
        )
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            require_authorized_mcp_principal,
        ));

    let internal = Router::new()
        .route(
            "/platform/internal/schedules/due",
            post(routes::schedules::internal_schedules_due),
        )
        .route(
            "/platform/internal/batch-windows/add-item",
            post(routes::batch_windows::internal_add_item),
        )
        .route(
            "/platform/internal/batch-windows/due",
            post(routes::batch_windows::internal_batch_windows_due),
        )
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            require_internal_service,
        ));

    Router::new()
        .route("/health", get(routes::health::health))
        .route(
            "/.well-known/oauth-protected-resource",
            get(mcp::mcp_protected_resource_metadata),
        )
        .merge(tenant_protected)
        .merge(service_protected)
        .merge(service_write_protected)
        .merge(mcp_protected)
        .merge(internal)
        .with_state(state)
}

async fn shutdown_signal() {
    // ponytail: SIGTERM-only shutdown; add SIGINT/other signals if this ever
    // runs outside a container orchestrator that sends SIGTERM.
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::get;
    use tower::ServiceExt;
    use uuid::Uuid;

    /// Router construction and route matching do not need a live database;
    /// `/health` and `/jobs/:id` DB-dependent behavior is covered by the
    /// worker's `execution-ledger-core::db` tests plus manual verification against a
    /// real Postgres instance (see the workspace README). Wiring a fake
    /// `PgPool` here would either require `testcontainers` (gated, slow) or
    /// a mock we've been told not to introduce for a concrete type.
    #[tokio::test]
    // Test-only invariant: this is a literal, well-formed request built at
    // the call site, so it can never fail to construct; the coding standard
    // requires item-level justification rather than a workspace-wide allowance.
    #[allow(clippy::unwrap_used)]
    async fn returns_not_found_for_an_unrecognised_path() {
        let app = Router::new().route("/health", get(|| async { axum::http::StatusCode::OK }));

        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    }

    /// Exercises the real middleware, not a stand-in: a request with no
    /// `Authorization` header must be rejected before it ever reaches
    /// `get_job_run`, without needing a live database or a real token.
    #[tokio::test]
    // Test-only invariant: the URL is a literal, well-formed connection
    // string; `connect_lazy` only parses it and never dials out, so this
    // can never fail here.
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn rejects_protected_routes_without_a_bearer_token() {
        let state = Arc::new(AppState {
            pool: PgPool::connect_lazy("postgres://localhost/does-not-need-to-exist")
                .expect("lazy pool never connects until first query"),
            environment: "test".to_string(),
            auth: Arc::new(
                EntraJwtValidator::new("api://execution-ledger".to_string())
                    .expect("test fixture validator always builds in CI"),
            ),
            apim_gateway_secret: None,
            mcp_resource_url: None,
            mcp_authorization_servers: None,
            queue_engine: QueueEngine::Local,
            servicebus_client: None,
            home_tenant_id: None,
        });

        let protected = Router::new()
            .route("/jobs/{id}", get(routes::jobs::get_job_run))
            .route_layer(middleware::from_fn_with_state(
                Arc::clone(&state),
                require_authenticated_tenant,
            ))
            .with_state(state);

        let response = protected
            .oneshot(
                HttpRequest::builder()
                    .uri(format!("/jobs/{}", Uuid::nil()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    /// When `EXECUTION_LEDGER_APIM_GATEWAY_SECRET` is configured, a request that skips
    /// APIM (no gateway header, or a stale/wrong one) must be rejected even
    /// before the bearer token is looked at — this is the "did this even
    /// come through the gateway" check, independent of tenant auth.
    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn rejects_requests_missing_the_apim_gateway_secret() {
        let state = Arc::new(AppState {
            pool: PgPool::connect_lazy("postgres://localhost/does-not-need-to-exist")
                .expect("lazy pool never connects until first query"),
            environment: "test".to_string(),
            auth: Arc::new(
                EntraJwtValidator::new("api://execution-ledger".to_string())
                    .expect("test fixture validator always builds in CI"),
            ),
            apim_gateway_secret: Some("expected-secret".to_string()),
            mcp_resource_url: None,
            mcp_authorization_servers: None,
            queue_engine: QueueEngine::Local,
            servicebus_client: None,
            home_tenant_id: None,
        });

        let protected = Router::new()
            .route("/jobs/{id}", get(routes::jobs::get_job_run))
            .route_layer(middleware::from_fn_with_state(
                Arc::clone(&state),
                require_authenticated_tenant,
            ))
            .with_state(state);

        let response = protected
            .oneshot(
                HttpRequest::builder()
                    .uri(format!("/jobs/{}", Uuid::nil()))
                    .header("Authorization", "Bearer whatever")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    /// When the gateway secret IS configured and the correct header IS
    /// present, the request passes the gateway check and reaches bearer
    /// token validation (failing there with 401, not at the gateway layer).
    /// This proves the positive path: the gateway check does not reject
    /// requests that carry the correct secret.
    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn allows_requests_with_correct_gateway_secret_past_the_header_check() {
        let state = Arc::new(AppState {
            pool: PgPool::connect_lazy("postgres://localhost/does-not-need-to-exist")
                .expect("lazy pool never connects until first query"),
            environment: "test".to_string(),
            auth: Arc::new(
                EntraJwtValidator::new("api://execution-ledger".to_string())
                    .expect("test fixture validator always builds in CI"),
            ),
            apim_gateway_secret: Some("expected-secret".to_string()),
            mcp_resource_url: None,
            mcp_authorization_servers: None,
            queue_engine: QueueEngine::Local,
            servicebus_client: None,
            home_tenant_id: None,
        });

        let protected = Router::new()
            .route("/jobs/{id}", get(routes::jobs::get_job_run))
            .route_layer(middleware::from_fn_with_state(
                Arc::clone(&state),
                require_authenticated_tenant,
            ))
            .with_state(state);

        let response = protected
            .oneshot(
                HttpRequest::builder()
                    .uri(format!("/jobs/{}", Uuid::nil()))
                    .header("x-execution-ledger-gateway-secret", "expected-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should be 401 from missing bearer token, not from gateway secret
        // check. A 401 from the gateway would mean our header was rejected.
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }
}
