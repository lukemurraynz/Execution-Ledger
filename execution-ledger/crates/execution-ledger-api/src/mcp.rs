use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::AppState;
use super::auth::{
    AuthorizedPrincipal, SCOPE_JOBS_READ_ANY, SCOPE_PLATFORM_READ, SCOPE_PLATFORM_WRITE,
};

const PROTOCOL_VERSION: &str = "2025-11-25";

// ── Tool descriptor ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct McpToolDescriptor {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotations>,
}

#[derive(Debug, Serialize)]
pub struct McpToolAnnotations {
    #[serde(rename = "readOnlyHint")]
    pub read_only_hint: bool,
    #[serde(rename = "destructiveHint")]
    pub destructive_hint: bool,
    #[serde(rename = "idempotentHint")]
    pub idempotent_hint: bool,
}

// ── RPC types ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct McpRpcRequest {
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct McpCallParams {
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpGetJobArguments {
    pub run_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpListJobsArguments {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpListSchedulesArguments {
    #[serde(default = "default_limit")]
    pub limit: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpRecoveryRunsArguments {
    #[serde(default = "default_recovery_limit")]
    pub limit: i64,
}

fn default_limit() -> i64 {
    50
}
fn default_recovery_limit() -> i64 {
    500
}

// ── Schema helpers ──────────────────────────────────────────────────────────

fn schema_object(properties: serde_json::Value, required: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn schema_string(description: &str) -> serde_json::Value {
    serde_json::json!({ "type": "string", "description": description })
}

fn schema_number(description: &str, minimum: i64, maximum: i64, default: i64) -> serde_json::Value {
    serde_json::json!({
        "type": "integer",
        "description": description,
        "minimum": minimum,
        "maximum": maximum,
        "default": default
    })
}

// ── Tool registry ───────────────────────────────────────────────────────────

#[allow(clippy::unnecessary_wraps)] // callers assign to Option<McpToolAnnotations> fields
fn ro_annotations() -> Option<McpToolAnnotations> {
    Some(McpToolAnnotations {
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
    })
}

#[allow(clippy::unnecessary_wraps)] // callers assign to Option<McpToolAnnotations> fields
fn rw_annotations() -> Option<McpToolAnnotations> {
    Some(McpToolAnnotations {
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: false,
    })
}

#[allow(clippy::unnecessary_wraps)] // callers assign to Option<McpToolAnnotations> fields
fn destructive_annotations() -> Option<McpToolAnnotations> {
    Some(McpToolAnnotations {
        read_only_hint: false,
        destructive_hint: true,
        idempotent_hint: false,
    })
}

fn mcp_visible_tools(principal: &AuthorizedPrincipal) -> Vec<McpToolDescriptor> {
    let mut tools = vec![McpToolDescriptor {
        name: "execution_ledger.health",
        title: "Platform health",
        description: "Return platform health status and environment.",
        input_schema: schema_object(serde_json::json!({}), &[]),
        annotations: ro_annotations(),
    }];

    if principal.tenant_id.is_some() || principal.has_scope(SCOPE_JOBS_READ_ANY) {
        tools.push(McpToolDescriptor {
            name: "execution_ledger.jobs.get",
            title: "Get job run",
            description: "Get a single job run by its run ID. Returns id, jobId, status, currentAttempt, and maxAttempts. Tenant-scoped when called by a tenant principal.",
            input_schema: schema_object(
                serde_json::json!({ "runId": schema_string("The run UUID to retrieve") }),
                &["runId"],
            ),
            annotations: ro_annotations(),
        });
        tools.push(McpToolDescriptor {
            name: "execution_ledger.jobs.list",
            title: "List job runs",
            description: "List job runs. Optionally filter by status (pending, running, completed, failed). Default limit 50, max 500. Tenant-scoped when called by a tenant principal.",
            input_schema: schema_object(
                serde_json::json!({
                    "status": serde_json::json!({
                        "type": "string",
                        "description": "Filter by status",
                        "enum": ["pending", "running", "completed", "failed"]
                    }),
                    "limit": schema_number("Maximum runs to return", 1, 500, 50)
                }),
                &[],
            ),
            annotations: ro_annotations(),
        });
        tools.push(McpToolDescriptor {
            name: "execution_ledger.jobs.get_steps",
            title: "Get job run steps",
            description: "Get step-level execution details for a job run. Returns step_id, status, attempt, output, error, and timestamps for each step attempt.",
            input_schema: schema_object(
                serde_json::json!({ "runId": schema_string("The run UUID to retrieve steps for") }),
                &["runId"],
            ),
            annotations: ro_annotations(),
        });
    }

    if principal.has_scope(SCOPE_PLATFORM_READ) {
        tools.push(McpToolDescriptor {
            name: "execution_ledger.schedules.list",
            title: "List schedules",
            description: "List all configured schedules with their cron expressions, next run times, status, and enabled state. Default limit 50, max 500.",
            input_schema: schema_object(
                serde_json::json!({ "limit": schema_number("Maximum schedules to return", 1, 500, 50) }),
                &[],
            ),
            annotations: ro_annotations(),
        });
    }

    if principal.has_scope(SCOPE_PLATFORM_WRITE) {
        tools.push(McpToolDescriptor {
            name: "execution_ledger.recovery.runs",
            title: "Recover stale run leases",
            description: "Release stale run leases. Runs with expired leases are requeued (pending) or marked failed if max attempts reached. Returns counts of requeued and failed runs. Default limit 500, max 5000.",
            input_schema: schema_object(
                serde_json::json!({ "limit": schema_number("Maximum runs to recover", 1, 5000, 500) }),
                &[],
            ),
            annotations: destructive_annotations(),
        });
        tools.push(McpToolDescriptor {
            name: "execution_ledger.schedules.enable",
            title: "Enable schedule",
            description: "Enable a schedule by its scheduler ID. The schedule will resume firing at its next cron trigger.",
            input_schema: schema_object(
                serde_json::json!({ "schedulerId": schema_string("The schedule identifier to enable") }),
                &["schedulerId"],
            ),
            annotations: rw_annotations(),
        });
        tools.push(McpToolDescriptor {
            name: "execution_ledger.schedules.disable",
            title: "Disable schedule",
            description: "Disable a schedule by its scheduler ID. The schedule will stop firing until re-enabled.",
            input_schema: schema_object(
                serde_json::json!({ "schedulerId": schema_string("The schedule identifier to disable") }),
                &["schedulerId"],
            ),
            annotations: rw_annotations(),
        });
    }

    tools
}

pub async fn mcp_server_get(
    Extension(principal): Extension<AuthorizedPrincipal>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": true,
        "transport": "streamable-http",
        "message": "execution-ledger MCP endpoint",
        "scopes": principal.scopes
    }))
}

pub async fn mcp_server_delete() -> StatusCode {
    StatusCode::NO_CONTENT
}

pub async fn mcp_server_post(
    State(state): State<Arc<AppState>>,
    Extension(principal): Extension<AuthorizedPrincipal>,
    Json(payload): Json<McpRpcRequest>,
) -> Json<serde_json::Value> {
    let response = match payload.method.as_str() {
        "initialize" => mcp_ok(
            payload.id.clone(),
            serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": true },
                "serverInfo": { "name": "execution-ledger-mcp", "version": "0.1.0" }
            }),
        ),
        "tools/list" => mcp_ok(
            payload.id.clone(),
            serde_json::json!({
                "tools": mcp_visible_tools(&principal)
            }),
        ),
        "tools/call" => match mcp_call_tool(&state, &principal, payload.params).await {
            Ok(result) => mcp_ok(payload.id.clone(), result),
            Err((code, message)) => mcp_error(payload.id.clone(), code, message),
        },
        _ => mcp_error(payload.id.clone(), -32601, "method not found"),
    };

    Json(response)
}

