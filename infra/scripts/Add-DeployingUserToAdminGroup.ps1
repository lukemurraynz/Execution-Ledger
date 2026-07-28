#Requires -PSEdition Core

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$appName = "execution-ledger-$env:AZURE_ENV_NAME"
$groupName = "exec-ledger-admins-$env:AZURE_ENV_NAME"
$resourceGroup = $env:AZURE_RESOURCE_GROUP

# Runs after azd provision using the signed-in identity. This avoids Graph app
# permissions on ARM deployment identities while keeping azd up fully automated.

$null = az account show --output none 2>$null
if ($LASTEXITCODE -ne 0) {
    Write-Warning "Azure CLI is not logged in for postprovision identity setup; skipping Entra app/group bootstrap."
    Write-Warning "Run this manually after login: pwsh .\\infra\\scripts\\Add-DeployingUserToAdminGroup.ps1"
    exit 0
}

$me = az ad signed-in-user show --query id --output tsv 2>$null
if (-not $me -or $me -eq 'null') {
    Write-Warning "Could not determine signed-in user object ID; skipping Entra app/group bootstrap."
    exit 0
}

$appAudience = azd env get-value EXECUTION_LEDGER_ENTRA_AUDIENCE 2>$null
if (-not $appAudience) {
    $deploymentName = az deployment group list -g $resourceGroup --query "[?contains(name, '$env:AZURE_ENV_NAME')]|sort_by(@,&properties.timestamp)[-1].name" --output tsv
    if ($deploymentName) {
        $appAudience = az deployment group show -g $resourceGroup -n $deploymentName --query "properties.outputs.EXECUTION_LEDGER_ENTRA_AUDIENCE.value" --output tsv
    }
}
if (-not $appAudience -or $appAudience -eq 'null') {
    throw "Unable to resolve EXECUTION_LEDGER_ENTRA_AUDIENCE from azd env or deployment outputs."
}

$appId = az ad app list --filter "identifierUris/any(x:x eq '$appAudience')" --query "[0].appId" --output tsv 2>$null
if (-not $appId -or $appId -eq 'null') {
    $appId = az ad app create --display-name $appName --identifier-uris $appAudience --sign-in-audience AzureADMyOrg --query appId --output tsv 2>$null
    if (-not $appId -or $appId -eq 'null') {
        throw "Failed to create Entra app registration for audience '$appAudience'."
    }
    Write-Host "Created app registration '$appName' ($appId)."
}

az ad sp create --id $appId --only-show-errors 2>$null | Out-Null
$spObjectId = az ad sp show --id $appId --query id --output tsv
$appObjectId = az ad app show --id $appId --query id --output tsv
if (-not $spObjectId -or -not $appObjectId -or $spObjectId -eq 'null' -or $appObjectId -eq 'null') {
    throw "Failed to resolve service principal or application object for appId '$appId'."
}

$scope = {
    param(
        [string]$value,
        [string]$display,
        [string]$description,
        [string]$type
    )
    $id = [guid]::NewGuid().Guid
    if ($type -eq 'User') {
        return @{
            id = $id
            value = $value
            type = $type
            isEnabled = $true
            adminConsentDisplayName = $display
            adminConsentDescription = $description
            userConsentDisplayName = $display
            userConsentDescription = $description
        }
    }
    return @{
        id = $id
        value = $value
        type = $type
        isEnabled = $true
        adminConsentDisplayName = $display
        adminConsentDescription = $description
    }
}

$payload = @{
    api = @{
        requestedAccessTokenVersion = 2
        oauth2PermissionScopes = @(
            (& $scope 'jobs:read:own' 'Read own jobs' 'Read own tenant job runs' 'User')
            (& $scope 'jobs:read:any' 'Read any jobs' 'Read any tenant job runs' 'Admin')
            (& $scope 'jobs:write:own' 'Write own jobs' 'Write own tenant jobs' 'User')
            (& $scope 'platform:read' 'Platform read' 'Read platform health, definitions, and schedules' 'Admin')
            (& $scope 'platform:write' 'Platform write' 'Write platform config, trigger jobs, manage schedules' 'Admin')
            (& $scope 'mcp:execute' 'MCP execute' 'Execute MCP tools' 'Admin')
        )
    }
}

$payloadJson = $payload | ConvertTo-Json -Depth 10 -Compress
az rest `
    --method PATCH `
    --uri "https://graph.microsoft.com/v1.0/applications/$appObjectId" `
    --headers "Content-Type=application/json" `
    --body $payloadJson `
    --only-show-errors | Out-Null

az ad app permission admin-consent --id $appId --only-show-errors 2>$null | Out-Null

$group = az ad group list --display-name $groupName --query "[0].id" --output tsv 2>$null
if (-not $group -or $group -eq 'null') {
    $group = az ad group create `
        --display-name $groupName `
        --mail-nickname $groupName `
        --description "Execution Ledger administrators" `
        --query id `
        --output tsv
    Write-Host "Created admin group '$groupName' ($group)."
}

