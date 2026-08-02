#Requires -Version 7.6
#Requires -PSEdition Core

<#
.SYNOPSIS
    Validates Execution Ledger deployment capacity and cost against acceptance gates.

.DESCRIPTION
    Checks Container App replica counts, PostgreSQL SKU, Service Bus tier, and other
    resource dimensions against defined capacity and cost thresholds. Produces a
    pass/fail gate result suitable for CI/CD deployment pipelines and operational
    proof harness integration.

.EXAMPLE
    .\Invoke-CapacityCostGate.ps1 -ResourceGroupName "rg-executionledger-prod" `
        -ContainerAppApiName "api" -ContainerAppWorkerName "worker" `
        -ContainerAppSchedulerName "scheduler" -ServiceBusNamespaceName "sbns-prod"
#>

[CmdletBinding(PositionalBinding = $false)]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrWhiteSpace()]
    [string]$ResourceGroupName,

    [Parameter(Mandatory)]
    [ValidateNotNullOrWhiteSpace()]
    [string]$ContainerAppApiName,

    [Parameter(Mandatory)]
    [ValidateNotNullOrWhiteSpace()]
    [string]$ContainerAppWorkerName,

    [Parameter(Mandatory)]
    [ValidateNotNullOrWhiteSpace()]
    [string]$ContainerAppSchedulerName,

    [Parameter(Mandatory)]
    [ValidateNotNullOrWhiteSpace()]
    [string]$ServiceBusNamespaceName,

    [ValidateNotNullOrWhiteSpace()]
    [string]$ServiceBusQueueName = 'execution-ledger-intents',

    [string]$ApiBaseUrl,

    [string]$ApiManagementServiceName,

    [Parameter(Mandatory)]
    [ValidateNotNullOrWhiteSpace()]
    [string]$ContainerRegistryName,

    [Parameter(Mandatory)]
    [ValidateNotNullOrWhiteSpace()]
    [string]$PostgresServerName,

    [Parameter(Mandatory)]
    [ValidateNotNullOrWhiteSpace()]
    [string]$LogAnalyticsWorkspaceName,

    [ValidateRange(0, 100000)]
    [int]$MaxQueueActiveMessages = 100,

    [ValidateRange(0, 100)]
    [int]$MaxTotalMinReplicas = 3,

    [ValidateNotNullOrWhiteSpace()]
    [string]$TodoId = 'capacity-gate-automation',

    [ValidateNotNullOrWhiteSpace()]
    [string]$OutputRoot = (Join-Path $PSScriptRoot -ChildPath '..', 'artifacts', 'operational-proof'),

    [ValidateNotNullOrWhiteSpace()]
    [string]$BaselinePath = (Join-Path $PSScriptRoot -ChildPath '..', 'artifacts', 'operational-proof', 'baseline-latest.json'),

    [string]$SubscriptionId
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$runTimestamp = Get-Date
$runId = $runTimestamp.ToString('yyyyMMdd-HHmmss')
$outputRootResolved = [System.IO.Path]::GetFullPath($OutputRoot)
$runDirectory = Join-Path $outputRootResolved -ChildPath "manual-$runId-capacity-cost"
$commandDirectory = Join-Path $runDirectory -ChildPath 'commands'
$artifactPath = Join-Path $runDirectory -ChildPath 'capacity-cost-gates.json'

$null = New-Item -ItemType Directory -Path $commandDirectory -Force

function Write-JsonArtifact {
    param(
        [Parameter(Mandatory)]
        [ValidateNotNull()]
        [object]$InputObject,

        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$Path
    )

    $json = $InputObject | ConvertTo-Json -Depth 32
    Set-Content -Path $Path -Value $json -Encoding utf8
}

function Convert-AzListPayloadToArray {
    param(
        [AllowNull()]
        [object]$Payload
    )

    if ($null -eq $Payload) {
        return @()
    }

    if ($Payload -is [System.Array]) {
        return @($Payload)
    }

    $valueProperty = $Payload.PSObject.Properties['value']
    if ($null -ne $valueProperty) {
        return @($valueProperty.Value)
    }

    return @($Payload)
}