fn mcp_ok(id: Option<serde_json::Value>, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn mcp_error(id: Option<serde_json::Value>, code: i64, message: &'static str) -> serde_json::Value {
    let correlation_id = Uuid::new_v4().to_string();
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
            "data": {
                "correlationId": correlation_id
            }
        }
    })
}

async fn mcp_call_tool(
    state: &Arc<AppState>,
    principal: &AuthorizedPrincipal,
    params: Option<serde_json::Value>,
) -> std::result::Result<serde_json::Value, (i64, &'static str)> {
    let raw = params.ok_or((-32602, "missing params"))?;
    let call: McpCallParams =
        serde_json::from_value(raw).map_err(|_| (-32602, "invalid params"))?;

    match call.name.as_str() {
        "execution_ledger.health" => {
            sqlx::query("SELECT 1")
                .execute(&state.pool)
                .await
                .map_err(|_| (-32000, "database unavailable"))?;

            Ok(serde_json::json!({
                "tool": call.name,
                "content": [{
                    "type": "json",
                    "json": {
                        "status": "ok",
                        "environment": state.environment
                    }
                }]
            }))
        }
        "execution_ledger.jobs.get" => {
            if principal.tenant_id.is_none() && !principal.has_scope(SCOPE_JOBS_READ_ANY) {
                return Err((-32003, "forbidden"));
            }

            let args: McpGetJobArguments = serde_json::from_value(call.arguments)
                .map_err(|_| (-32602, "invalid arguments"))?;
            let run_id = Uuid::parse_str(&args.run_id).map_err(|_| (-32602, "invalid runId"))?;

            let row = if let Some(tenant_id) = principal.tenant_id {
                sqlx::query_as::<_, (Uuid, String, String, i32, i32)>(
                    "SELECT id, job_id, status, current_attempt, max_attempts
                     FROM EXECUTION_LEDGER_jobs_runs
                     WHERE id = $1 AND tenant_id = $2",
                )
                .bind(run_id)
                .bind(tenant_id)
                .fetch_optional(&state.pool)
                .await
                .map_err(|_| (-32000, "database query failed"))?
            } else {
                sqlx::query_as::<_, (Uuid, String, String, i32, i32)>(
                    "SELECT id, job_id, status, current_attempt, max_attempts
                     FROM EXECUTION_LEDGER_jobs_runs
                     WHERE id = $1",
                )
                .bind(run_id)
                .fetch_optional(&state.pool)
                .await
                .map_err(|_| (-32000, "database query failed"))?
            };

            let Some((id, job_id, status, current_attempt, max_attempts)) = row else {
                return Err((-32004, "job run not found"));
            };

            Ok(serde_json::json!({
                "tool": call.name,
                "content": [{
                    "type": "json",
                    "json": {
                        "id": id,
                        "jobId": job_id,
                        "status": status,
                        "currentAttempt": current_attempt,
                        "maxAttempts": max_attempts
                    }
                }]
            }))
        }
        "execution_ledger.jobs.list" => {
            if principal.tenant_id.is_none() && !principal.has_scope(SCOPE_JOBS_READ_ANY) {
                return Err((-32003, "forbidden"));
            }

            let args: McpListJobsArguments = serde_json::from_value(call.arguments)
                .map_err(|_| (-32602, "invalid arguments"))?;
            let limit = args.limit.clamp(1, 500);
            let status_filter = args.status.as_deref().unwrap_or("");

            let query = if principal.tenant_id.is_some() {
                if status_filter.is_empty() {
                    "SELECT row_to_json(t) FROM (
                        SELECT id, job_id, status, current_attempt, max_attempts, created_at
                        FROM EXECUTION_LEDGER_jobs_runs
                        WHERE tenant_id = $1
                        ORDER BY created_at DESC LIMIT $2
                    ) t"
                } else {
                    "SELECT row_to_json(t) FROM (
                        SELECT id, job_id, status, current_attempt, max_attempts, created_at
                        FROM EXECUTION_LEDGER_jobs_runs
                        WHERE tenant_id = $1 AND status = $2
                        ORDER BY created_at DESC LIMIT $3
                    ) t"
                }
            } else {
                if status_filter.is_empty() {
                    "SELECT row_to_json(t) FROM (
                        SELECT id, job_id, tenant_id, status, current_attempt, max_attempts, created_at
                        FROM EXECUTION_LEDGER_jobs_runs
                        ORDER BY created_at DESC LIMIT $1
                    ) t"
                } else {
                    "SELECT row_to_json(t) FROM (
                        SELECT id, job_id, tenant_id, status, current_attempt, max_attempts, created_at
                        FROM EXECUTION_LEDGER_jobs_runs
                        WHERE status = $1
                        ORDER BY created_at DESC LIMIT $2
                    ) t"
                }
            };

            let mut q = sqlx::query_as::<_, (serde_json::Value,)>(query);
            if let Some(tenant_id) = principal.tenant_id {
                q = q.bind(tenant_id);
                if !status_filter.is_empty() {
                    q = q.bind(status_filter);
                }
                q = q.bind(limit);
            } else {
                if !status_filter.is_empty() {
                    q = q.bind(status_filter);
                }
                q = q.bind(limit);
            }
            let rows = q
                .fetch_all(&state.pool)
                .await
                .map_err(|_| (-32000, "database query failed"))?;

            let items: Vec<serde_json::Value> = rows.into_iter().map(|(v,)| v).collect();
            Ok(serde_json::json!({
                "tool": call.name,
                "content": [{ "type": "json", "json": { "runs": items, "count": items.len() } }]
            }))
        }
        "execution_ledger.jobs.get_steps" => {
            if principal.tenant_id.is_none() && !principal.has_scope(SCOPE_JOBS_READ_ANY) {
                return Err((-32003, "forbidden"));
            }

            let args: McpGetJobArguments = serde_json::from_value(call.arguments)
                .map_err(|_| (-32602, "invalid arguments"))?;
            let run_id = Uuid::parse_str(&args.run_id).map_err(|_| (-32602, "invalid runId"))?;

            let rows = sqlx::query_as::<_, (serde_json::Value,)>(
                "SELECT row_to_json(t) FROM (
                    SELECT step_id, status, attempt, output, error, started_at, completed_at
                    FROM EXECUTION_LEDGER_steps_runs
                    WHERE run_id = $1
                    ORDER BY attempt ASC, started_at ASC
                ) t",
            )
            .bind(run_id)
            .fetch_all(&state.pool)
            .await
            .map_err(|_| (-32000, "database query failed"))?;

            let items: Vec<serde_json::Value> = rows.into_iter().map(|(v,)| v).collect();
            Ok(serde_json::json!({
                "tool": call.name,
                "content": [{ "type": "json", "json": { "steps": items, "count": items.len() } }]
            }))
        }
        "execution_ledger.schedules.list" => {
            if !principal.has_scope(SCOPE_PLATFORM_READ) {
                return Err((-32003, "forbidden"));
            }

            let args: McpListSchedulesArguments = serde_json::from_value(call.arguments)
                .map_err(|_| (-32602, "invalid arguments"))?;
            let limit = args.limit.clamp(1, 500);

            let items = execution_ledger_core::db::list_schedules(&state.pool, limit)
                .await
                .map_err(|_| (-32000, "database query failed"))?;

            Ok(serde_json::json!({
                "tool": call.name,
                "content": [{ "type": "json", "json": { "schedules": items, "count": items.len() } }]
            }))
        }
        "execution_ledger.recovery.runs" => {
            if !principal.has_scope(SCOPE_PLATFORM_WRITE) {
                return Err((-32003, "forbidden"));
            }

            let args: McpRecoveryRunsArguments = serde_json::from_value(call.arguments)
                .map_err(|_| (-32602, "invalid arguments"))?;
            let limit = args.limit.clamp(1, 5000);

            let summary = execution_ledger_core::db::recover_expired_run_leases(&state.pool, limit)
                .await
                .map_err(|_| (-32000, "database query failed"))?;

            Ok(serde_json::json!({
                "tool": call.name,
                "content": [{ "type": "json", "json": {
                    "requeuedPending": summary.requeued_pending,
                    "failedTerminal": summary.failed_terminal
                }}]
            }))
        }
        "execution_ledger.schedules.enable" => {
            if !principal.has_scope(SCOPE_PLATFORM_WRITE) {
                return Err((-32003, "forbidden"));
            }

            let scheduler_id = call
                .arguments
                .get("schedulerId")
                .and_then(|v| v.as_str())
                .ok_or((-32602, "missing schedulerId"))?;

            let changed = execution_ledger_core::db::scheduler_set_status(
                &state.pool,
                scheduler_id,
                "ACTIVE",
            )
            .await
            .map_err(|_| (-32000, "database query failed"))?;

            if !changed {
                return Err((-32004, "schedule not found"));
            }

            Ok(serde_json::json!({
                "tool": call.name,
                "content": [{ "type": "json", "json": { "schedulerId": scheduler_id, "status": "ACTIVE" } }]
            }))
        }
        "execution_ledger.schedules.disable" => {
            if !principal.has_scope(SCOPE_PLATFORM_WRITE) {
                return Err((-32003, "forbidden"));
            }

            let scheduler_id = call
                .arguments
                .get("schedulerId")
                .and_then(|v| v.as_str())
                .ok_or((-32602, "missing schedulerId"))?;

            let changed = execution_ledger_core::db::scheduler_set_status(
                &state.pool,
                scheduler_id,
                "DISABLED",
            )
            .await
            .map_err(|_| (-32000, "database query failed"))?;

            if !changed {
                return Err((-32004, "schedule not found"));
            }

            Ok(serde_json::json!({
                "tool": call.name,
                "content": [{ "type": "json", "json": { "schedulerId": scheduler_id, "status": "DISABLED" } }]
            }))
        }
        _ => Err((-32601, "tool not found")),
    }
}

pub async fn mcp_protected_resource_metadata(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "resource": state.mcp_resource_url.clone().unwrap_or_else(|| "https://execution-ledger.local/api/mcp/server".to_string()),
        "authorization_servers": state
            .mcp_authorization_servers
            .as_deref()
            .map(|s| s.split(',').map(str::trim).filter(|v| !v.is_empty()).collect::<Vec<_>>())
            .unwrap_or_default()
    }))
}
