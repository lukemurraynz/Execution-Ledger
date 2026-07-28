#Requires -PSEdition Core

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Continue'

$appName = "execution-ledger-$env:AZURE_ENV_NAME"
$groupName = "exec-ledger-admins-$env:AZURE_ENV_NAME"

# ── Clean up Entra ID resources before resource group deletion ──────────────
# Runs before azd down. Deletes the app registration and admin group
# so no orphaned tenant-level resources remain after the RG is gone.

# 1. Delete app registration
$app = az ad app list --display-name $appName --query "[0].appId" --output tsv 2>$null
if ($app -and $app -ne 'null') {
    Write-Host "Deleting app registration: $appName ($app)"
    az ad app delete --id $app 2>$null
    if ($LASTEXITCODE -eq 0) {
        Write-Host "  Deleted."
    } else {
        Write-Warning "  Failed to delete app registration. Remove manually: az ad app delete --id $app"
    }
} else {
    Write-Host "No app registration found for '$appName' — skipped."
}

# 2. Delete admin group
$group = az ad group list --display-name $groupName --query "[0].id" --output tsv 2>$null
if ($group -and $group -ne 'null') {
    Write-Host "Deleting admin group: $groupName ($group)"
    az ad group delete --group $groupName 2>$null
    if ($LASTEXITCODE -eq 0) {
        Write-Host "  Deleted."
    } else {
        Write-Warning "  Failed to delete admin group. Remove manually: az ad group delete --group $groupName"
    }
} else {
    Write-Host "No admin group found for '$groupName' — skipped."
}
