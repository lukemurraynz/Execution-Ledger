use chrono::{SecondsFormat, Utc};
use hmac::{Hmac, KeyInit, Mac};
use reqwest::Method;
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_APIM_CLIENT_ID: &str = "execution-ledger-apim";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiSession {
    base_url: String,
    bearer_token: Option<String>,
    gateway_secret: Option<String>,
    apim_client_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TriggerJobRequest {
    session: ApiSession,
    job_id: String,
    data: serde_json::Value,
    idempotency_key: Option<String>,
    correlation_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplayRunRequest {
    session: ApiSession,
    run_id: String,
    step_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SkipRunRequest {
    session: ApiSession,
    run_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompensateRunRequest {
    session: ApiSession,
    run_id: String,
    reason: Option<String>,
    data: serde_json::Value,
}

fn normalized_base_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("baseUrl is required".to_string());
    }
    Ok(trimmed.to_string())
}

fn authorized_token(session: &ApiSession) -> Result<String, String> {
    let token = session
        .bearer_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "bearer token is required for this action".to_string())?;
    Ok(token.to_string())
}

fn maybe_secret(session: &ApiSession) -> Option<String> {
    session
        .gateway_secret
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("disabled"))
        .map(ToString::to_string)
}

fn apim_client_id(session: &ApiSession) -> String {
    session
        .apim_client_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_APIM_CLIENT_ID)
        .to_string()
}

fn sign_apim_request(
    secret: &str,
    client_id: &str,
    method: &Method,
    request_uri: &str,
) -> Result<(String, String), String> {
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let material = format!(
        "{}|{}|{}|{}",
        method.as_str(),
        request_uri,
        client_id,
        timestamp
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|error| format!("failed to initialize APIM HMAC signer: {error}"))?;
    mac.update(material.as_bytes());
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    Ok((timestamp, signature))
}

async fn call_api(
    session: &ApiSession,
    method: Method,
    path: &str,
    body: Option<serde_json::Value>,
    require_auth: bool,
    extra_headers: &[(&str, String)],
) -> Result<serde_json::Value, String> {
    let base_url = normalized_base_url(&session.base_url)?;
    let url = format!("{base_url}{path}");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(45))
        .build()
        .map_err(|error| format!("failed to create HTTP client: {error}"))?;

    let mut request = client.request(method.clone(), url);
    let gateway_secret = maybe_secret(session);
    if require_auth {
        if let Ok(token) = authorized_token(session) {
            request = request.bearer_auth(token);
        } else if gateway_secret.is_none() {
            return Err("bearer token is required for this action".to_string());
        }
    }
    if let Some(secret) = gateway_secret {
        let client_id = apim_client_id(session);
        let (timestamp, signature) = sign_apim_request(&secret, &client_id, &method, path)?;
        request = request.header("x-execution-ledger-gateway-secret", secret);
        request = request.header("x-execution-ledger-client-id", client_id);
        request = request.header("x-execution-ledger-apim-timestamp", timestamp);
        request = request.header("x-execution-ledger-apim-signature", signature);
    }
    for (name, value) in extra_headers {
        request = request.header(*name, value);
    }
    if let Some(payload) = body {
        request = request.json(&payload);
    }

    let response = request
        .send()
        .await
        .map_err(|error| format!("request failed: {error}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| format!("failed to read response body: {error}"))?;

    if !status.is_success() {
        return Err(format!("api {status}: {text}"));
    }
    if text.trim().is_empty() {
        return Ok(json!({ "ok": true }));
    }
    serde_json::from_str(&text).map_err(|error| format!("invalid JSON response: {error}"))
}

#[tauri::command]
async fn app_health() -> Result<serde_json::Value, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("clock error: {error}"))?
        .as_secs();
    Ok(json!({
        "status": "ok",
        "source": "tauri-operator",
        "epochSeconds": now
    }))
}

#[tauri::command]
async fn api_health(session: ApiSession) -> Result<serde_json::Value, String> {
    call_api(&session, Method::GET, "/health", None, false, &[]).await
}

#[tauri::command]
async fn api_list_runs(
    session: ApiSession,
    limit: Option<i64>,
) -> Result<serde_json::Value, String> {
    let effective_limit = limit.unwrap_or(50).clamp(1, 200);
    let path = format!("/platform/jobs?limit={effective_limit}");
    call_api(&session, Method::GET, &path, None, true, &[]).await
}

#[tauri::command]
async fn api_get_run(session: ApiSession, run_id: String) -> Result<serde_json::Value, String> {
    let path = format!("/jobs/{run_id}");
    call_api(&session, Method::GET, &path, None, true, &[]).await
}

#[tauri::command]
async fn api_get_run_steps(
    session: ApiSession,
    run_id: String,
) -> Result<serde_json::Value, String> {
    let path = format!("/platform/jobs/{run_id}/steps");
    call_api(&session, Method::GET, &path, None, true, &[]).await
}

#[tauri::command]
async fn api_trigger_job(args: TriggerJobRequest) -> Result<serde_json::Value, String> {
    if args.job_id.trim().is_empty() {
        return Err("jobId is required".to_string());
    }
    let mut headers: Vec<(&str, String)> = Vec::new();
    if let Some(idempotency_key) = args
        .idempotency_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        headers.push(("idempotency-key", idempotency_key.to_string()));
    }
    if let Some(correlation_id) = args
        .correlation_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        headers.push(("x-correlation-id", correlation_id.to_string()));
    }

    call_api(
        &args.session,
        Method::POST,
        "/platform/jobs/trigger",
        Some(json!({
            "jobId": args.job_id,
            "data": args.data
        })),
        true,
        &headers,
    )
    .await
}

