//! Azure SQL connector.
//!
//! Resizes Azure SQL Database and SQL Elastic Pool resources via the ARM REST API,
//! authenticated with managed identity. Supports four actions:
//!
//! - **`sql-database-get`**: Reads current SKU, tier, license type, and capacity.
//! - **`sql-database-resize`**: Patches SQL Database SKU, tier, capacity, and license type.
//! - **`sql-elastic-pool-get`**: Reads current elastic pool SKU and capacity.
//! - **`sql-elastic-pool-resize`**: Patches elastic pool SKU and capacity.
//!
//! ## `LicenseType` handling
//!
//! This is a known Azure SQL API gap (first documented for this connector's
//! design during a review of [Bellhop](https://github.com/Azure/bellhop),
//! an archived, unrelated Microsoft autoscaling sample, issue #77 — cited
//! here only as the source of the observation, not as a dependency).
//!
//! SQL Database supports two licensing models:
//! - `LicenseType = "BasePrice"` — customer has Azure Hybrid Benefit, no SQL Server license cost added
//! - `LicenseType = "LicenseIncluded"` — SQL Server license included in the database cost
//!
//! When switching between DTU and vCore purchasing models, Azure may omit `licenseType`
//! in the current resource representation. If `licenseType` is absent and a customer had
//! Hybrid Benefit enabled, the scale-up restore would lose that flag and silently add cost.
//!
//!
//! The `sql-database-get` action captures `licenseType` in the state blob. The
//! `sql-database-resize` action accepts an explicit `licenseType` in `targetSku` to
//! ensure Hybrid Benefit is preserved across model switches. If not specified, the
//! current value is preserved.
//!
//! ## MSI Authentication
//!
//! Uses `azure_identity::ManagedIdentityCredential` to acquire a bearer token for
//! `https://management.azure.com/.default` — the same scope used by all Azure
//! management-plane APIs. No connection strings, no key vault secrets.
//!
//! ## Configuration
//!
//! | Key | Required | Notes |
//! |-----|----------|-------|
//! | `subscriptionId` | yes | Azure subscription ID |
//! | `resourceGroup` | yes | Target resource group name |
//! | `serverName` | yes | SQL Server name (not the FQDN) |
//! | `databaseName` | yes (database actions) | Database name |
//! | `elasticPoolName` | yes (pool actions) | Elastic pool name |
//! | `targetSku.name` | yes (resize) | e.g. `S0`, `S1`, `GP_S_Gen5_1`, `BC_Gen5_2` |
//! | `targetSku.tier` | no | e.g. `Basic`, `Standard`, `Premium`, `GeneralPurpose`, `BusinessCritical` |
//! | `targetSku.capacity` | no (DTU) | DTU unit count |
//! | `targetSku.family` | no (vCore) | Compute generation, e.g. `Gen5` |
//! | `targetSku/licenses` | no | vCore count (when using Hybrid Benefit) |
//! | `targetSku.licenseType` | no | `"BasePrice"` (Hybrid Benefit) or `"LicenseIncluded"` |
//! | `capacity` | yes (elastic pool) | Pool capacity in DTUs or vCores |
//!
//! ## Fail-closed behavior
//!
//! - Returns `InvalidConfiguration` if any required config key is missing.
//! - Returns `DependencyFailed` if the ARM token request fails.
//! - Returns `DependencyFailed` if the ARM API returns non-2xx.

// SKU capacities, DTU counts, vCore counts from ARM API responses
// are always small integers (max a few thousand). Use try_from to make
// the truncation boundary explicit rather than silent as-casting.

use execution_ledger_core::ExecutionLedgerError;
use reqwest::Method;
use serde_json::{Value, json};

use crate::connectors::arm::{self, ArmHeader};
use crate::connectors::helpers::{
    input_string, resource_group, step_config_object, step_config_string, subscription_id,
};
use crate::connectors::{ConnectorExecutionContext, ConnectorKind, ConnectorStep};