function Get-NestedValue {
    param(
        [AllowNull()]
        [object]$InputObject,

        [Parameter(Mandatory)]
        [ValidateNotNullOrEmpty()]
        [string[]]$Path,

        [AllowNull()]
        [object]$Default = $null
    )

    $current = $InputObject
    foreach ($segment in $Path) {
        if ($null -eq $current) {
            return $Default
        }

        if ($current -is [System.Collections.IDictionary]) {
            if (-not $current.Contains($segment)) {
                return $Default
            }
            $current = $current[$segment]
            continue
        }

        $property = $current.PSObject.Properties[$segment]
        if ($null -eq $property) {
            return $Default
        }

        $current = $property.Value
    }

    if ($null -eq $current) {
        return $Default
    }

    return $current
}

function Invoke-AzJsonCommand {
    param(
        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$CommandName,

        [Parameter(Mandatory)]
        [ValidateNotNull()]
        [string[]]$Arguments,

        [switch]$AllowFailure
    )

    $finalArguments = [System.Collections.Generic.List[string]]::new()
    foreach ($argument in $Arguments) {
        $null = $finalArguments.Add($argument)
    }

    if (-not [string]::IsNullOrWhiteSpace($SubscriptionId) -and -not $finalArguments.Contains('--subscription')) {
        $null = $finalArguments.Add('--subscription')
        $null = $finalArguments.Add($SubscriptionId)
    }

    if (-not $finalArguments.Contains('--only-show-errors')) {
        $null = $finalArguments.Add('--only-show-errors')
    }

    if (-not $finalArguments.Contains('--output')) {
        $null = $finalArguments.Add('--output')
        $null = $finalArguments.Add('json')
    }

    $started = Get-Date
    $rawOutput = & az @finalArguments 2>&1
    $exitCode = $LASTEXITCODE
    $finished = Get-Date
    $rawText = ($rawOutput | ForEach-Object { "$_" }) -join [Environment]::NewLine
    $outputPath = Join-Path $commandDirectory -ChildPath ('{0}-{1}.log' -f (Get-Date -Format 'HHmmssfff'), $CommandName)
    Set-Content -Path $outputPath -Value $rawText -Encoding utf8

    if ($exitCode -ne 0 -and -not $AllowFailure.IsPresent) {
        throw "Azure CLI command '$CommandName' failed with exit code $exitCode. See $outputPath"
    }

    $parsedJson = $null
    if ($exitCode -eq 0 -and -not [string]::IsNullOrWhiteSpace($rawText)) {
        try {
            $parsedJson = $rawText | ConvertFrom-Json -Depth 64
        }
        catch {
            if (-not $AllowFailure.IsPresent) {
                throw "Azure CLI command '$CommandName' returned non-JSON output. See $outputPath"
            }
        }
    }

    return [pscustomobject]@{
        commandName   = $CommandName
        commandText   = "az $($finalArguments -join ' ')"
        startedAtUtc  = $started.ToUniversalTime().ToString('o')
        finishedAtUtc = $finished.ToUniversalTime().ToString('o')
        durationMs    = [int]([System.Math]::Round(($finished - $started).TotalMilliseconds))
        exitCode      = $exitCode
        succeeded     = ($exitCode -eq 0)
        outputFile    = $outputPath
        json          = $parsedJson
    }
}

function Get-HealthSnapshot {
    param(
        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$Uri
    )

    $started = Get-Date
    try {
        $response = Invoke-WebRequest -Uri $Uri -Method Get -TimeoutSec 30
        $finished = Get-Date
        return [ordered]@{
            uri          = $Uri
            succeeded    = $true
            statusCode   = [int]$response.StatusCode
            durationMs   = [int]([System.Math]::Round(($finished - $started).TotalMilliseconds))
            startedAtUtc = $started.ToUniversalTime().ToString('o')
            finishedAtUtc = $finished.ToUniversalTime().ToString('o')
            body         = $response.Content
            error        = $null
        }
    }
    catch {
        $finished = Get-Date
        return [ordered]@{
            uri          = $Uri
            succeeded    = $false
            statusCode   = $null
            durationMs   = [int]([System.Math]::Round(($finished - $started).TotalMilliseconds))
            startedAtUtc = $started.ToUniversalTime().ToString('o')
            finishedAtUtc = $finished.ToUniversalTime().ToString('o')
            body         = $null
            error        = $_.Exception.Message
        }
    }
}

