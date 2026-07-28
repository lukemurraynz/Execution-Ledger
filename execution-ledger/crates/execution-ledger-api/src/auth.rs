use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use chrono::Utc;
use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::PgPool;
use uuid::Uuid;

use super::AppState;

pub const SCOPE_PLATFORM_READ: &str = "platform:read";
pub const SCOPE_PLATFORM_WRITE: &str = "platform:write";
pub const SCOPE_MCP_EXECUTE: &str = "mcp:execute";
pub const SCOPE_JOBS_READ_ANY: &str = "jobs:read:any";
pub const SCOPE_JOBS_READ_OWN: &str = "jobs:read:own";
pub const SCOPE_JOBS_WRITE_OWN: &str = "jobs:write:own";
const DEFAULT_APIM_CLIENT_ID: &str = "execution-ledger-apim";
const DEFAULT_APIM_CLIENT_DISPLAY_NAME: &str = "Execution Ledger APIM gateway";

#[derive(Debug, Clone)]
pub struct AuthorizedPrincipal {
    pub tenant_id: Option<Uuid>,
    pub scopes: Vec<String>,
}

impl AuthorizedPrincipal {
    pub fn has_scope(&self, required: &str) -> bool {
        self.scopes.iter().any(|scope| scope == required)
    }
}

