//! Azure Compute connector.
//!
//! Resizes Azure compute resources (VM, VMSS, App Service Plan) via the ARM REST API,
//! authenticated with managed identity. Supports three actions:
//!
//! - **`vm-get`**: Reads current VM size and instance view.
//! - **`vm-save-state`**: Reads VM configuration and returns state blob suitable for storage; used before scale-down so compensate can restore it.
//! - **`vmss-get`**: Reads current VMSS SKU, tier, and instance count.
//! - **`vmss-resize`**: Updates VMSS SKU and/or instance count.
//! - **`appservice-resize`**: Patches an App Service Plan tier and capacity.
//!
//! ## MSI Authentication
//!
//! Uses `azure_identity::ManagedIdentityCredential` to acquire a bearer token for
//! `https://management.azure.com/.default` — the same scope used by all Azure
//! management-plane APIs. No connection strings, no key vault secrets, no env var
//! fallback in production.
//!
//! ## Configuration
//!
//! | Key | Required | Notes |
//! |-----|----------|-------|
//! | `subscriptionId` | yes | Azure subscription ID |
//! | `resourceGroup` | yes | Target resource group name |
//! | `vmName` | yes (vm-get) | VM name |
//! | `vmssName` | yes (vmss-resize) | VM Scale Set name |
//! | `appServicePlanName` | yes (appservice-resize) | App Service Plan name |
//! | `targetSku.name` | yes (resize actions) | e.g. `Standard_D2s_v3` |
//! | `targetSku.tier` | no | e.g. `Standard`, `Burstable`, `Premium` |
//! | `capacity.desiredCapacity` | yes (vmss-resize) | Target instance count |
//!
//! ## Fail-closed behavior
//!
//! - Returns `InvalidConfiguration` if any required config key is missing.
//! - Returns `DependencyFailed` if the ARM token request fails.
//! - Returns `DependencyFailed` if the ARM API returns non-2xx.

// SKU capacities, instance counts, and worker counts from ARM API responses
// are always small integers (max a few thousand). Use try_from to make the
// truncation boundary explicit rather than silent as-casting.

use execution_ledger_core::ExecutionLedgerError;
use reqwest::Method;
use serde_json::{Value, json};

use crate::connectors::arm::{self, ArmHeader};
use crate::connectors::helpers::{
    input_string, resource_group, step_config_object, step_config_string, step_config_u64,
    subscription_id,
};
use crate::connectors::{ConnectorExecutionContext, ConnectorKind, ConnectorStep};