$evidenceLinks = [System.Collections.Generic.List[string]]::new()
$baselineMetadata = $null
if (Test-Path -LiteralPath $BaselinePath -PathType Leaf) {
    $baselineMetadata = Get-Content -LiteralPath $BaselinePath -Raw | ConvertFrom-Json -Depth 64
    $evidenceLinks.Add($BaselinePath)
    if ($null -ne $baselineMetadata.summaryPath -and -not [string]::IsNullOrWhiteSpace([string]$baselineMetadata.summaryPath)) {
        $evidenceLinks.Add([string]$baselineMetadata.summaryPath)
    }
}

$containerApps = [ordered]@{}
foreach ($service in @(
        @{ Role = 'api'; Name = $ContainerAppApiName },
        @{ Role = 'worker'; Name = $ContainerAppWorkerName },
        @{ Role = 'scheduler'; Name = $ContainerAppSchedulerName }
    )) {
    $showResult = Invoke-AzJsonCommand -CommandName "containerapp-$($service.Role)-show" -Arguments @('containerapp', 'show', '--name', $service.Name, '--resource-group', $ResourceGroupName)
    $listResult = Invoke-AzJsonCommand -CommandName "containerapp-$($service.Role)-revision-list" -Arguments @('containerapp', 'revision', 'list', '--name', $service.Name, '--resource-group', $ResourceGroupName)
    $evidenceLinks.Add($showResult.outputFile)
    $evidenceLinks.Add($listResult.outputFile)

    $showJson = $showResult.json
    $revisionList = Convert-AzListPayloadToArray -Payload $listResult.json
    $latestRevisionName = [string]$showJson.properties.latestRevisionName
    $latestReadyRevisionName = [string]$showJson.properties.latestReadyRevisionName
    $latestRevision = $revisionList | Where-Object { [string]::Equals([string]$_.name, $latestRevisionName, [System.StringComparison]::OrdinalIgnoreCase) } | Select-Object -First 1
    $latestReadyRevision = $revisionList | Where-Object { [string]::Equals([string]$_.name, $latestReadyRevisionName, [System.StringComparison]::OrdinalIgnoreCase) } | Select-Object -First 1
    $container = Get-NestedValue -InputObject $showJson -Path @('properties', 'template', 'containers')
    if ($container -is [System.Array]) {
        $container = $container | Select-Object -First 1
    }

    $containerApps[$service.Role] = [ordered]@{
        name                    = $service.Name
        minReplicas             = [int](Get-NestedValue -InputObject $showJson -Path @('properties', 'template', 'scale', 'minReplicas') -Default 0)
        maxReplicas             = [int](Get-NestedValue -InputObject $showJson -Path @('properties', 'template', 'scale', 'maxReplicas') -Default 0)
        currentReplicas         = if ($null -ne $latestRevision) { [Nullable[int]](Get-NestedValue -InputObject $latestRevision -Path @('properties', 'replicas')) } else { $null }
        workloadProfileName     = [string](Get-NestedValue -InputObject $showJson -Path @('properties', 'workloadProfileName') -Default '')
        provisioningState       = [string](Get-NestedValue -InputObject $showJson -Path @('properties', 'provisioningState') -Default '')
        runningStatus           = [string](Get-NestedValue -InputObject $showJson -Path @('properties', 'runningStatus') -Default '')
        latestRevision          = $latestRevisionName
        latestReadyRevision     = $latestReadyRevisionName
        latestRevisionHealth    = if ($null -ne $latestRevision) { [string](Get-NestedValue -InputObject $latestRevision -Path @('properties', 'healthState') -Default '') } else { $null }
        latestRevisionState     = if ($null -ne $latestRevision) { [string](Get-NestedValue -InputObject $latestRevision -Path @('properties', 'runningState') -Default '') } else { $null }
        latestRevisionStateInfo = if ($null -ne $latestRevision) { [string](Get-NestedValue -InputObject $latestRevision -Path @('properties', 'runningStateDetails') -Default '') } else { $null }
        latestReadyHealth       = if ($null -ne $latestReadyRevision) { [string](Get-NestedValue -InputObject $latestReadyRevision -Path @('properties', 'healthState') -Default '') } else { $null }
        cpu                     = if ($null -ne $container) { [string](Get-NestedValue -InputObject $container -Path @('resources', 'cpu') -Default '') } else { $null }
        memory                  = if ($null -ne $container) { [string](Get-NestedValue -InputObject $container -Path @('resources', 'memory') -Default '') } else { $null }
        ingressFqdn             = [string](Get-NestedValue -InputObject $showJson -Path @('properties', 'configuration', 'ingress', 'fqdn') -Default '')
    }
}