/// Extracts and validates the bearer token, confirms the tenant is
/// onboarded and active, then attaches [`AuthenticatedTenant`] to the
/// request for handlers to scope their queries by.
pub async fn require_authenticated_tenant(
    State(state): State<Arc<AppState>>,
    mut request: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    enforce_gateway_header(request.headers(), state.apim_gateway_secret.as_deref())?;

    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let authenticated = state.auth.validate(token).await.map_err(|error| {
        tracing::warn!(%error, "bearer token rejected");
        StatusCode::UNAUTHORIZED
    })?;
    let required_scopes = if request.method() == axum::http::Method::GET {
        [
            SCOPE_JOBS_READ_OWN,
            SCOPE_JOBS_READ_ANY,
            SCOPE_PLATFORM_READ,
            SCOPE_PLATFORM_WRITE,
        ]
        .as_slice()
    } else {
        [SCOPE_JOBS_WRITE_OWN, SCOPE_PLATFORM_WRITE].as_slice()
    };
    if !authenticated.has_any_scope(required_scopes) {
        return Err(StatusCode::FORBIDDEN);
    }

    match execution_ledger_core::db::is_tenant_active(&state.pool, authenticated.tenant_id).await {
        Ok(true) => {}
        Ok(false) => {
            let can_bootstrap = state
                .home_tenant_id
                .is_some_and(|home_tenant_id| home_tenant_id == authenticated.tenant_id);
            if !can_bootstrap {
                return Err(StatusCode::FORBIDDEN);
            }

            execution_ledger_core::db::ensure_tenant_active(
                &state.pool,
                authenticated.tenant_id,
                &format!("Entra tenant {}", authenticated.tenant_id),
            )
            .await
            .map_err(|error| {
                tracing::error!(%error, "home tenant bootstrap failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        }
        Err(error) => {
            tracing::error!(%error, "tenant lookup failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    request.extensions_mut().insert(authenticated);
    Ok(next.run(request).await)
}

/// Authorizes platform read endpoints with either a valid bearer token from an
/// active tenant, or a valid APIM trust context.
pub async fn require_authorized_principal(
    State(state): State<Arc<AppState>>,
    mut request: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    enforce_gateway_header(request.headers(), state.apim_gateway_secret.as_deref())?;

    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));

    if let Some(token) = bearer
        && let Ok(authenticated) = state.auth.validate(token).await
    {
        if !authenticated.has_any_scope(&[SCOPE_PLATFORM_READ, SCOPE_PLATFORM_WRITE]) {
            return Err(StatusCode::FORBIDDEN);
        }
        match execution_ledger_core::db::is_tenant_active(&state.pool, authenticated.tenant_id)
            .await
        {
            Ok(true) => {
                request.extensions_mut().insert(authenticated);
                return Ok(next.run(request).await);
            }
            Ok(false) => {}
            Err(error) => {
                tracing::error!(%error, "tenant lookup failed");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    let scopes = authorize_apim_client(
        request.headers(),
        request.method().as_str(),
        request.uri().path_and_query().map_or_else(
            || request.uri().path(),
            axum::http::uri::PathAndQuery::as_str,
        ),
        &state.pool,
        state.apim_gateway_secret.as_deref(),
        SCOPE_PLATFORM_READ,
    )
    .await?;
    let tenant = resolve_apim_tenant(&state.pool, request.headers(), scopes).await?;
    request.extensions_mut().insert(tenant);

    Ok(next.run(request).await)
}

/// Authorizes platform mutating endpoints (`platform:write`).
pub async fn require_authorized_writer(
    State(state): State<Arc<AppState>>,
    mut request: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    enforce_gateway_header(request.headers(), state.apim_gateway_secret.as_deref())?;

    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));

    if let Some(token) = bearer
        && let Ok(authenticated) = state.auth.validate(token).await
    {
        if !authenticated.has_scope(SCOPE_PLATFORM_WRITE) {
            return Err(StatusCode::FORBIDDEN);
        }
        match execution_ledger_core::db::is_tenant_active(&state.pool, authenticated.tenant_id)
            .await
        {
            Ok(true) => {
                request.extensions_mut().insert(authenticated);
                return Ok(next.run(request).await);
            }
            Ok(false) => {}
            Err(error) => {
                tracing::error!(%error, "tenant lookup failed");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    let scopes = authorize_apim_client(
        request.headers(),
        request.method().as_str(),
        request.uri().path_and_query().map_or_else(
            || request.uri().path(),
            axum::http::uri::PathAndQuery::as_str,
        ),
        &state.pool,
        state.apim_gateway_secret.as_deref(),
        SCOPE_PLATFORM_WRITE,
    )
    .await?;
    let tenant = resolve_apim_tenant(&state.pool, request.headers(), scopes).await?;
    request.extensions_mut().insert(tenant);

    Ok(next.run(request).await)
}

// ── Internal service-to-service auth (Drasi → API) ──────────────────────────

/// Validates internal service calls (Drasi, scheduler companion) by checking
/// the Bearer token against the configured APIM gateway secret using
/// constant-time comparison.
///
/// Unlike tenant-facing middleware, this does not require JWT validation or
/// tenant lookup — the caller is a trusted internal service running in the
/// same Container Apps Environment.
pub async fn require_internal_service(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(expected) = state.apim_gateway_secret.as_deref() else {
        // No gateway secret configured — local/dev mode, allow through
        return Ok(next.run(request).await);
    };

    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;

    // Constant-time comparison to prevent timing side-channel attacks
    let mut mac_actual =
        Hmac::<Sha256>::new_from_slice(b"internal-service-auth").map_err(|_| StatusCode::UNAUTHORIZED)?;
    mac_actual.update(token.as_bytes());
    let actual_tag = mac_actual.finalize().into_bytes();
    let mut mac_expected =
        Hmac::<Sha256>::new_from_slice(b"internal-service-auth").map_err(|_| StatusCode::UNAUTHORIZED)?;
    mac_expected.update(expected.as_bytes());
    let expected_tag = mac_expected.finalize().into_bytes();

    if hmac::digest::CtOutput::<Sha256>::from(&actual_tag)
        != hmac::digest::CtOutput::<Sha256>::from(&expected_tag)
    {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(request).await)
}

/// Authorizes MCP endpoints and attaches principal context for scope-based
/// tool filtering and call enforcement.
pub async fn require_authorized_mcp_principal(
    State(state): State<Arc<AppState>>,
    mut request: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    enforce_gateway_header(request.headers(), state.apim_gateway_secret.as_deref())?;

    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));

    if let Some(token) = bearer
        && let Ok(authenticated) = state.auth.validate(token).await
    {
        if !authenticated.has_scope(SCOPE_MCP_EXECUTE) {
            return Err(StatusCode::FORBIDDEN);
        }
        match execution_ledger_core::db::is_tenant_active(&state.pool, authenticated.tenant_id)
            .await
        {
            Ok(true) => {
                request.extensions_mut().insert(AuthorizedPrincipal {
                    tenant_id: Some(authenticated.tenant_id),
                    scopes: authenticated.scopes,
                });
                return Ok(next.run(request).await);
            }
            Ok(false) => {}
            Err(error) => {
                tracing::error!(%error, "tenant lookup failed");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    let scopes = authorize_apim_client(
        request.headers(),
        request.method().as_str(),
        request.uri().path_and_query().map_or_else(
            || request.uri().path(),
            axum::http::uri::PathAndQuery::as_str,
        ),
        &state.pool,
        state.apim_gateway_secret.as_deref(),
        SCOPE_MCP_EXECUTE,
    )
    .await?;

    request.extensions_mut().insert(AuthorizedPrincipal {
        tenant_id: None,
        scopes,
    });
    Ok(next.run(request).await)
}

pub fn enforce_gateway_header(
    headers: &HeaderMap,
    shared_secret: Option<&str>,
) -> Result<(), StatusCode> {
    if let Some(expected) = shared_secret {
        let actual = headers
            .get("x-execution-ledger-gateway-secret")
            .and_then(|value| value.to_str().ok());
        match actual {
            Some(actual) => {
                // F09 fix: use constant-time comparison to prevent timing
                // side-channel attacks against the gateway secret.
                let mut mac_actual = Hmac::<Sha256>::new_from_slice(b"gateway-secret-compare")
                    .map_err(|_| StatusCode::UNAUTHORIZED)?;
                mac_actual.update(actual.as_bytes());
                let actual_tag = mac_actual.finalize().into_bytes();
                let mut mac_expected = Hmac::<Sha256>::new_from_slice(b"gateway-secret-compare")
                    .map_err(|_| StatusCode::UNAUTHORIZED)?;
                mac_expected.update(expected.as_bytes());
                let expected_tag = mac_expected.finalize().into_bytes();
                if hmac::digest::CtOutput::<Sha256>::from(&actual_tag)
                    != hmac::digest::CtOutput::<Sha256>::from(&expected_tag)
                {
                    return Err(StatusCode::UNAUTHORIZED);
                }
            }
            None => return Err(StatusCode::UNAUTHORIZED),
        }
    }
    Ok(())
}

pub async fn authorize_apim_client(
    headers: &HeaderMap,
    method: &str,
    request_uri: &str,
    pool: &PgPool,
    shared_secret: Option<&str>,
    required_scope: &str,
) -> Result<Vec<String>, StatusCode> {
    let Some(secret) = shared_secret else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    let client_id = headers
        .get("x-execution-ledger-client-id")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let timestamp = headers
        .get("x-execution-ledger-apim-timestamp")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let signature = headers
        .get("x-execution-ledger-apim-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let ts =
        chrono::DateTime::parse_from_rfc3339(timestamp).map_err(|_| StatusCode::UNAUTHORIZED)?;
    let skew = (Utc::now() - ts.with_timezone(&Utc)).num_seconds().abs();
    if skew > 300 {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let mut candidates = Vec::with_capacity(2);
    candidates.push(request_uri);
    if let Some((path_only, _)) = request_uri.split_once('?')
        && path_only != request_uri
    {
        candidates.push(path_only);
    }

    let provided = signature
        .strip_prefix("sha256=")
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let mut valid_signature = false;
    for candidate in candidates {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
            .map_err(|_| StatusCode::UNAUTHORIZED)?;
        let material = format!("{method}|{candidate}|{client_id}|{timestamp}");
        mac.update(material.as_bytes());
        let expected_bytes = mac.finalize().into_bytes();
        let provided_bytes = hex::decode(provided).map_err(|_| StatusCode::UNAUTHORIZED)?;
        // Constant-time comparison: XOR every byte pair and check no differences.
        // Prevents timing side-channel attacks against the APIM signature.
        if expected_bytes.len() == provided_bytes.len() {
            let mut diff = 0u8;
            for (a, b) in expected_bytes.iter().zip(provided_bytes.iter()) {
                diff |= a ^ b;
            }
            if diff == 0 {
                valid_signature = true;
                break;
            }
        }
    }
    if !valid_signature {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let mut row = sqlx::query_as::<_, (String, serde_json::Value)>(
        "SELECT status, scopes
         FROM EXECUTION_LEDGER_api_clients
         WHERE client_id = $1",
    )
    .bind(client_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        tracing::error!(%error, "api client lookup failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let should_repair_default = client_id == DEFAULT_APIM_CLIENT_ID
        && row.as_ref().is_none_or(|(status, scopes)| {
            status != "active"
                || !has_required_scopes(
                    scopes,
                    &[SCOPE_PLATFORM_READ, SCOPE_PLATFORM_WRITE, SCOPE_MCP_EXECUTE],
                )
        });
    if should_repair_default {
        row = Some(
            sqlx::query_as::<_, (String, serde_json::Value)>(
                "INSERT INTO EXECUTION_LEDGER_api_clients (client_id, display_name, status, scopes)
                 VALUES ($1, $2, 'active', jsonb_build_array('platform:read', 'platform:write', 'mcp:execute'))
                 ON CONFLICT (client_id) DO UPDATE
                 SET display_name = EXCLUDED.display_name,
                     status = 'active',
                     scopes = EXCLUDED.scopes,
                     updated_at = now()
                 RETURNING status, scopes",
            )
            .bind(DEFAULT_APIM_CLIENT_ID)
            .bind(DEFAULT_APIM_CLIENT_DISPLAY_NAME)
            .fetch_one(pool)
            .await
            .map_err(|error| {
                tracing::error!(%error, "default apim client bootstrap failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })?,
        );
    }

    let Some(row) = row else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    if row.0 != "active" {
        return Err(StatusCode::FORBIDDEN);
    }

    let has_scope = has_required_scopes(&row.1, &[required_scope]);
    if !has_scope {
        return Err(StatusCode::FORBIDDEN);
    }

    let scopes = row
        .1
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToString::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(scopes)
}

fn has_required_scopes(scopes: &serde_json::Value, required: &[&str]) -> bool {
    scopes.as_array().is_some_and(|items| {
        required.iter().all(|required_scope| {
            items
                .iter()
                .any(|item| item.as_str() == Some(*required_scope))
        })
    })
}

async fn resolve_apim_tenant(
    pool: &PgPool,
    headers: &HeaderMap,
    scopes: Vec<String>,
) -> Result<execution_ledger_core::AuthenticatedTenant, StatusCode> {
    let requested_tenant_id = headers
        .get("x-execution-ledger-tenant-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|_| StatusCode::UNAUTHORIZED)?;

    let tenant_id = if let Some(explicit_tenant_id) = requested_tenant_id {
        match execution_ledger_core::db::is_tenant_active(pool, explicit_tenant_id).await {
            Ok(true) => explicit_tenant_id,
            Ok(false) => return Err(StatusCode::FORBIDDEN),
            Err(error) => {
                tracing::error!(%error, "tenant lookup failed");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    } else {
        let tenant_from_recent_run = sqlx::query_scalar::<_, Uuid>(
            "SELECT r.tenant_id
             FROM EXECUTION_LEDGER_jobs_runs r
             JOIN EXECUTION_LEDGER_tenants t ON t.id = r.tenant_id
             WHERE t.status = 'active'
             ORDER BY r.updated_at DESC
             LIMIT 1",
        )
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            tracing::error!(%error, "default tenant lookup failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        if let Some(tenant_id) = tenant_from_recent_run {
            tenant_id
        } else {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT t.id
                 FROM EXECUTION_LEDGER_tenants t
                 WHERE t.status = 'active'
                 ORDER BY t.created_at DESC
                 LIMIT 1",
            )
            .fetch_optional(pool)
            .await
            .map_err(|error| {
                tracing::error!(%error, "default tenant lookup failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })?
            .ok_or(StatusCode::FORBIDDEN)?
        }
    };

    let subject = headers
        .get("x-execution-ledger-client-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or_else(
            || "apim:unknown".to_string(),
            |value| format!("apim:{value}"),
        );

    Ok(execution_ledger_core::AuthenticatedTenant {
        tenant_id,
        subject,
        scopes,
    })
}