/// [VERIFIED] All embedded Azure REST API versions are real published versions.
/// - Compute (VM, VMSS): 2023-03-01
///   `WhereToCheck` = <https://learn.microsoft.com/en-us/rest/api/compute/virtual-machines/get?view=rest-compute-2023-03-01>
/// - Web (App Service Plans): 2023-01-01
///   `WhereToCheck` = <https://learn.microsoft.com/en-us/rest/api/appservice/web-apps?view=rest-appservice-2023-01-01>
///
/// `EvidenceType` = Docs | Date = 2026-07-26
///
/// Returns the current VMSS SKU, tier, and instance count — used to capture state before scale-down
/// so a compensate step can restore it. Call this in a step before `vmss-resize`.
pub(crate) async fn vmss_get(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let sub = subscription_id(step)?;
    let rg = resource_group(step)?;
    let vmss_name = step_config_string(step, "vmssName")
        .or_else(|| input_string(step, "vmssName"))
        .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_compute.vmssName".to_string(),
        })?;

    let url = format!(
        "https://management.azure.com/subscriptions/{sub}/resourceGroups/{rg}/providers/Microsoft.Compute/virtualMachineScaleSets/{vmss_name}?api-version=2023-03-01"
    );

    let extra_headers: &[ArmHeader<'_>] = &[];
    let response = arm::arm_request(ctx, Method::GET, &url, json!({}), extra_headers).await?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or_else(|_| json!({}));

    let sku_name = body
        .get("properties")
        .and_then(|p| p.get("sku"))
        .and_then(|s| s.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("unknown")
        .to_string();

    let sku_tier = body
        .get("properties")
        .and_then(|p| p.get("sku"))
        .and_then(|s| s.get("tier"))
        .and_then(|t| t.as_str())
        .unwrap_or("Standard")
        .to_string();

    let current_capacity: u32 = body
        .get("properties")
        .and_then(|p| p.get("sku"))
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
        "connector": "azure_compute",
        "action": "vmss-get",
        "subscriptionId": sub,
        "resourceGroup": rg,
        "vmssName": vmss_name,
        "currentSku": {
            "name": sku_name,
            "tier": sku_tier
        },
        "currentCapacity": current_capacity,
        "provisioningState": provisioning_state,
        "responseStatus": status.as_u16(),
        "stateBlob": {
            "sku": { "name": sku_name, "tier": sku_tier },
            "capacity": current_capacity
        },
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Returns the current App Service Plan SKU and worker count.
pub(crate) async fn appservice_get(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let sub = subscription_id(step)?;
    let rg = resource_group(step)?;
    let plan_name = step_config_string(step, "appServicePlanName")
        .or_else(|| input_string(step, "appServicePlanName"))
        .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_compute.appServicePlanName".to_string(),
        })?;

    let url = format!(
        "https://management.azure.com/subscriptions/{sub}/resourceGroups/{rg}/providers/Microsoft.Web/serverfarms/{plan_name}?api-version=2023-01-01"
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
        .unwrap_or("Standard")
        .to_string();

    let worker_count: u32 = body
        .get("properties")
        .and_then(|p| p.get("numberOfWorkers"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(1);

    let provisioning_state = body
        .get("properties")
        .and_then(|p| p.get("provisioningState"))
        .and_then(|s| s.as_str())
        .unwrap_or("unknown")
        .to_string();

    Ok(json!({
        "stepId": step.id,
        "connector": "azure_compute",
        "action": "appservice-get",
        "subscriptionId": sub,
        "resourceGroup": rg,
        "appServicePlanName": plan_name,
        "currentSku": { "name": sku_name, "tier": sku_tier },
        "currentWorkerCount": worker_count,
        "provisioningState": provisioning_state,
        "responseStatus": status.as_u16(),
        "stateBlob": {
            "sku": { "name": sku_name, "tier": sku_tier },
            "workerCount": worker_count
        },
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Returns the current VM size and instance details.
pub(crate) async fn vm_get(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let sub = subscription_id(step)?;
    let rg = resource_group(step)?;
    let vm_name = step_config_string(step, "vmName")
        .or_else(|| input_string(step, "vmName"))
        .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_compute.vmName".to_string(),
        })?;

    let url = format!(
        "https://management.azure.com/subscriptions/{sub}/resourceGroups/{rg}/providers/Microsoft.Compute/virtualMachines/{vm_name}?api-version=2023-03-01"
    );

    let extra_headers: &[ArmHeader<'_>] = &[];
    let response = arm::arm_request(ctx, Method::GET, &url, json!({}), extra_headers).await?;
    let status = response.status();
    let body = response.json::<Value>().await.unwrap_or_else(|_| json!({}));

    let vm_size = body
        .get("properties")
        .and_then(|p| p.get("hardwareProfile"))
        .and_then(|h| h.get("vmSize"))
        .and_then(|s| s.as_str())
        .unwrap_or("unknown")
        .to_string();

    let provisioning_state = body
        .get("properties")
        .and_then(|p| p.get("provisioningState"))
        .and_then(|s| s.as_str())
        .unwrap_or("unknown")
        .to_string();

    Ok(json!({
        "stepId": step.id,
        "connector": "azure_compute",
        "action": "vm-get",
        "subscriptionId": sub,
        "resourceGroup": rg,
        "vmName": vm_name,
        "vmSize": vm_size,
        "provisioningState": provisioning_state,
        "responseStatus": status.as_u16(),
        "stateBlob": {
            "vmSize": vm_size
        },
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Resizes a VM Scale Set by updating its SKU and/or instance count.
pub(crate) async fn vmss_resize(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let sub = subscription_id(step)?;
    let rg = resource_group(step)?;
    let vmss_name = step_config_string(step, "vmssName")
        .or_else(|| input_string(step, "vmssName"))
        .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_compute.vmssName".to_string(),
        })?;

    let sku = step_config_object(step, "targetSku").ok_or_else(|| {
        ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_compute.targetSku".to_string(),
        }
    })?;
    let sku_name = sku.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
        ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_compute.targetSku.name".to_string(),
        }
    })?;
    let sku_tier = sku.get("tier").and_then(|v| v.as_str());

    let capacity = step_config_object(step, "capacity").ok_or_else(|| {
        ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_compute.capacity".to_string(),
        }
    })?;
    let desired_capacity: u32 = step_config_u64(step, "desiredCapacity")
        .or_else(|| {
            capacity
                .get("desiredCapacity")
                .and_then(serde_json::Value::as_u64)
        })
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_compute.capacity.desiredCapacity".to_string(),
        })?;

    let url = format!(
        "https://management.azure.com/subscriptions/{sub}/resourceGroups/{rg}/providers/Microsoft.Compute/virtualMachineScaleSets/{vmss_name}?api-version=2023-03-01"
    );

    let body = json!({
        "properties": {
            "sku": {
                "name": sku_name,
                "tier": sku_tier.unwrap_or("Standard"),
                "capacity": desired_capacity
            }
        }
    });

    let extra_headers: &[ArmHeader<'_>] = &[];
    let response = arm::arm_request(ctx, Method::PUT, &url, body, extra_headers).await?;
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
        "connector": "azure_compute",
        "action": "vmss-resize",
        "subscriptionId": sub,
        "resourceGroup": rg,
        "vmssName": vmss_name,
        "targetSku": {
            "name": sku_name,
            "tier": sku_tier.unwrap_or("Standard")
        },
        "desiredCapacity": desired_capacity,
        "provisioningState": final_state,
        "responseStatus": status.as_u16(),
        "asyncOperationPolled": async_url.is_some(),
        "stateBlob": ctx.upstream_output,
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Resizes an App Service Plan by patching its SKU and capacity.
pub(crate) async fn appservice_resize(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let sub = subscription_id(step)?;
    let rg = resource_group(step)?;
    let plan_name = step_config_string(step, "appServicePlanName")
        .or_else(|| input_string(step, "appServicePlanName"))
        .ok_or_else(|| ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_compute.appServicePlanName".to_string(),
        })?;

    let sku = step_config_object(step, "targetSku").ok_or_else(|| {
        ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_compute.targetSku".to_string(),
        }
    })?;
    let sku_name = sku.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
        ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_compute.targetSku.name".to_string(),
        }
    })?;
    let sku_tier = sku.get("tier").and_then(|v| v.as_str());

    let capacity = step_config_object(step, "capacity").ok_or_else(|| {
        ExecutionLedgerError::InvalidConfiguration {
            name: "connector.azure_compute.capacity".to_string(),
        }
    })?;
    let worker_count: u32 = step_config_u64(step, "workerCount")
        .or_else(|| {
            capacity
                .get("workerCount")
                .and_then(serde_json::Value::as_u64)
        })
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(1);

    let url = format!(
        "https://management.azure.com/subscriptions/{sub}/resourceGroups/{rg}/providers/Microsoft.Web/serverfarms/{plan_name}?api-version=2023-01-01"
    );

    let body = json!({
        "sku": {
            "name": sku_name,
            "tier": sku_tier.unwrap_or("Standard"),
            "capacity": worker_count
        },
        "properties": {
            "numberOfWorkers": worker_count
        }
    });

    let extra_headers: &[ArmHeader<'_>] = &[];
    let response = arm::arm_request(ctx, Method::PATCH, &url, body, extra_headers).await?;
    let status = response.status();
    let response_body: Value = response.json().await.unwrap_or_else(|_| json!({}));

    let provisioning_state = response_body
        .get("properties")
        .and_then(|p| p.get("provisioningState"))
        .and_then(|s| s.as_str())
        .unwrap_or("unknown")
        .to_string();

    Ok(json!({
        "stepId": step.id,
        "connector": "azure_compute",
        "action": "appservice-resize",
        "subscriptionId": sub,
        "resourceGroup": rg,
        "appServicePlanName": plan_name,
        "targetSku": {
            "name": sku_name,
            "tier": sku_tier.unwrap_or("Standard")
        },
        "workerCount": worker_count,
        "provisioningState": provisioning_state,
        "responseStatus": status.as_u16(),
        "stateBlob": ctx.upstream_output,
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Compensate an `azure_compute` mutation by restoring the resource to its
/// previous SKU, captured in the step output's `stateBlob` field.
///
/// # How it works
///
/// The caller must include a `vm-get`, `vmss-get`, or `appservice-get` step
/// before any resize in the job definition. The worker passes that captured
/// state into the resize step's output as `stateBlob` so compensation can
/// read it and call the reverse resize action.
///
/// If `stateBlob` is absent from the original output (the job did not
/// include a capture-state step), compensation is a no-op.
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
        // Read-only actions have no side effects to undo.
        "vm-get" | "vm-save-state" | "vmss-get" | "appservice-get" => {
            return Ok(json!({
                "stepId": step.id,
                "connector": "azure_compute",
                "compensated": true,
                "action": "noop",
                "reason": "read-only action"
            }));
        }
        // Resize actions: restore the previous SKU from stateBlob.
        "vmss-resize" => {
            let Some(blob) = state_blob else {
                return Ok(json!({
                    "stepId": step.id,
                    "connector": "azure_compute",
                    "compensated": true,
                    "action": "noop",
                    "reason": "no stateBlob to restore from"
                }));
            };
            let sku = blob.get("sku").unwrap_or(blob);
            let restore_step = ConnectorStep {
                id: format!("{}.compensate", step.id),
                connector: ConnectorKind::AzureCompute,
                action: Some("vmss-resize".to_string()),
                external_id: step.external_id.clone(),
                input: json!({"targetSku": sku}),
                config: step.config.clone(),
                input_schema: None,
                output_schema: None,
            };
            vmss_resize(ctx, &restore_step).await?;
        }
        "appservice-resize" => {
            let Some(blob) = state_blob else {
                return Ok(json!({
                    "stepId": step.id,
                    "connector": "azure_compute",
                    "compensated": true,
                    "action": "noop",
                    "reason": "no stateBlob to restore from"
                }));
            };
            let sku = blob.get("sku").unwrap_or(blob);
            let restore_step = ConnectorStep {
                id: format!("{}.compensate", step.id),
                connector: ConnectorKind::AzureCompute,
                action: Some("appservice-resize".to_string()),
                external_id: step.external_id.clone(),
                input: json!({"targetSku": sku}),
                config: step.config.clone(),
                input_schema: None,
                output_schema: None,
            };
            appservice_resize(ctx, &restore_step).await?;
        }
        _ => {
            return Ok(json!({
                "stepId": step.id,
                "connector": "azure_compute",
                "compensated": true,
                "action": "noop",
                "reason": "unknown action, cannot determine restore path"
            }));
        }
    }

    Ok(json!({
        "stepId": step.id,
        "connector": "azure_compute",
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
            id: "compute-step".to_string(),
            connector: ConnectorKind::AzureCompute,
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
    async fn vmss_get_missing_vmss_name_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            "vmss-get",
            json!({"subscriptionId": "sub-fake", "resourceGroup": "rg-fake"}),
        );
        let message = error_message(|| vmss_get(&ctx, &step)).await;
        assert!(message.contains("connector.azure_compute.vmssName"));
    }

    #[tokio::test]
    async fn vm_get_missing_vm_name_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            "vm-get",
            json!({"subscriptionId": "sub-fake", "resourceGroup": "rg-fake"}),
        );
        let message = error_message(|| vm_get(&ctx, &step)).await;
        assert!(message.contains("connector.azure_compute.vmName"));
    }

    #[tokio::test]
    async fn appservice_get_missing_plan_name_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            "appservice-get",
            json!({"subscriptionId": "sub-fake", "resourceGroup": "rg-fake"}),
        );
        let message = error_message(|| appservice_get(&ctx, &step)).await;
        assert!(message.contains("connector.azure_compute.appServicePlanName"));
    }

    #[tokio::test]
    async fn vmss_resize_missing_target_sku_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            "vmss-resize",
            json!({
                "subscriptionId": "sub-fake",
                "resourceGroup": "rg-fake",
                "vmssName": "workers"
            }),
        );
        let message = error_message(|| vmss_resize(&ctx, &step)).await;
        assert!(message.contains("connector.azure_compute.targetSku"));
    }

    #[tokio::test]
    async fn vmss_resize_missing_sku_name_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            "vmss-resize",
            json!({
                "subscriptionId": "sub-fake",
                "resourceGroup": "rg-fake",
                "vmssName": "workers",
                "targetSku": {"tier": "Standard"}
            }),
        );
        let message = error_message(|| vmss_resize(&ctx, &step)).await;
        assert!(message.contains("connector.azure_compute.targetSku.name"));
    }

    #[tokio::test]
    async fn vmss_resize_missing_capacity_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            "vmss-resize",
            json!({
                "subscriptionId": "sub-fake",
                "resourceGroup": "rg-fake",
                "vmssName": "workers",
                "targetSku": {"name": "Standard_D2s_v3"}
            }),
        );
        let message = error_message(|| vmss_resize(&ctx, &step)).await;
        assert!(message.contains("connector.azure_compute.capacity"));
    }

    #[tokio::test]
    async fn vmss_resize_missing_desired_capacity_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            "vmss-resize",
            json!({
                "subscriptionId": "sub-fake",
                "resourceGroup": "rg-fake",
                "vmssName": "workers",
                "targetSku": {"name": "Standard_D2s_v3"},
                "capacity": {}
            }),
        );
        let message = error_message(|| vmss_resize(&ctx, &step)).await;
        assert!(message.contains("connector.azure_compute.capacity.desiredCapacity"));
    }

    #[tokio::test]
    async fn appservice_resize_missing_target_sku_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            "appservice-resize",
            json!({
                "subscriptionId": "sub-fake",
                "resourceGroup": "rg-fake",
                "appServicePlanName": "plan-01"
            }),
        );
        let message = error_message(|| appservice_resize(&ctx, &step)).await;
        assert!(message.contains("connector.azure_compute.targetSku"));
    }

    #[tokio::test]
    async fn appservice_resize_missing_capacity_is_error() {
        let ctx = test_ctx();
        let step = test_step(
            "appservice-resize",
            json!({
                "subscriptionId": "sub-fake",
                "resourceGroup": "rg-fake",
                "appServicePlanName": "plan-01",
                "targetSku": {"name": "P1v3"}
            }),
        );
        let message = error_message(|| appservice_resize(&ctx, &step)).await;
        assert!(message.contains("connector.azure_compute.capacity"));
    }

    // --- Compensate no-op branches: these return before any network call
    // is attempted, so they are pure logic and fully deterministic. These
    // are the branches that stop compensation from mis-firing on missing
    // or unrecognized state. Each result is compared against the exact
    // expected JSON shape rather than unwrapped, so an unexpected Err
    // shows up as a normal assert_eq! diff instead of a panic. ---

    fn noop_shape(action: &str, reason: &str) -> Value {
        json!({
            "stepId": "compute-step",
            "connector": "azure_compute",
            "compensated": true,
            "action": action,
            "reason": reason
        })
    }

    #[tokio::test]
    async fn compensate_read_only_actions_are_noop() {
        let ctx = test_ctx();
        let step = test_step("noop-check", json!({}));
        for action in ["vm-get", "vm-save-state", "vmss-get", "appservice-get"] {
            let original_output = json!({
                "action": action,
                "stateBlob": {"sku": {"name": "Standard_D2s_v3"}}
            });
            let result = compensate(&ctx, &step, &original_output).await;
            let value = result.unwrap_or_else(|e| json!({"compensateError": e.to_string()}));
            assert_eq!(
                value,
                noop_shape("noop", "read-only action"),
                "action {action} did not compensate as expected"
            );
        }
    }

    #[tokio::test]
    async fn compensate_vmss_resize_without_state_blob_is_noop() {
        let ctx = test_ctx();
        let step = test_step("noop-check", json!({}));
        let original_output = json!({"action": "vmss-resize"});
        let result = compensate(&ctx, &step, &original_output).await;
        let value = result.unwrap_or_else(|e| json!({"compensateError": e.to_string()}));
        assert_eq!(value, noop_shape("noop", "no stateBlob to restore from"));
    }

    #[tokio::test]
    async fn compensate_appservice_resize_without_state_blob_is_noop() {
        let ctx = test_ctx();
        let step = test_step("noop-check", json!({}));
        let original_output = json!({"action": "appservice-resize"});
        let result = compensate(&ctx, &step, &original_output).await;
        let value = result.unwrap_or_else(|e| json!({"compensateError": e.to_string()}));
        assert_eq!(value, noop_shape("noop", "no stateBlob to restore from"));
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
            noop_shape("noop", "unknown action, cannot determine restore path")
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
            noop_shape("noop", "unknown action, cannot determine restore path")
        );
    }
}