$serviceBusNamespaceResult = Invoke-AzJsonCommand -CommandName 'servicebus-namespace-show' -Arguments @('servicebus', 'namespace', 'show', '--name', $ServiceBusNamespaceName, '--resource-group', $ResourceGroupName)
$serviceBusQueueResult = Invoke-AzJsonCommand -CommandName 'servicebus-queue-show' -Arguments @('servicebus', 'queue', 'show', '--name', $ServiceBusQueueName, '--namespace-name', $ServiceBusNamespaceName, '--resource-group', $ResourceGroupName)
$acrResult = Invoke-AzJsonCommand -CommandName 'acr-show' -Arguments @('acr', 'show', '--name', $ContainerRegistryName, '--resource-group', $ResourceGroupName)
$acrRetentionResult = Invoke-AzJsonCommand -CommandName 'acr-retention-show' -AllowFailure -Arguments @('acr', 'config', 'retention', 'show', '--registry', $ContainerRegistryName)
$postgresResult = Invoke-AzJsonCommand -CommandName 'postgres-show' -Arguments @('postgres', 'flexible-server', 'show', '--name', $PostgresServerName, '--resource-group', $ResourceGroupName)
$logAnalyticsResult = Invoke-AzJsonCommand -CommandName 'loganalytics-workspace-show' -Arguments @('monitor', 'log-analytics', 'workspace', 'show', '--workspace-name', $LogAnalyticsWorkspaceName, '--resource-group', $ResourceGroupName)

foreach ($cmd in @($serviceBusNamespaceResult, $serviceBusQueueResult, $acrResult, $acrRetentionResult, $postgresResult, $logAnalyticsResult)) {
    $evidenceLinks.Add($cmd.outputFile)
}

$apiIngressFqdn = [string]$containerApps.api.ingressFqdn
$resolvedApiBaseUrl = $ApiBaseUrl
if ([string]::IsNullOrWhiteSpace($resolvedApiBaseUrl)) {
    $apimResult = $null
    if (-not [string]::IsNullOrWhiteSpace($ApiManagementServiceName)) {
        $apimResult = Invoke-AzJsonCommand -AllowFailure -CommandName 'apim-show' -Arguments @('apim', 'show', '--name', $ApiManagementServiceName, '--resource-group', $ResourceGroupName)
    }
    else {
        $apimResult = Invoke-AzJsonCommand -AllowFailure -CommandName 'apim-list' -Arguments @('apim', 'list', '--resource-group', $ResourceGroupName)
    }

    if ($null -ne $apimResult) {
        $evidenceLinks.Add($apimResult.outputFile)
        if ($apimResult.succeeded) {
            if (-not [string]::IsNullOrWhiteSpace($ApiManagementServiceName)) {
                $apimGatewayUrl = [string](Get-NestedValue -InputObject $apimResult.json -Path @('gatewayUrl') -Default '')
                if (-not [string]::IsNullOrWhiteSpace($apimGatewayUrl)) {
                    $resolvedApiBaseUrl = "$($apimGatewayUrl.TrimEnd('/'))/execution-ledger"
                }
            }
            else {
                $apimItems = Convert-AzListPayloadToArray -Payload $apimResult.json
                if ($apimItems.Count -gt 0) {
                    $apimGatewayUrl = [string](Get-NestedValue -InputObject $apimItems[0] -Path @('gatewayUrl') -Default '')
                    if (-not [string]::IsNullOrWhiteSpace($apimGatewayUrl)) {
                        $resolvedApiBaseUrl = "$($apimGatewayUrl.TrimEnd('/'))/execution-ledger"
                    }
                }
            }
        }
    }
}

if ([string]::IsNullOrWhiteSpace($resolvedApiBaseUrl)) {
    if ([string]::IsNullOrWhiteSpace($apiIngressFqdn)) {
        throw "API container app '$ContainerAppApiName' does not expose ingress FQDN, and APIM gateway was not resolved."
    }
    $resolvedApiBaseUrl = "https://$apiIngressFqdn"
}