$existingAssignment = az rest `
    --method GET `
    --uri "https://graph.microsoft.com/v1.0/servicePrincipals/$spObjectId/appRoleAssignedTo" `
    --query "value[?principalId=='$group'].id" `
    --output tsv 2>$null
if (-not $existingAssignment -or $existingAssignment -eq 'null') {
    az rest `
        --method POST `
        --uri "https://graph.microsoft.com/v1.0/servicePrincipals/$spObjectId/appRoleAssignedTo" `
        --body "{`"principalId`":`"$group`",`"resourceId`":`"$spObjectId`",`"appRoleId`":`"00000000-0000-0000-0000-000000000000`"}" `
        --only-show-errors 2>$null | Out-Null
}

$existingMember = az ad group member check --group $groupName --member-id $me --query value --output tsv 2>$null
if ($existingMember -ne 'true') {
    az ad group member add --group $groupName --member-id $me --only-show-errors 2>$null
    Write-Host "Added current user to '$groupName'."
}

# Entra v2 access tokens always emit aud = clientId GUID, not the identifierUri, regardless
# of how the resource was specified at token-request time. Store the GUID as the canonical
# audience so that APIM validate-jwt and the API's own JWT validator both match tokens.
$currentAudience = azd env get-value EXECUTION_LEDGER_ENTRA_AUDIENCE 2>$null
if ($currentAudience -ne $appId) {
    azd env set EXECUTION_LEDGER_ENTRA_AUDIENCE $appId | Out-Null
    Write-Host "Updated EXECUTION_LEDGER_ENTRA_AUDIENCE to app client ID: $appId"
}

# Bicep provisions APIM named value and Container App env vars from the azd env value, which
# may be absent or URI-form on first run. Sync them to the GUID now so this provision is
# immediately functional without requiring a second azd provision.
$sub = az account show --query id --output tsv
$apimName = az apim list -g $resourceGroup --query "[0].name" --output tsv 2>$null
if ($apimName) {
    $mgmtToken = az account get-access-token --resource https://management.azure.com --query accessToken --output tsv
    $nvUri = "https://management.azure.com/subscriptions/$sub/resourceGroups/$resourceGroup/providers/Microsoft.ApiManagement/service/$apimName/namedValues/entra-audience?api-version=2022-08-01"
    $nvBody = @{ properties = @{ displayName = 'entra-audience'; value = $appId; secret = $false } } | ConvertTo-Json -Depth 5
    try {
        Invoke-RestMethod -Method Put -Uri $nvUri -Headers @{ Authorization = "Bearer $mgmtToken"; 'Content-Type' = 'application/json' } -Body $nvBody | Out-Null
        Write-Host "Updated APIM named value 'entra-audience' to $appId."
    }
    catch {
        Write-Warning "Could not update APIM named value 'entra-audience': $($_.Exception.Message)"
    }
}
else {
    Write-Warning "No APIM instance found in '$resourceGroup'; skipping named value sync."
}

foreach ($svc in @('api', 'worker', 'scheduler', 'drasi')) {
    $caName = "$env:AZURE_ENV_NAME-$svc"
    $exists = az containerapp show -g $resourceGroup -n $caName --query name --output tsv 2>$null
    if ($exists -and $exists -ne 'null') {
        az containerapp update -g $resourceGroup -n $caName --set-env-vars "EXECUTION_LEDGER_ENTRA_AUDIENCE=$appId" --only-show-errors 2>$null | Out-Null
        Write-Host "Updated Container App '$caName' EXECUTION_LEDGER_ENTRA_AUDIENCE to $appId."
    }
}

Write-Host ""
Write-Host "Connection details for Tauri operator app:"
Write-Host "  API base URL  : https://`$AZURE_ENV_NAME-api.<region>.azurecontainerapps.io"
Write-Host "  APIM gateway  : $(azd env get-value apimGatewayUrl 2>$null)/execution-ledger"
Write-Host "  Entra audience: $appId"
Write-Host "  Identifier URI: $appAudience"
Write-Host "  Scopes        : jobs:read:own, platform:read, platform:write, mcp:execute"
Write-Host ""
Write-Host "Acquire a token:"
Write-Host "  az account get-access-token --resource '$appAudience' --query accessToken -o tsv"