#[tauri::command]
async fn api_replay_run(args: ReplayRunRequest) -> Result<serde_json::Value, String> {
    let path = format!("/platform/jobs/{}/replay", args.run_id);
    call_api(
        &args.session,
        Method::POST,
        &path,
        Some(json!({ "stepIds": args.step_ids })),
        true,
        &[],
    )
    .await
}

#[tauri::command]
async fn api_skip_run(args: SkipRunRequest) -> Result<serde_json::Value, String> {
    let path = format!("/platform/jobs/{}/skip", args.run_id);
    call_api(&args.session, Method::POST, &path, None, true, &[]).await
}

#[tauri::command]
async fn api_compensate_run(args: CompensateRunRequest) -> Result<serde_json::Value, String> {
    let path = format!("/platform/jobs/{}/compensate", args.run_id);
    call_api(
        &args.session,
        Method::POST,
        &path,
        Some(json!({
            "reason": args.reason,
            "data": args.data
        })),
        true,
        &[],
    )
    .await
}

#[tauri::command]
async fn api_list_schedules(
    session: ApiSession,
    limit: Option<i64>,
) -> Result<serde_json::Value, String> {
    let effective_limit = limit.unwrap_or(50).clamp(1, 500);
    let path = format!("/platform/schedules?limit={effective_limit}");
    call_api(&session, Method::GET, &path, None, true, &[]).await
}

#[tauri::command]
async fn api_register_schedule(
    session: ApiSession,
    scheduler_id: String,
    job_id: String,
    cron_expression: String,
    timezone: Option<String>,
    next_run: Option<String>,
) -> Result<serde_json::Value, String> {
    call_api(
        &session,
        Method::POST,
        "/platform/schedules",
        Some(json!({
            "schedulerId": scheduler_id,
            "jobId": job_id,
            "cronExpression": cron_expression,
            "timezone": timezone,
            "nextRun": next_run,
        })),
        true,
        &[],
    )
    .await
}

#[tauri::command]
async fn api_enable_schedule(
    session: ApiSession,
    scheduler_id: String,
) -> Result<serde_json::Value, String> {
    let path = format!("/platform/schedules/{scheduler_id}/enable");
    call_api(&session, Method::POST, &path, None, true, &[]).await
}

#[tauri::command]
async fn api_disable_schedule(
    session: ApiSession,
    scheduler_id: String,
) -> Result<serde_json::Value, String> {
    let path = format!("/platform/schedules/{scheduler_id}/disable");
    call_api(&session, Method::POST, &path, None, true, &[]).await
}

#[tauri::command]
async fn api_recover_run_leases(
    session: ApiSession,
    limit: Option<i64>,
) -> Result<serde_json::Value, String> {
    let effective_limit = limit.unwrap_or(500).clamp(1, 5000);
    let path = format!("/platform/recovery/runs?limit={effective_limit}");
    call_api(&session, Method::POST, &path, None, true, &[]).await
}

#[tauri::command]
async fn api_recover_schedule_leases(
    session: ApiSession,
    limit: Option<i64>,
) -> Result<serde_json::Value, String> {
    let effective_limit = limit.unwrap_or(200).clamp(1, 5000);
    let path = format!("/platform/recovery/schedules?limit={effective_limit}");
    call_api(&session, Method::POST, &path, None, true, &[]).await
}

#[tauri::command]
async fn api_drain_local_poison(
    session: ApiSession,
    limit: Option<i64>,
) -> Result<serde_json::Value, String> {
    let effective_limit = limit.unwrap_or(500).clamp(1, 5000);
    let path = format!("/platform/recovery/local-poison?limit={effective_limit}");
    call_api(&session, Method::POST, &path, None, true, &[]).await
}

#[tauri::command]
async fn api_drain_servicebus_poison(
    session: ApiSession,
    limit: Option<i64>,
) -> Result<serde_json::Value, String> {
    let effective_limit = limit.unwrap_or(200).clamp(1, 5000);
    let path = format!("/platform/recovery/servicebus-poison?limit={effective_limit}");
    call_api(&session, Method::POST, &path, None, true, &[]).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            app_health,
            api_health,
            api_list_runs,
            api_get_run,
            api_get_run_steps,
            api_trigger_job,
            api_replay_run,
            api_skip_run,
            api_compensate_run,
            api_list_schedules,
            api_register_schedule,
            api_enable_schedule,
            api_disable_schedule,
            api_recover_run_leases,
            api_recover_schedule_leases,
            api_drain_local_poison,
            api_drain_servicebus_poison
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::{apim_client_id, maybe_secret, normalized_base_url, sign_apim_request, ApiSession};
    use reqwest::Method;

    #[test]
    fn normalizes_base_url() {
        let normalized = normalized_base_url(" https://example.com/path/ ").expect("url");
        assert_eq!(normalized, "https://example.com/path");
    }

    #[test]
    fn ignores_disabled_secret() {
        let session = ApiSession {
            base_url: "https://example.com".to_string(),
            bearer_token: None,
            gateway_secret: Some("disabled".to_string()),
            apim_client_id: None,
        };
        assert_eq!(maybe_secret(&session), None);
    }

    #[test]
    fn defaults_apim_client_id() {
        let session = ApiSession {
            base_url: "https://example.com".to_string(),
            bearer_token: None,
            gateway_secret: Some("secret".to_string()),
            apim_client_id: None,
        };
        assert_eq!(apim_client_id(&session), "execution-ledger-apim");
    }

    #[test]
    fn signs_apim_request() {
        let (timestamp, signature) =
            sign_apim_request("secret", "execution-ledger-apim", &Method::POST, "/health")
                .expect("signature");
        assert!(chrono::DateTime::parse_from_rfc3339(&timestamp).is_ok());
        assert!(signature.starts_with("sha256="));
        assert_eq!(signature.len(), 71);
    }
}