$healthSnapshot = Get-HealthSnapshot -Uri "$($resolvedApiBaseUrl.TrimEnd('/'))/health"
$queueMessageCount = [int](Get-NestedValue -InputObject $serviceBusQueueResult.json -Path @('countDetails', 'activeMessageCount') -Default 0)
$queueDeadLetterCount = [int](Get-NestedValue -InputObject $serviceBusQueueResult.json -Path @('countDetails', 'deadLetterMessageCount') -Default 0)
$dailyQuotaGb = [double](Get-NestedValue -InputObject $logAnalyticsResult.json -Path @('workspaceCapping', 'dailyQuotaGb') -Default -1)
$totalMinReplicas = [int]$containerApps.api.minReplicas + [int]$containerApps.worker.minReplicas + [int]$containerApps.scheduler.minReplicas
$totalMaxReplicas = [int]$containerApps.api.maxReplicas + [int]$containerApps.worker.maxReplicas + [int]$containerApps.scheduler.maxReplicas

$capacityChecks = [System.Collections.Generic.List[object]]::new()
$costChecks = [System.Collections.Generic.List[object]]::new()

$replicaBoundsPass = $true
foreach ($serviceName in @('api', 'worker', 'scheduler')) {
    $serviceSnapshot = $containerApps[$serviceName]
    if ($serviceSnapshot.maxReplicas -lt $serviceSnapshot.minReplicas -or $serviceSnapshot.minReplicas -lt 0) {
        $replicaBoundsPass = $false
        break
    }
}
$capacityChecks.Add([ordered]@{
        name    = 'replica_bounds_defined'
        status  = if ($replicaBoundsPass) { 'pass' } else { 'fail' }
        details = if ($replicaBoundsPass) {
            "Replica bounds are defined for all services (api $($containerApps.api.minReplicas)-$($containerApps.api.maxReplicas), worker $($containerApps.worker.minReplicas)-$($containerApps.worker.maxReplicas), scheduler $($containerApps.scheduler.minReplicas)-$($containerApps.scheduler.maxReplicas))."
        }
        else {
            'At least one service has invalid replica bounds (maxReplicas < minReplicas or minReplicas < 0).'
        }
    })

$runtimeProvisioningPass = (
    [string]::Equals($containerApps.api.provisioningState, 'Succeeded', [System.StringComparison]::OrdinalIgnoreCase) -and
    [string]::Equals($containerApps.worker.provisioningState, 'Succeeded', [System.StringComparison]::OrdinalIgnoreCase) -and
    [string]::Equals($containerApps.scheduler.provisioningState, 'Succeeded', [System.StringComparison]::OrdinalIgnoreCase) -and
    [string]::Equals($containerApps.api.runningStatus, 'Running', [System.StringComparison]::OrdinalIgnoreCase) -and
    [string]::Equals($containerApps.worker.runningStatus, 'Running', [System.StringComparison]::OrdinalIgnoreCase) -and
    [string]::Equals($containerApps.scheduler.runningStatus, 'Running', [System.StringComparison]::OrdinalIgnoreCase)
)
$capacityChecks.Add([ordered]@{
        name    = 'runtime_provisioning_state'
        status  = if ($runtimeProvisioningPass) { 'pass' } else { 'fail' }
        details = if ($runtimeProvisioningPass) {
            'All container apps report provisioningState=Succeeded and runningStatus=Running.'
        }
        else {
            'One or more container apps are not fully healthy at platform runtime level.'
        }
    })

$apiRevisionHealthy = (
    [string]::Equals($containerApps.api.latestRevisionHealth, 'Healthy', [System.StringComparison]::OrdinalIgnoreCase) -and
    [string]::Equals($containerApps.api.latestRevisionState, 'Running', [System.StringComparison]::OrdinalIgnoreCase)
)
$capacityChecks.Add([ordered]@{
        name    = 'api_revision_health'
        status  = if ($apiRevisionHealthy) { 'pass' } else { 'fail' }
        details = if ($apiRevisionHealthy) {
            "API latest revision '$($containerApps.api.latestRevision)' is Healthy and Running."
        }
        else {
            "API latest revision '$($containerApps.api.latestRevision)' health=$($containerApps.api.latestRevisionHealth), state=$($containerApps.api.latestRevisionState), details=$($containerApps.api.latestRevisionStateInfo)."
        }
    })