/// [VERIFIED] Azure SQL Database management REST API version is a real published version.
/// - SQL Databases: 2025-01-01
///   `WhereToCheck` = <https://learn.microsoft.com/en-us/rest/api/sql/databases/get?view=rest-sql-2025-01-01>
/// - SQL Elastic Pools: 2025-01-01
///   `WhereToCheck` = <https://learn.microsoft.com/en-us/rest/api/sql/elastic-pools/get?view=rest-sql-2025-01-01>
///
/// `EvidenceType` = Docs | Date = 2026-07-26
///
/// Resolves the SQL Server logical name from connector step configuration.
impl ConnectorStep {
    fn server_name(&self) -> Result<String, ExecutionLedgerError> {
        step_config_string(self, "serverName").ok_or_else(|| {
            ExecutionLedgerError::InvalidConfiguration {
                name: "connector.azure_sql.serverName".to_string(),
            }
        })
    }
}

/// Returns current SQL Database SKU, tier, capacity, and license type.
///
/// Use this in a step before `sql-database-resize` to capture the current
/// license type before switching purchasing models (DTU ↔ vCore).
/// The license type is stored in the `stateBlob` field of the response.
pub(crate) async fn sql_database_get(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let sub = subscription_id(step)?;
    let rg = resource_group(step)?;
    let server = step.server_name()?;
    let db_name = step_config_string(step, "databaseName")
        .or_else(|| input_string(step, "databaseName"))
        .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_sql.databaseName".to_string(),
        })?;

    let url = format!(
        "https://management.azure.com/subscriptions/{sub}/resourceGroups/{rg}/providers/Microsoft.Sql/servers/{server}/databases/{db_name}?api-version=2025-01-01"
    );

    let extra_headers: &[ArmHeader<'_>] = &[];
    let response = arm::arm_request(ctx, Method::GET, &url, json!({}), extra_headers).await?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or_else(|_| json!({}));

    let sku_name = body
        .get("sku")
        .and_then(|s| s.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("unknown")
        .to_string();

    let sku_tier = body
        .get("sku")
        .and_then(|s| s.get("tier"))
        .and_then(|t| t.as_str())
        .unwrap_or("unknown")
        .to_string();

    let sku_capacity: u32 = body
        .get("sku")
        .and_then(|s| s.get("capacity"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0);

    let sku_family = body
        .get("sku")
        .and_then(|s| s.get("family"))
        .and_then(|f| f.as_str())
        .map(std::string::ToString::to_string);

    // Bellhop issue #77: licenseType may be absent on DTU databases that have
    // Hybrid Benefit. Capture it explicitly here so restore can set it.
    let license_type = body
        .get("properties")
        .and_then(|p| p.get("licenseType"))
        .and_then(|l| l.as_str())
        .map(std::string::ToString::to_string);

    let provisioning_state = body
        .get("properties")
        .and_then(|p| p.get("provisioningState"))
        .and_then(|s| s.as_str())
        .unwrap_or("unknown")
        .to_string();

    let max_size_bytes = body
        .get("properties")
        .and_then(|p| p.get("maxSizeBytes"))
        .and_then(serde_json::Value::as_u64);

    Ok(json!({
        "stepId": step.id,
        "connector": "azure_sql",
        "action": "sql-database-get",
        "subscriptionId": sub,
        "resourceGroup": rg,
        "serverName": server,
        "databaseName": db_name,
        "currentSku": {
            "name": sku_name,
            "tier": sku_tier,
            "capacity": sku_capacity,
            "family": sku_family
        },
        "licenseType": license_type,
        "provisioningState": provisioning_state,
        "maxSizeBytes": max_size_bytes,
        "responseStatus": status.as_u16(),
        "stateBlob": {
            "sku": {
                "name": sku_name,
                "tier": sku_tier,
                "capacity": sku_capacity,
                "family": sku_family
            },
            "licenseType": license_type,
            "maxSizeBytes": max_size_bytes
        },
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Patches SQL Database SKU, tier, capacity, and license type.
///
/// For DTU → vCore or vCore → DTU model switches, include the `licenseType`
/// explicitly in `targetSku` to preserve Hybrid Benefit (Bellhop issue #77).
///
/// ARM returns 200 OK for sync updates or 202 Accepted with
/// `Azure-AsyncOperation` for long-running operations.
pub(crate) async fn sql_database_resize(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let sub = subscription_id(step)?;
    let rg = resource_group(step)?;
    let server = step.server_name()?;
    let db_name = step_config_string(step, "databaseName")
        .or_else(|| input_string(step, "databaseName"))
        .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_sql.databaseName".to_string(),
        })?;

    let target_sku = step_config_object(step, "targetSku").ok_or_else(|| {
        ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_sql.targetSku".to_string(),
        }
    })?;

    let sku_name = target_sku
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_sql.targetSku.name".to_string(),
        })?
        .to_string();

    let sku_tier = target_sku
        .get("tier")
        .and_then(|v| v.as_str())
        .map(String::from);

    let sku_capacity = target_sku
        .get("capacity")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok());

    let sku_family = target_sku
        .get("family")
        .and_then(|v| v.as_str())
        .map(String::from);

    let sku_licenses = target_sku
        .get("licenses")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok());

    // LicenseType: "BasePrice" = Hybrid Benefit, "LicenseIncluded" = full price.
    // If not specified in targetSku but present in step input, use input value.
    let license_type = target_sku
        .get("licenseType")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| input_string(step, "licenseType"));

    let mut body_map = serde_json::Map::new();

    let mut sku_map = serde_json::Map::new();
    sku_map.insert("name".to_string(), json!(sku_name));
    if let Some(ref tier) = sku_tier {
        sku_map.insert("tier".to_string(), json!(tier));
    }
    if let Some(capacity) = sku_capacity {
        sku_map.insert("capacity".to_string(), json!(capacity));
    }
    if let Some(ref family) = sku_family {
        sku_map.insert("family".to_string(), json!(family));
    }
    if let Some(licenses) = sku_licenses {
        sku_map.insert("licenses".to_string(), json!(licenses));
    }
    body_map.insert("sku".to_string(), Value::Object(sku_map));

    let mut props = serde_json::Map::new();
    if let Some(ref lt) = license_type {
        props.insert("licenseType".to_string(), json!(lt));
    }
    if !props.is_empty() {
        body_map.insert("properties".to_string(), Value::Object(props));
    }

    let body = Value::Object(body_map);
    let url = format!(
        "https://management.azure.com/subscriptions/{sub}/resourceGroups/{rg}/providers/Microsoft.Sql/servers/{server}/databases/{db_name}?api-version=2025-01-01"
    );

    let extra_headers: &[ArmHeader<'_>] = &[];
    let response = arm::arm_request(ctx, Method::PATCH, &url, body, extra_headers).await?;
    let status = response.status();
    let async_url = response
        .headers()
        .get("azure-asyncoperation")
        .and_then(|v| v.to_str().ok())
        .map(std::string::ToString::to_string);

    let response_body: Value = response.json().await.unwrap_or_else(|_| json!({}));

    let provisioning_state = response_body
        .get("properties")
        .and_then(|p| p.get("provisioningState"))
        .and_then(|s| s.as_str())
        .unwrap_or("unknown")
        .to_string();

    let final_state = if let Some(ref async_url) = async_url {
        arm::poll_arm_async_operation(ctx, async_url).await?
    } else {
        provisioning_state
    };

    Ok(json!({
        "stepId": step.id,
        "connector": "azure_sql",
        "action": "sql-database-resize",
        "subscriptionId": sub,
        "resourceGroup": rg,
        "serverName": server,
        "databaseName": db_name,
        "targetSku": {
            "name": sku_name,
            "tier": sku_tier,
            "capacity": sku_capacity,
            "family": sku_family
        },
        "licenseType": license_type,
        "asyncOperationPolled": async_url.is_some(),
        "provisioningState": final_state,
        "responseStatus": status.as_u16(),
        "stateBlob": ctx.upstream_output,
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Returns current SQL Elastic Pool SKU and capacity.
pub(crate) async fn sql_elastic_pool_get(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let sub = subscription_id(step)?;
    let rg = resource_group(step)?;
    let server = step.server_name()?;
    let pool_name = step_config_string(step, "elasticPoolName")
        .or_else(|| input_string(step, "elasticPoolName"))
        .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_sql.elasticPoolName".to_string(),
        })?;

    let url = format!(
        "https://management.azure.com/subscriptions/{sub}/resourceGroups/{rg}/providers/Microsoft.Sql/servers/{server}/elasticpools/{pool_name}?api-version=2025-01-01"
    );

    let extra_headers: &[ArmHeader<'_>] = &[];
    let response = arm::arm_request(ctx, Method::GET, &url, json!({}), extra_headers).await?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or_else(|_| json!({}));

    let sku_name = body
        .get("sku")
        .and_then(|s| s.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("unknown")
        .to_string();

    let sku_tier = body
        .get("sku")
        .and_then(|s| s.get("tier"))
        .and_then(|t| t.as_str())
        .unwrap_or("unknown")
        .to_string();

    let sku_capacity: u32 = body
        .get("sku")
        .and_then(|s| s.get("capacity"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0);

    let provisioning_state = body
        .get("properties")
        .and_then(|p| p.get("provisioningState"))
        .and_then(|s| s.as_str())
        .unwrap_or("unknown")
        .to_string();

    Ok(json!({
        "stepId": step.id,
        "connector": "azure_sql",
        "action": "sql-elastic-pool-get",
        "subscriptionId": sub,
        "resourceGroup": rg,
        "serverName": server,
        "elasticPoolName": pool_name,
        "currentSku": {
            "name": sku_name,
            "tier": sku_tier,
            "capacity": sku_capacity
        },
        "provisioningState": provisioning_state,
        "responseStatus": status.as_u16(),
        "stateBlob": {
            "sku": {
                "name": sku_name,
                "tier": sku_tier,
                "capacity": sku_capacity
            }
        },
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Patches SQL Elastic Pool SKU and capacity.
///
/// ARM returns 200 OK or 202 Accepted with async polling.
pub(crate) async fn sql_elastic_pool_resize(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let sub = subscription_id(step)?;
    let rg = resource_group(step)?;
    let server = step.server_name()?;
    let pool_name = step_config_string(step, "elasticPoolName")
        .or_else(|| input_string(step, "elasticPoolName"))
        .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_sql.elasticPoolName".to_string(),
        })?;

    let target_sku = step_config_object(step, "targetSku").ok_or_else(|| {
        ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_sql.targetSku".to_string(),
        }
    })?;

    let sku_name = target_sku
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_sql.targetSku.name".to_string(),
        })?
        .to_string();

    let sku_tier = target_sku
        .get("tier")
        .and_then(|v| v.as_str())
        .map(String::from);

    let sku_capacity = target_sku
        .get("capacity")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_sql.targetSku.capacity".to_string(),
        })?;

    let sku_family = target_sku
        .get("family")
        .and_then(|v| v.as_str())
        .map(String::from);

    let mut sku_map = serde_json::Map::new();
    sku_map.insert("name".to_string(), json!(sku_name));
    if let Some(ref tier) = sku_tier {
        sku_map.insert("tier".to_string(), json!(tier));
    }
    sku_map.insert("capacity".to_string(), json!(sku_capacity));
    if let Some(ref family) = sku_family {
        sku_map.insert("family".to_string(), json!(family));
    }

    let body = json!({ "sku": Value::Object(sku_map) });
    let url = format!(
        "https://management.azure.com/subscriptions/{sub}/resourceGroups/{rg}/providers/Microsoft.Sql/servers/{server}/elasticpools/{pool_name}?api-version=2025-01-01"
    );

    let extra_headers: &[ArmHeader<'_>] = &[];
    let response = arm::arm_request(ctx, Method::PATCH, &url, body, extra_headers).await?;
    let status = response.status();
    let async_url = response
        .headers()
        .get("azure-asyncoperation")
        .and_then(|v| v.to_str().ok())
        .map(std::string::ToString::to_string);

    let response_body: Value = response.json().await.unwrap_or_else(|_| json!({}));

    let provisioning_state = response_body
        .get("properties")
        .and_then(|p| p.get("provisioningState"))
        .and_then(|s| s.as_str())
        .unwrap_or("unknown")
        .to_string();

    let final_state = if let Some(ref async_url) = async_url {
        arm::poll_arm_async_operation(ctx, async_url).await?
    } else {
        provisioning_state
    };

    Ok(json!({
        "stepId": step.id,
        "connector": "azure_sql",
        "action": "sql-elastic-pool-resize",
        "subscriptionId": sub,
        "resourceGroup": rg,
        "serverName": server,
        "elasticPoolName": pool_name,
        "targetSku": {
            "name": sku_name,
            "tier": sku_tier,
            "capacity": sku_capacity,
            "family": sku_family
        },
        "asyncOperationPolled": async_url.is_some(),
        "provisioningState": final_state,
        "responseStatus": status.as_u16(),
        "stateBlob": ctx.upstream_output,
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Compensate an `azure_sql` mutation by restoring the database or elastic
/// pool to its previous SKU, captured in the step output's `stateBlob` field.
///
/// Same save-then-restore pattern as `azure_compute`: include a
/// `sql-database-get` or `sql-elastic-pool-get` step before any resize.
/// The worker passes the captured state into the resize step's output as
/// `stateBlob`, and compensation reads it to resize back.
///
/// If `stateBlob` is absent, compensation is a no-op.
pub(crate) async fn compensate(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
    original_output: &Value,
) -> Result<Value, ExecutionLedgerError> {
    let state_blob = original_output.get("stateBlob");
    let action = original_output
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("");

    match action {
        "sql-database-get" | "sql-elastic-pool-get" => {
            return Ok(json!({
                "stepId": step.id,
                "connector": "azure_sql",
                "compensated": true,
                "action": "noop",
                "reason": "read-only action"
            }));
        }
        "sql-database-resize" => {
            let Some(blob) = state_blob else {
                return Ok(json!({
                    "stepId": step.id,
                    "connector": "azure_sql",
                    "compensated": true,
                    "action": "noop",
                    "reason": "no stateBlob to restore from"
                }));
            };
            let sku = blob.get("sku").unwrap_or(blob);
            let restore_step = ConnectorStep {
                id: format!("{}.compensate", step.id),
                connector: ConnectorKind::AzureSql,
                action: Some("sql-database-resize".to_string()),
                external_id: step.external_id.clone(),
                input: json!({"targetSku": sku}),
                config: step.config.clone(),
                input_schema: None,
                output_schema: None,
            };
            sql_database_resize(ctx, &restore_step).await?;
        }
        "sql-elastic-pool-resize" => {
            let Some(blob) = state_blob else {
                return Ok(json!({
                    "stepId": step.id,
                    "connector": "azure_sql",
                    "compensated": true,
                    "action": "noop",
                    "reason": "no stateBlob to restore from"
                }));
            };
            let sku = blob.get("sku").unwrap_or(blob);
            let restore_step = ConnectorStep {
                id: format!("{}.compensate", step.id),
                connector: ConnectorKind::AzureSql,
                action: Some("sql-elastic-pool-resize".to_string()),
                external_id: step.external_id.clone(),
                input: json!({"targetSku": sku}),
                config: step.config.clone(),
                input_schema: None,
                output_schema: None,
            };
            sql_elastic_pool_resize(ctx, &restore_step).await?;
        }
        _ => {
            return Ok(json!({
                "stepId": step.id,
                "connector": "azure_sql",
                "compensated": true,
                "action": "noop",
                "reason": "unknown action, cannot determine restore path"
            }));
        }
    }

    Ok(json!({
        "stepId": step.id,
        "connector": "azure_sql",
        "compensated": true,
        "action": "restore",
        "restoredFromStateBlob": state_blob
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn test_ctx() -> ConnectorExecutionContext<'static> {
        ConnectorExecutionContext {
            run_id: Uuid::new_v4(),
            correlation_id: "test-correlation",
            job_id: "test-job",
            event: "test",
            environment: "test",
            upstream_output: None,
        }
    }

    fn test_step(action: &str, config: Value) -> ConnectorStep {
        ConnectorStep {
            id: "sql-step".to_string(),
            connector: ConnectorKind::AzureSql,
            action: Some(action.to_string()),
            external_id: None,
            input: json!({}),
            config,
            input_schema: None,
            output_schema: None,
        }
    }

    /// Runs a connector function that fails before any network call and
    /// returns the error message (empty string if it unexpectedly succeeded).
    async fn error_message<F, Fut>(f: F) -> String
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Value, ExecutionLedgerError>>,
    {
        f().await.map_or_else(|e| e.to_string(), |_| String::new())
    }

    // --- Validation: these fail before any network/MSI call is attempted,
    // so they run deterministically with no external dependencies. ---

    #[tokio::test]
    async fn sql_database_get_missing_server_name_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            "sql-database-get",
            json!({"subscriptionId": "sub-fake", "resourceGroup": "rg-fake", "databaseName": "db1"}),
        );
        let message = error_message(|| sql_database_get(&ctx, &step)).await;
        assert!(message.contains("connector.azure_sql.serverName"));
    }

    #[tokio::test]
    async fn sql_database_get_missing_database_name_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            "sql-database-get",
            json!({"subscriptionId": "sub-fake", "resourceGroup": "rg-fake", "serverName": "srv"}),
        );
        let message = error_message(|| sql_database_get(&ctx, &step)).await;
        assert!(message.contains("connector.azure_sql.databaseName"));
    }

    #[tokio::test]
    async fn sql_elastic_pool_get_missing_pool_name_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            "sql-elastic-pool-get",
            json!({"subscriptionId": "sub-fake", "resourceGroup": "rg-fake", "serverName": "srv"}),
        );
        let message = error_message(|| sql_elastic_pool_get(&ctx, &step)).await;
        assert!(message.contains("connector.azure_sql.elasticPoolName"));
    }

    #[tokio::test]
    async fn sql_database_resize_missing_target_sku_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            "sql-database-resize",
            json!({
                "subscriptionId": "sub-fake",
                "resourceGroup": "rg-fake",
                "serverName": "srv",
                "databaseName": "db1"
            }),
        );
        let message = error_message(|| sql_database_resize(&ctx, &step)).await;
        assert!(message.contains("connector.azure_sql.targetSku"));
    }

    #[tokio::test]
    async fn sql_database_resize_missing_sku_name_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            "sql-database-resize",
            json!({
                "subscriptionId": "sub-fake",
                "resourceGroup": "rg-fake",
                "serverName": "srv",
                "databaseName": "db1",
                "targetSku": {"tier": "Standard"}
            }),
        );
        let message = error_message(|| sql_database_resize(&ctx, &step)).await;
        assert!(message.contains("connector.azure_sql.targetSku.name"));
    }

    #[tokio::test]
    async fn sql_elastic_pool_resize_missing_target_sku_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            "sql-elastic-pool-resize",
            json!({
                "subscriptionId": "sub-fake",
                "resourceGroup": "rg-fake",
                "serverName": "srv",
                "elasticPoolName": "pool1"
            }),
        );
        let message = error_message(|| sql_elastic_pool_resize(&ctx, &step)).await;
        assert!(message.contains("connector.azure_sql.targetSku"));
    }

    /// Unlike `sql_database_resize` (capacity is optional — DTU vs. vCore
    /// models represent capacity differently and Azure may omit it),
    /// `sql_elastic_pool_resize` requires `targetSku.capacity` explicitly.
    /// This test pins that asymmetry down so it isn't "fixed" into a bug.
    #[tokio::test]
    async fn sql_elastic_pool_resize_missing_capacity_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            "sql-elastic-pool-resize",
            json!({
                "subscriptionId": "sub-fake",
                "resourceGroup": "rg-fake",
                "serverName": "srv",
                "elasticPoolName": "pool1",
                "targetSku": {"name": "GP_Gen5"}
            }),
        );
        let message = error_message(|| sql_elastic_pool_resize(&ctx, &step)).await;
        assert!(message.contains("connector.azure_sql.targetSku.capacity"));
    }

    // --- Compensate no-op branches: these return before any network call
    // is attempted, so they are pure logic and fully deterministic. Each
    // result is compared against the exact expected JSON shape rather than
    // unwrapped, so an unexpected Err shows up as a normal assert_eq! diff
    // instead of a panic. ---

    fn noop_shape(reason: &str) -> Value {
        json!({
            "stepId": "sql-step",
            "connector": "azure_sql",
            "compensated": true,
            "action": "noop",
            "reason": reason
        })
    }

    #[tokio::test]
    async fn compensate_read_only_actions_are_noop() {
        let ctx = test_ctx();
        let step = test_step("noop-check", json!({}));
        for action in ["sql-database-get", "sql-elastic-pool-get"] {
            let original_output = json!({
                "action": action,
                "stateBlob": {"sku": {"name": "S1"}}
            });
            let result = compensate(&ctx, &step, &original_output).await;
            let value = result.unwrap_or_else(|e| json!({"compensateError": e.to_string()}));
            assert_eq!(
                value,
                noop_shape("read-only action"),
                "action {action} did not compensate as expected"
            );
        }
    }

    #[tokio::test]
    async fn compensate_sql_database_resize_without_state_blob_is_noop() {
        let ctx = test_ctx();
        let step = test_step("noop-check", json!({}));
        let original_output = json!({"action": "sql-database-resize"});
        let result = compensate(&ctx, &step, &original_output).await;
        let value = result.unwrap_or_else(|e| json!({"compensateError": e.to_string()}));
        assert_eq!(value, noop_shape("no stateBlob to restore from"));
    }

    #[tokio::test]
    async fn compensate_sql_elastic_pool_resize_without_state_blob_is_noop() {
        let ctx = test_ctx();
        let step = test_step("noop-check", json!({}));
        let original_output = json!({"action": "sql-elastic-pool-resize"});
        let result = compensate(&ctx, &step, &original_output).await;
        let value = result.unwrap_or_else(|e| json!({"compensateError": e.to_string()}));
        assert_eq!(value, noop_shape("no stateBlob to restore from"));
    }

    #[tokio::test]
    async fn compensate_unknown_action_is_noop() {
        let ctx = test_ctx();
        let step = test_step("noop-check", json!({}));
        let original_output = json!({"action": "some-future-action"});
        let result = compensate(&ctx, &step, &original_output).await;
        let value = result.unwrap_or_else(|e| json!({"compensateError": e.to_string()}));
        assert_eq!(
            value,
            noop_shape("unknown action, cannot determine restore path")
        );
    }

    #[tokio::test]
    async fn compensate_missing_action_field_is_noop() {
        let ctx = test_ctx();
        let step = test_step("noop-check", json!({}));
        let original_output = json!({});
        let result = compensate(&ctx, &step, &original_output).await;
        let value = result.unwrap_or_else(|e| json!({"compensateError": e.to_string()}));
        assert_eq!(
            value,
            noop_shape("unknown action, cannot determine restore path")
        );
    }
}