$endpointReachable = $healthSnapshot.succeeded -and $healthSnapshot.statusCode -eq 200
$capacityChecks.Add([ordered]@{
        name    = 'api_endpoint_reachability'
        status  = if ($endpointReachable) { 'pass' } else { 'fail' }
        details = if ($endpointReachable) {
            "GET /health returned HTTP 200 in $($healthSnapshot.durationMs)ms."
        }
        else {
            "GET /health failed. statusCode=$($healthSnapshot.statusCode); error=$($healthSnapshot.error)"
        }
    })

$queueBacklogWithinThreshold = $queueMessageCount -le $MaxQueueActiveMessages
$capacityChecks.Add([ordered]@{
        name    = 'queue_backlog_pressure'
        status  = if ($queueBacklogWithinThreshold) { 'pass' } else { 'fail' }
        details = if ($queueBacklogWithinThreshold) {
            "Queue activeMessageCount=$queueMessageCount is within threshold <= $MaxQueueActiveMessages."
        }
        else {
            "Queue activeMessageCount=$queueMessageCount exceeds threshold <= $MaxQueueActiveMessages."
        }
    })

$deployedSkusCapturedPass = (
    -not [string]::IsNullOrWhiteSpace([string]$containerApps.api.workloadProfileName) -and
    -not [string]::IsNullOrWhiteSpace([string](Get-NestedValue -InputObject $acrResult.json -Path @('sku', 'name') -Default '')) -and
    -not [string]::IsNullOrWhiteSpace([string](Get-NestedValue -InputObject $postgresResult.json -Path @('sku', 'name') -Default '')) -and
    -not [string]::IsNullOrWhiteSpace([string](Get-NestedValue -InputObject $serviceBusNamespaceResult.json -Path @('sku', 'name') -Default '')) -and
    -not [string]::IsNullOrWhiteSpace([string](Get-NestedValue -InputObject $logAnalyticsResult.json -Path @('sku', 'name') -Default ''))
)
$costChecks.Add([ordered]@{
        name    = 'deployed_skus_captured'
        status  = if ($deployedSkusCapturedPass) { 'pass' } else { 'fail' }
        details = if ($deployedSkusCapturedPass) {
            "Container Apps $($containerApps.api.workloadProfileName), ACR $(Get-NestedValue -InputObject $acrResult.json -Path @('sku', 'name') -Default ''), Postgres $(Get-NestedValue -InputObject $postgresResult.json -Path @('sku', 'name') -Default ''), Service Bus $(Get-NestedValue -InputObject $serviceBusNamespaceResult.json -Path @('sku', 'name') -Default ''), Log Analytics $(Get-NestedValue -InputObject $logAnalyticsResult.json -Path @('sku', 'name') -Default '')."
        }
        else {
            'One or more deployed SKUs are missing from current runtime snapshot.'
        }
    })

$logAnalyticsCapPass = $dailyQuotaGb -gt 0
$costChecks.Add([ordered]@{
        name    = 'log_analytics_daily_cap'
        status  = if ($logAnalyticsCapPass) { 'pass' } else { 'fail' }
        details = if ($logAnalyticsCapPass) {
            "workspaceCapping.dailyQuotaGb=$dailyQuotaGb."
        }
        else {
            "workspaceCapping.dailyQuotaGb=$dailyQuotaGb (unbounded ingestion)."
        }
    })

$baselineComputePass = $totalMinReplicas -le $MaxTotalMinReplicas
$costChecks.Add([ordered]@{
        name    = 'always_on_baseline_compute'
        status  = if ($baselineComputePass) { 'pass' } else { 'fail' }
        details = if ($baselineComputePass) {
            "totalMinReplicas=$totalMinReplicas is within threshold <= $MaxTotalMinReplicas."
        }
        else {
            "totalMinReplicas=$totalMinReplicas exceeds threshold <= $MaxTotalMinReplicas."
        }
    })

$queueCostPressurePass = $queueMessageCount -eq 0 -and $queueDeadLetterCount -eq 0
$costChecks.Add([ordered]@{
        name    = 'queue_backlog_pressure'
        status  = if ($queueCostPressurePass) { 'pass' } else { 'fail' }
        details = if ($queueCostPressurePass) {
            "Queue activeMessageCount=$queueMessageCount and deadLetterMessageCount=$queueDeadLetterCount."
        }
        else {
            "Queue activeMessageCount=$queueMessageCount and deadLetterMessageCount=$queueDeadLetterCount indicate backlog cost risk."
        }
    })

$acrSkuName = [string](Get-NestedValue -InputObject $acrResult.json -Path @('sku', 'name') -Default '')
$acrRetentionStatus = if ($acrRetentionResult.succeeded) { [string](Get-NestedValue -InputObject $acrRetentionResult.json -Path @('status') -Default '') } else { $null }
$acrLifecycleGuardPass = $false
$acrLifecycleCheckStatus = 'evidence-gapped'
$acrLifecycleDetails = ''

if ([string]::Equals($acrSkuName, 'Premium', [System.StringComparison]::OrdinalIgnoreCase)) {
    $acrLifecycleGuardPass = [string]::Equals($acrRetentionStatus, 'enabled', [System.StringComparison]::OrdinalIgnoreCase)
    $acrLifecycleCheckStatus = if ($acrRetentionResult.succeeded) {
        if ($acrLifecycleGuardPass) { 'pass' } else { 'fail' }
    }
    else {
        'evidence-gapped'
    }
    $acrLifecycleDetails = if ($acrRetentionResult.succeeded) {
        "ACR retention status=$acrRetentionStatus, days=$(Get-NestedValue -InputObject $acrRetentionResult.json -Path @('days') -Default '')."
    }
    else {
        "Unable to resolve ACR retention policy from current SKU. See $($acrRetentionResult.outputFile)"
    }
}
else {
    $acrLifecycleGuardPass = $true
    $acrLifecycleCheckStatus = 'pass'
    $acrLifecycleDetails = "ACR SKU '$acrSkuName' does not support retention policy; lifecycle guard is not applicable."
}

$costChecks.Add([ordered]@{
        name    = 'acr_lifecycle_guard'
        status  = $acrLifecycleCheckStatus
        details = $acrLifecycleDetails
    })

$capacityGatePass = $true
foreach ($check in $capacityChecks) {
    if (-not [string]::Equals([string]$check.status, 'pass', [System.StringComparison]::OrdinalIgnoreCase)) {
        $capacityGatePass = $false
        break
    }
}

$costGatePass = $true
foreach ($check in $costChecks) {
    if (-not [string]::Equals([string]$check.status, 'pass', [System.StringComparison]::OrdinalIgnoreCase)) {
        $costGatePass = $false
        break
    }
}

$monthlyRiskDrivers = [System.Collections.Generic.List[string]]::new()
if (-not $logAnalyticsCapPass) {
    $monthlyRiskDrivers.Add('Log Analytics ingestion is unbounded because dailyQuotaGb is not set above zero.')
}
if (-not $baselineComputePass) {
    $monthlyRiskDrivers.Add('Always-on min replicas keep non-zero baseline spend under low traffic.')
}
if (-not $queueCostPressurePass) {
    $monthlyRiskDrivers.Add('Queue backlog and dead-letter accumulation can increase compute and investigation costs.')
}
if (-not $acrLifecycleGuardPass) {
    $monthlyRiskDrivers.Add('Container registry retention controls are missing or unresolved, which can increase image storage cost.')
}
if (-not $apiRevisionHealthy) {
    $monthlyRiskDrivers.Add('API revision health issues can amplify retries and telemetry ingestion spend.')
}

$artifact = [ordered]@{
    generatedAtUtc  = (Get-Date).ToUniversalTime().ToString('o')
    todoId          = $TodoId
    runId           = $runId
    runArtifactPath = $runDirectory
    capacity        = [ordered]@{
        gate     = if ($capacityGatePass) { 'pass' } else { 'fail' }
        checks   = $capacityChecks
        findings = [ordered]@{
            api         = $containerApps.api
            worker      = $containerApps.worker
            scheduler   = $containerApps.scheduler
            serviceBus  = [ordered]@{
                namespace               = $ServiceBusNamespaceName
                queue                   = $ServiceBusQueueName
                activeMessageCount      = $queueMessageCount
                deadLetterMessageCount  = $queueDeadLetterCount
                status                  = [string](Get-NestedValue -InputObject $serviceBusQueueResult.json -Path @('status') -Default '')
            }
            healthCheck = $healthSnapshot
        }
    }
    cost            = [ordered]@{
        gate             = if ($costGatePass) { 'pass' } else { 'fail' }
        checks           = $costChecks
        resourceSnapshot = [ordered]@{
            containerApps = [ordered]@{
                workloadProfile = [string]$containerApps.api.workloadProfileName
                apiCpu          = [string]$containerApps.api.cpu
                apiMemory       = [string]$containerApps.api.memory
                workerCpu       = [string]$containerApps.worker.cpu
                workerMemory    = [string]$containerApps.worker.memory
                schedulerCpu    = [string]$containerApps.scheduler.cpu
                schedulerMemory = [string]$containerApps.scheduler.memory
                totalMinReplicas = $totalMinReplicas
                totalMaxReplicas = $totalMaxReplicas
            }
            acr           = [ordered]@{
                name                = $ContainerRegistryName
                sku                 = [string](Get-NestedValue -InputObject $acrResult.json -Path @('sku', 'name') -Default '')
                adminUserEnabled    = [bool](Get-NestedValue -InputObject $acrResult.json -Path @('adminUserEnabled') -Default $false)
                publicNetworkAccess = [string](Get-NestedValue -InputObject $acrResult.json -Path @('publicNetworkAccess') -Default '')
                retentionStatus     = $acrRetentionStatus
            }
            postgres      = [ordered]@{
                name            = $PostgresServerName
                skuName         = [string](Get-NestedValue -InputObject $postgresResult.json -Path @('sku', 'name') -Default '')
                tier            = [string](Get-NestedValue -InputObject $postgresResult.json -Path @('sku', 'tier') -Default '')
                storageGb       = [int](Get-NestedValue -InputObject $postgresResult.json -Path @('storage', 'storageSizeGb') -Default 0)
                iops            = [int](Get-NestedValue -InputObject $postgresResult.json -Path @('storage', 'iops') -Default 0)
                storageTier     = [string](Get-NestedValue -InputObject $postgresResult.json -Path @('storage', 'tier') -Default '')
                highAvailability = [string](Get-NestedValue -InputObject $postgresResult.json -Path @('highAvailability', 'state') -Default '')
                state           = [string](Get-NestedValue -InputObject $postgresResult.json -Path @('state') -Default '')
            }
            serviceBus    = [ordered]@{
                namespace           = $ServiceBusNamespaceName
                sku                 = [string](Get-NestedValue -InputObject $serviceBusNamespaceResult.json -Path @('sku', 'name') -Default '')
                queue               = $ServiceBusQueueName
                queueMessageCount   = $queueMessageCount
                queueMaxSizeMb      = [int](Get-NestedValue -InputObject $serviceBusQueueResult.json -Path @('maxSizeInMegabytes') -Default 0)
                deadLetterCount     = $queueDeadLetterCount
            }
            logAnalytics  = [ordered]@{
                name          = $LogAnalyticsWorkspaceName
                sku           = [string](Get-NestedValue -InputObject $logAnalyticsResult.json -Path @('sku', 'name') -Default '')
                retentionDays = [int](Get-NestedValue -InputObject $logAnalyticsResult.json -Path @('retentionInDays') -Default 0)
                dailyQuotaGb  = $dailyQuotaGb
            }
        }
        monthlyRiskDrivers = $monthlyRiskDrivers
    }
    runtimeSnapshot = [ordered]@{
        capturedAtUtc = (Get-Date).ToUniversalTime().ToString('o')
        apiBaseUrl    = $resolvedApiBaseUrl
        healthCheck   = $healthSnapshot
        containerApps = $containerApps
        serviceBusQueue = [ordered]@{
            namespace               = $ServiceBusNamespaceName
            queue                   = $ServiceBusQueueName
            status                  = [string](Get-NestedValue -InputObject $serviceBusQueueResult.json -Path @('status') -Default '')
            activeMessageCount      = $queueMessageCount
            deadLetterMessageCount  = $queueDeadLetterCount
        }
    }
    evidence        = $evidenceLinks
}

if ($null -ne $baselineMetadata) {
    $artifact.baseline = $baselineMetadata
}

Write-JsonArtifact -InputObject $artifact -Path $artifactPath
Write-Information "Capacity-cost artifact: $artifactPath"

if (-not $capacityGatePass -or -not $costGatePass) {
    exit 1
}

exit 0
