#Requires -Version 7.6
#Requires -PSEdition Core

<#
.SYNOPSIS
    Runs the Execution Ledger operational proof harness across baseline, soak, chaos,
    and collect stages against a deployed environment.

.DESCRIPTION
    Validates deployment health by executing staged operational checks: baseline
    precondition validation, soak load testing, chaos fault injection (Service Bus
    queue-state disruption via Azure Chaos Studio), and evidence collection. Exits
    with overallStatus=Succeeded only when all stages pass. Optionally invokes a
    bounded PostgreSQL restart/failover fault.

.EXAMPLE
    .\Invoke-OperationalProofRun.ps1 -ResourceGroupName "rg-executionledger-prod" `
        -ContainerAppApiName "api" -ContainerAppWorkerName "worker" `
        -ContainerAppSchedulerName "scheduler" -ServiceBusNamespaceName "sbns-prod"
#>

[CmdletBinding(PositionalBinding = $false)]
param(
    [string[]]$Stages = @('baseline', 'soak', 'chaos', 'collect'),

    [string]$ApiBaseUrl,

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

    [string]$ApiManagementServiceName,

    [string]$ChaosWorkspaceName,

    [ValidateNotNullOrWhiteSpace()]
    [string]$ChaosScenarioName = 'MessagingDisruption-1.0',

    [string]$ChaosScenarioConfigName,

    [ValidateRange(1, 1440)]
    [int]$SoakDurationMinutes = 15,

    [ValidateRange(5, 600)]
    [int]$SoakIntervalSeconds = 30,

    [ValidateRange(1, 240)]
    [int]$ChaosTimeoutMinutes = 20,

    [ValidateRange(5, 300)]
    [int]$ChaosPollIntervalSeconds = 20,

    [switch]$RunPostgresFault,

    [ValidateNotNullOrWhiteSpace()]
    [string]$PostgresFaultScriptPath = (Join-Path $PSScriptRoot -ChildPath 'Invoke-PostgresFlexibleServerFault.ps1'),

    [Parameter(Mandatory)]
    [ValidateNotNullOrWhiteSpace()]
    [string]$PostgresServerName,

    [ValidateSet('Restart', 'PlannedFailover', 'ForcedFailover')]
    [string]$PostgresFaultMode = 'Restart',

    [ValidateRange(1, 240)]
    [int]$PostgresFaultTimeoutMinutes = 20,

    [ValidateRange(5, 300)]
    [int]$PostgresFaultPollIntervalSeconds = 20,

    [ValidateRange(1, 72)]
    [int]$CollectActivityLogLookbackHours = 4,

    [ValidateNotNullOrWhiteSpace()]
    [string]$OutputRoot = (Join-Path $PSScriptRoot -ChildPath '..', 'artifacts', 'operational-proof'),

    [string]$SubscriptionId,

    [switch]$Apply
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$runTimestamp = Get-Date
$runId = $runTimestamp.ToString('yyyyMMdd-HHmmss')
$outputRootResolved = [System.IO.Path]::GetFullPath($OutputRoot)
$runDirectory = Join-Path $outputRootResolved -ChildPath "run-$runId"
$stageDirectory = Join-Path $runDirectory -ChildPath 'stages'
$commandDirectory = Join-Path $runDirectory -ChildPath 'commands'

$null = New-Item -ItemType Directory -Path $stageDirectory -Force
$null = New-Item -ItemType Directory -Path $commandDirectory -Force

$allowedStages = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
foreach ($allowedStageName in @('baseline', 'soak', 'chaos', 'collect')) {
    $null = $allowedStages.Add($allowedStageName)
}

$normalizedStages = [System.Collections.Generic.List[string]]::new()
foreach ($stageExpression in $Stages) {
    if ([string]::IsNullOrWhiteSpace($stageExpression)) {
        continue
    }

    foreach ($stageName in $stageExpression.Split(',', [System.StringSplitOptions]::RemoveEmptyEntries)) {
        $trimmedStageName = $stageName.Trim()
        if (-not [string]::IsNullOrWhiteSpace($trimmedStageName)) {
            $null = $normalizedStages.Add($trimmedStageName)
        }
    }
}

if ($normalizedStages.Count -eq 0) {
    throw 'At least one stage must be provided. Allowed values: baseline, soak, chaos, collect.'
}

$invalidStages = [System.Collections.Generic.List[string]]::new()
foreach ($stageName in $normalizedStages) {
    if (-not $allowedStages.Contains($stageName)) {
        $null = $invalidStages.Add($stageName)
    }
}

if ($invalidStages.Count -gt 0) {
    throw "Invalid stage value(s): $($invalidStages -join ', '). Allowed values: baseline, soak, chaos, collect."
}

$selectedStages = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
foreach ($stageName in $normalizedStages) {
    $null = $selectedStages.Add($stageName)
}

if (-not $PSBoundParameters.ContainsKey('ApiBaseUrl') -or [string]::IsNullOrWhiteSpace($ApiBaseUrl)) {
    $apiShowArguments = [System.Collections.Generic.List[string]]::new()
    foreach ($argument in @(
            'containerapp',
            'show',
            '--name', $ContainerAppApiName,
            '--resource-group', $ResourceGroupName,
            '--query', 'properties.configuration.ingress.fqdn',
            '--output', 'tsv',
            '--only-show-errors'
        )) {
        $null = $apiShowArguments.Add($argument)
    }

    if (-not [string]::IsNullOrWhiteSpace($SubscriptionId)) {
        $null = $apiShowArguments.Add('--subscription')
        $null = $apiShowArguments.Add($SubscriptionId)
    }

    $resolvedApiFqdnOutput = & az @apiShowArguments 2>&1
    $resolvedApiFqdnExitCode = $LASTEXITCODE
    if ($resolvedApiFqdnExitCode -ne 0) {
        $resolvedApiFqdnMessage = ($resolvedApiFqdnOutput | ForEach-Object { "$_" }) -join [Environment]::NewLine
        throw "Unable to resolve API ingress FQDN from Container App '$ContainerAppApiName'. $resolvedApiFqdnMessage"
    }

    $resolvedApiFqdn = (($resolvedApiFqdnOutput | ForEach-Object { "$_" }) -join [Environment]::NewLine).Trim()
    if ([string]::IsNullOrWhiteSpace($resolvedApiFqdn)) {
        throw "Container App '$ContainerAppApiName' returned an empty ingress FQDN."
    }

    $ApiBaseUrl = "https://$resolvedApiFqdn"

    $apimArguments = [System.Collections.Generic.List[string]]::new()
    if (-not [string]::IsNullOrWhiteSpace($ApiManagementServiceName)) {
        foreach ($argument in @('apim', 'show', '--name', $ApiManagementServiceName, '--resource-group', $ResourceGroupName, '--query', 'gatewayUrl', '--output', 'tsv', '--only-show-errors')) {
            $null = $apimArguments.Add($argument)
        }
    }
    else {
        foreach ($argument in @('apim', 'list', '--resource-group', $ResourceGroupName, '--query', '[0].gatewayUrl', '--output', 'tsv', '--only-show-errors')) {
            $null = $apimArguments.Add($argument)
        }
    }

    if (-not [string]::IsNullOrWhiteSpace($SubscriptionId)) {
        $null = $apimArguments.Add('--subscription')
        $null = $apimArguments.Add($SubscriptionId)
    }

    $apimGatewayRaw = & az @apimArguments 2>&1
    if ($LASTEXITCODE -eq 0) {
        $apimGatewayUrl = (($apimGatewayRaw | ForEach-Object { "$_" }) -join [Environment]::NewLine).Trim()
        if (-not [string]::IsNullOrWhiteSpace($apimGatewayUrl)) {
            $ApiBaseUrl = "$($apimGatewayUrl.TrimEnd('/'))/execution-ledger"
        }
    }
}

$metadata = [ordered]@{
    runId               = $runId
    startedAtUtc        = $runTimestamp.ToUniversalTime().ToString('o')
    selectedStages      = @($normalizedStages)
    apiBaseUrl          = $ApiBaseUrl
    resourceGroupName   = $ResourceGroupName
    chaosWorkspaceName  = $ChaosWorkspaceName
    chaosScenarioName   = $ChaosScenarioName
    chaosConfigName     = $ChaosScenarioConfigName
    outputDirectory     = $runDirectory
    apply               = $Apply.IsPresent
    runPostgresFault    = $RunPostgresFault.IsPresent
}

if ($PSBoundParameters.ContainsKey('SubscriptionId')) {
    $metadata.subscriptionId = $SubscriptionId
}

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

function Write-TextArtifact {
    param(
        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$Path,

        [Parameter(Mandatory)]
        [AllowEmptyString()]
        [string]$Content
    )

    Set-Content -Path $Path -Value $Content -Encoding utf8
}

function Get-StagePath {
    param(
        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$StageName,

        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$FileName
    )

    $stageRoot = Join-Path $stageDirectory -ChildPath $StageName
    $null = New-Item -ItemType Directory -Path $stageRoot -Force
    return Join-Path $stageRoot -ChildPath $FileName
}

function Invoke-AzCommand {
    param(
        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$StageName,

        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$CommandName,

        [Parameter(Mandatory)]
        [ValidateNotNull()]
        [string[]]$Arguments,

        [switch]$ExpectJson
    )

    $finalArguments = [System.Collections.Generic.List[string]]::new()
    foreach ($argument in $Arguments) {
        $null = $finalArguments.Add($argument)
    }

    if (-not [string]::IsNullOrWhiteSpace($SubscriptionId) -and -not ($finalArguments.Contains('--subscription'))) {
        $null = $finalArguments.Add('--subscription')
        $null = $finalArguments.Add($SubscriptionId)
    }

    if (-not ($finalArguments.Contains('--only-show-errors'))) {
        $null = $finalArguments.Add('--only-show-errors')
    }

    if ($ExpectJson -and -not ($finalArguments.Contains('--output'))) {
        $null = $finalArguments.Add('--output')
        $null = $finalArguments.Add('json')
    }

    $commandText = "az $($finalArguments -join ' ')"
    $started = Get-Date
    $rawText = ''
    $exitCode = 0
    $parsedJson = $null

    try {
        $rawOutput = & az @finalArguments 2>&1
        $exitCode = $LASTEXITCODE
        $rawText = ($rawOutput | ForEach-Object { "$_" }) -join [Environment]::NewLine
        if ($ExpectJson -and $exitCode -eq 0 -and -not [string]::IsNullOrWhiteSpace($rawText)) {
            try {
                $parsedJson = $rawText | ConvertFrom-Json -Depth 64
            }
            catch {
                $parsedJson = $null
            }
        }
    }
    catch {
        $exitCode = 1
        $rawText = $_.Exception.Message
    }

    $finished = Get-Date

    $result = [ordered]@{
        stageName      = $StageName
        commandName    = $CommandName
        commandText    = $commandText
        startedAtUtc   = $started.ToUniversalTime().ToString('o')
        finishedAtUtc  = $finished.ToUniversalTime().ToString('o')
        durationMs     = [int]([System.Math]::Round(($finished - $started).TotalMilliseconds))
        exitCode       = $exitCode
        succeeded      = ($exitCode -eq 0)
        outputFile     = ''
        parsedJsonFile = $null
    }

    $outputFileName = '{0}-{1}.log' -f (Get-Date -Format 'HHmmssfff'), $CommandName
    $outputFilePath = Join-Path $commandDirectory -ChildPath $outputFileName
    Write-TextArtifact -Path $outputFilePath -Content $rawText
    $result.outputFile = $outputFilePath

    if ($null -ne $parsedJson) {
        $jsonFileName = '{0}-{1}.json' -f (Get-Date -Format 'HHmmssfff'), $CommandName
        $jsonFilePath = Join-Path $commandDirectory -ChildPath $jsonFileName
        Write-JsonArtifact -InputObject $parsedJson -Path $jsonFilePath
        $result.parsedJsonFile = $jsonFilePath
    }

    return [pscustomobject]@{
        Result = $result
        Json   = $parsedJson
        Text   = $rawText
    }
}

function Invoke-HealthCheck {
    param(
        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$Uri
    )

    $started = Get-Date

    for ($attempt = 1; $attempt -le 2; $attempt++) {
        try {
            $response = Invoke-WebRequest -Method Get -Uri $Uri -TimeoutSec 30
            $finished = Get-Date
            return [ordered]@{
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
            if ($attempt -lt 2) {
                Start-Sleep -Seconds 2
                continue
            }

            $finished = Get-Date
            return [ordered]@{
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

function Resolve-ChaosScenarioContext {
    param(
        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$StageName
    )

    $workspaceHelp = Invoke-AzCommand -StageName $StageName -CommandName 'chaos-help' -Arguments @('chaos', '-h')
    $scenarioHelp = Invoke-AzCommand -StageName $StageName -CommandName 'chaos-scenario-help' -Arguments @('chaos', 'scenario', '-h')
    $runHelp = Invoke-AzCommand -StageName $StageName -CommandName 'chaos-scenario-run-help' -Arguments @('chaos', 'scenario', 'run', '-h')
    $runStartHelp = Invoke-AzCommand -StageName $StageName -CommandName 'chaos-scenario-run-start-help' -Arguments @('chaos', 'scenario', 'run', 'start', '-h')

    $helpResults = @($workspaceHelp.Result, $scenarioHelp.Result, $runHelp.Result, $runStartHelp.Result)
    foreach ($entry in $helpResults) {
        if (-not $entry.succeeded) {
            throw "Current Azure CLI chaos command surface validation failed on '$($entry.commandName)'."
        }
    }

    $resolvedWorkspace = $ChaosWorkspaceName
    if ([string]::IsNullOrWhiteSpace($resolvedWorkspace)) {
        $workspaceList = Invoke-AzCommand -StageName $StageName -CommandName 'chaos-workspace-list' -Arguments @('chaos', 'workspace', 'list', '--resource-group', $ResourceGroupName) -ExpectJson
        if (-not $workspaceList.Result.succeeded) {
            throw 'Unable to list Chaos Studio workspaces for the configured resource group.'
        }

        $workspaces = @(Convert-AzListPayloadToArray -Payload $workspaceList.Json)

        if ($workspaces.Count -eq 1) {
            $resolvedWorkspace = [string]$workspaces[0].name
        }
        elseif ($workspaces.Count -gt 1) {
            $workspaceCandidates = [System.Collections.Generic.List[string]]::new()
            foreach ($workspace in $workspaces) {
                if ($null -ne $workspace.name -and -not [string]::IsNullOrWhiteSpace([string]$workspace.name)) {
                    $null = $workspaceCandidates.Add([string]$workspace.name)
                }
            }
            throw "Multiple Chaos Studio workspaces found. Provide -ChaosWorkspaceName. Candidates: $($workspaceCandidates -join ', ')."
        }
        else {
            throw "No Chaos Studio workspace found in resource group '$ResourceGroupName'."
        }
    }

    $scenarioShow = Invoke-AzCommand -StageName $StageName -CommandName 'chaos-scenario-show' -Arguments @('chaos', 'scenario', 'show', '--resource-group', $ResourceGroupName, '--workspace-name', $resolvedWorkspace, '--scenario-name', $ChaosScenarioName) -ExpectJson
    if (-not $scenarioShow.Result.succeeded) {
        throw "Chaos scenario '$ChaosScenarioName' was not found in workspace '$resolvedWorkspace'."
    }

    $configList = Invoke-AzCommand -StageName $StageName -CommandName 'chaos-scenario-config-list' -Arguments @('chaos', 'scenario', 'config', 'list', '--resource-group', $ResourceGroupName, '--workspace-name', $resolvedWorkspace, '--scenario-name', $ChaosScenarioName) -ExpectJson
    if (-not $configList.Result.succeeded) {
        throw "Unable to list scenario configs for '$ChaosScenarioName'."
    }

    $configs = @(Convert-AzListPayloadToArray -Payload $configList.Json)

    $resolvedConfigName = $ChaosScenarioConfigName
    if ([string]::IsNullOrWhiteSpace($resolvedConfigName)) {
        if ($configs.Count -eq 1) {
            $resolvedConfigName = [string]$configs[0].name
        }
        else {
            $defaultConfig = $configs | Where-Object { [string]::Equals([string]$_.name, 'default', [System.StringComparison]::OrdinalIgnoreCase) } | Select-Object -First 1
            if ($null -ne $defaultConfig) {
                $resolvedConfigName = [string]$defaultConfig.name
            }
        }
    }

    if ([string]::IsNullOrWhiteSpace($resolvedConfigName)) {
        $availableConfigs = [System.Collections.Generic.List[string]]::new()
        foreach ($config in $configs) {
            if ($null -ne $config.name -and -not [string]::IsNullOrWhiteSpace([string]$config.name)) {
                $null = $availableConfigs.Add([string]$config.name)
            }
        }

        if ($availableConfigs.Count -eq 0) {
            throw "No scenario configuration found for scenario '$ChaosScenarioName'."
        }

        throw "Multiple scenario configurations found. Provide -ChaosScenarioConfigName. Candidates: $($availableConfigs -join ', ')."
    }

    return [ordered]@{
        workspaceName = $resolvedWorkspace
        scenarioName  = $ChaosScenarioName
        configName    = $resolvedConfigName
        helpCommands  = $helpResults
    }
}

function Resolve-ChaosRunId {
    param(
        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$StageName,

        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$WorkspaceName,

        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$ScenarioName
    )

    $runList = Invoke-AzCommand -StageName $StageName -CommandName 'chaos-scenario-run-list' -Arguments @('chaos', 'scenario', 'run', 'list', '--resource-group', $ResourceGroupName, '--workspace-name', $WorkspaceName, '--scenario-name', $ScenarioName) -ExpectJson
    if (-not $runList.Result.succeeded) {
        throw 'Unable to list scenario runs after start.'
    }

    $runs = @(Convert-AzListPayloadToArray -Payload $runList.Json)

    if ($runs.Count -eq 0) {
        throw 'No scenario run was returned after start.'
    }

    $sortedRuns = $runs | Sort-Object -Property @{
            Expression = {
                $systemDataProperty = $_.PSObject.Properties['systemData']
                if ($null -eq $systemDataProperty -or $null -eq $systemDataProperty.Value) {
                    return [DateTime]::MinValue
                }

                $createdAtProperty = $systemDataProperty.Value.PSObject.Properties['createdAt']
                if ($null -eq $createdAtProperty -or $null -eq $createdAtProperty.Value) {
                    return [DateTime]::MinValue
                }

                try {
                    return [DateTime]::Parse([string]$createdAtProperty.Value)
                }
                catch {
                    return [DateTime]::MinValue
                }
            }
            Descending = $true
        }

    $latestRun = $sortedRuns | Select-Object -First 1
    if ($null -eq $latestRun) {
        throw 'Unable to resolve the latest scenario run.'
    }

    if ($null -ne $latestRun.name -and -not [string]::IsNullOrWhiteSpace([string]$latestRun.name)) {
        return [ordered]@{
            runId       = [string]$latestRun.name
            listCommand = $runList.Result
        }
    }

    throw 'Unable to resolve scenario run identifier from run list.'
}
$stageOrder = @('baseline', 'soak', 'chaos', 'collect')
$stageResults = [System.Collections.Generic.List[object]]::new()
$chaosRunId = $null
$chaosWorkspaceNameResolved = $null
$chaosScenarioNameResolved = $null
$chaosConfigNameResolved = $null

foreach ($stageName in $stageOrder) {
    if (-not $selectedStages.Contains($stageName)) {
        $stageResults.Add([ordered]@{
                stageName     = $stageName
                status        = 'NotSelected'
                startedAtUtc  = $null
                finishedAtUtc = $null
                details       = [ordered]@{ reason = 'Stage not selected.' }
            })
        continue
    }

    $stageStarted = Get-Date
    $status = 'Succeeded'
    $details = [ordered]@{}

    try {
        switch ($stageName) {
            'baseline' {
                $baselineHealth = Invoke-HealthCheck -Uri "$($ApiBaseUrl.TrimEnd('/'))/health"
                $details.healthCheck = $baselineHealth

                $baselineCommands = [System.Collections.Generic.List[object]]::new()
                $baselineCommands.Add((Invoke-AzCommand -StageName $stageName -CommandName 'account-show' -Arguments @('account', 'show') -ExpectJson).Result)
                $baselineCommands.Add((Invoke-AzCommand -StageName $stageName -CommandName 'containerapp-api-show' -Arguments @('containerapp', 'show', '--name', $ContainerAppApiName, '--resource-group', $ResourceGroupName) -ExpectJson).Result)
                $baselineCommands.Add((Invoke-AzCommand -StageName $stageName -CommandName 'containerapp-worker-show' -Arguments @('containerapp', 'show', '--name', $ContainerAppWorkerName, '--resource-group', $ResourceGroupName) -ExpectJson).Result)
                $baselineCommands.Add((Invoke-AzCommand -StageName $stageName -CommandName 'containerapp-scheduler-show' -Arguments @('containerapp', 'show', '--name', $ContainerAppSchedulerName, '--resource-group', $ResourceGroupName) -ExpectJson).Result)
                $baselineCommands.Add((Invoke-AzCommand -StageName $stageName -CommandName 'servicebus-queue-show' -Arguments @('servicebus', 'queue', 'show', '--name', $ServiceBusQueueName, '--namespace-name', $ServiceBusNamespaceName, '--resource-group', $ResourceGroupName) -ExpectJson).Result)
                $details.commands = $baselineCommands

                $hasCommandFailure = $baselineCommands.Exists({ param($entry) -not $entry.succeeded })
                if (-not $baselineHealth.succeeded -or $hasCommandFailure) {
                    $status = 'Failed'
                }

                Write-JsonArtifact -InputObject $details -Path (Get-StagePath -StageName $stageName -FileName 'baseline.json')
            }

            'soak' {
                $soakSamples = [System.Collections.Generic.List[object]]::new()
                $targetEnd = (Get-Date).AddMinutes($SoakDurationMinutes)
                $sampleIndex = 0

                while ((Get-Date) -lt $targetEnd) {
                    $sampleIndex += 1
                    $sample = Invoke-HealthCheck -Uri "$($ApiBaseUrl.TrimEnd('/'))/health"
                    $sample.sequence = $sampleIndex
                    $sample.timestampUtc = (Get-Date).ToUniversalTime().ToString('o')
                    $soakSamples.Add($sample)

                    if (-not $sample.succeeded -or $sample.statusCode -ne 200) {
                        $status = 'Failed'
                    }

                    Start-Sleep -Seconds $SoakIntervalSeconds
                }

                $details.durationMinutes = $SoakDurationMinutes
                $details.intervalSeconds = $SoakIntervalSeconds
                $details.sampleCount = $soakSamples.Count
                $details.samples = $soakSamples
                Write-JsonArtifact -InputObject $details -Path (Get-StagePath -StageName $stageName -FileName 'soak.json')

                $csvRows = foreach ($row in $soakSamples) {
                    [pscustomobject]@{
                        sequence      = $row.sequence
                        timestampUtc  = $row.timestampUtc
                        succeeded     = $row.succeeded
                        statusCode    = $row.statusCode
                        durationMs    = $row.durationMs
                        error         = $row.error
                    }
                }
                $csvRows | Export-Csv -Path (Get-StagePath -StageName $stageName -FileName 'soak.csv') -NoTypeInformation -Encoding utf8
            }

            'chaos' {
                if (-not $Apply.IsPresent) {
                    $status = 'Skipped'
                    $details.reason = 'Chaos stage requires -Apply for unattended mutation.'
                    Write-JsonArtifact -InputObject $details -Path (Get-StagePath -StageName $stageName -FileName 'chaos.json')
                    break
                }

                $chaosScenarioAvailable = $true
                try {
                    $chaosContext = Resolve-ChaosScenarioContext -StageName $stageName
                    $chaosWorkspaceNameResolved = [string]$chaosContext.workspaceName
                    $chaosScenarioNameResolved = [string]$chaosContext.scenarioName
                    $chaosConfigNameResolved = [string]$chaosContext.configName
                    $details.chaosContext = $chaosContext
                }
                catch {
                    $chaosResolutionError = $_.Exception.Message
                    if ($RunPostgresFault.IsPresent -and $chaosResolutionError -like "No Chaos Studio workspace found*") {
                        $chaosScenarioAvailable = $false
                        $details.chaosContext = [ordered]@{
                            skipped = $true
                            reason  = $chaosResolutionError
                        }
                    }
                    else {
                        throw
                    }
                }

                if ($chaosScenarioAvailable) {
                    $startResult = Invoke-AzCommand -StageName $stageName -CommandName 'chaos-scenario-run-start' -Arguments @(
                        'chaos', 'scenario', 'run', 'start',
                        '--resource-group', $ResourceGroupName,
                        '--workspace-name', $chaosWorkspaceNameResolved,
                        '--scenario-name', $chaosScenarioNameResolved,
                        '--config-name', $chaosConfigNameResolved,
                        '--no-wait'
                    ) -ExpectJson
                    $details.startCommand = $startResult.Result

                    if (-not $startResult.Result.succeeded) {
                        $status = 'Failed'
                        Write-JsonArtifact -InputObject $details -Path (Get-StagePath -StageName $stageName -FileName 'chaos.json')
                        break
                    }

                    $runContext = Resolve-ChaosRunId -StageName $stageName -WorkspaceName $chaosWorkspaceNameResolved -ScenarioName $chaosScenarioNameResolved
                    $chaosRunId = [string]$runContext.runId
                    $details.runListCommand = $runContext.listCommand

                    if ([string]::IsNullOrWhiteSpace($chaosRunId)) {
                        $status = 'Failed'
                        $details.error = 'Could not resolve chaos scenario run ID from run list.'
                        Write-JsonArtifact -InputObject $details -Path (Get-StagePath -StageName $stageName -FileName 'chaos.json')
                        break
                    }

                    $details.runId = $chaosRunId

                    $terminalStates = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
                    foreach ($terminalState in @('Succeeded', 'Failed', 'Cancelled', 'Canceled', 'Stopped')) {
                        $null = $terminalStates.Add($terminalState)
                    }

                    $pollHistory = [System.Collections.Generic.List[object]]::new()
                    $pollDeadline = (Get-Date).AddMinutes($ChaosTimeoutMinutes)
                    $currentState = 'Running'
                    while ((Get-Date) -lt $pollDeadline) {
                        $showResult = Invoke-AzCommand -StageName $stageName -CommandName 'chaos-scenario-run-show' -Arguments @(
                            'chaos', 'scenario', 'run', 'show',
                            '--resource-group', $ResourceGroupName,
                            '--workspace-name', $chaosWorkspaceNameResolved,
                            '--scenario-name', $chaosScenarioNameResolved,
                            '--run-id', $chaosRunId
                        ) -ExpectJson
                        $pollHistory.Add($showResult.Result)

                        if (-not $showResult.Result.succeeded) {
                            $status = 'Failed'
                            $currentState = 'CommandFailure'
                            break
                        }

                        $candidateState = $null
                        if ($null -ne $showResult.Json) {
                            if ($null -ne $showResult.Json.properties -and $null -ne $showResult.Json.properties.status) {
                                $candidateState = [string]$showResult.Json.properties.status
                            }
                            elseif ($null -ne $showResult.Json.status) {
                                $candidateState = [string]$showResult.Json.status
                            }
                            elseif ($null -ne $showResult.Json.properties -and $null -ne $showResult.Json.properties.provisioningState) {
                                $candidateState = [string]$showResult.Json.properties.provisioningState
                            }
                            elseif ($null -ne $showResult.Json.provisioningState) {
                                $candidateState = [string]$showResult.Json.provisioningState
                            }
                        }

                        if (-not [string]::IsNullOrWhiteSpace($candidateState)) {
                            $currentState = $candidateState
                            if ($terminalStates.Contains($currentState)) {
                                break
                            }
                        }

                        Start-Sleep -Seconds $ChaosPollIntervalSeconds
                    }

                    $details.pollHistory = $pollHistory
                    $details.finalState = $currentState

                    if (-not $terminalStates.Contains($currentState)) {
                        $status = 'Failed'
                        $details.error = "Chaos execution did not reach a terminal state before timeout (${ChaosTimeoutMinutes}m)."
                    }
                    elseif (-not [string]::Equals($currentState, 'Succeeded', [System.StringComparison]::OrdinalIgnoreCase)) {
                        $status = 'Failed'
                    }
                }

                if ($RunPostgresFault.IsPresent) {
                    if (-not (Test-Path -LiteralPath $PostgresFaultScriptPath -PathType Leaf)) {
                        $status = 'Failed'
                        $details.postgresFault = [ordered]@{
                            enabled = $true
                            error   = "PostgreSQL fault script not found: $PostgresFaultScriptPath"
                        }
                    }
                    else {
                        $postgresArguments = [System.Collections.Generic.List[string]]::new()
                        foreach ($item in @('-File', $PostgresFaultScriptPath, '-ResourceGroupName', $ResourceGroupName, '-ServerName', $PostgresServerName, '-FaultMode', $PostgresFaultMode, '-TimeoutMinutes', [string]$PostgresFaultTimeoutMinutes, '-PollIntervalSeconds', [string]$PostgresFaultPollIntervalSeconds, '-OutputRoot', (Join-Path $runDirectory -ChildPath 'postgres-chaos'))) {
                            $null = $postgresArguments.Add($item)
                        }

                        if ($PSBoundParameters.ContainsKey('SubscriptionId')) {
                            $null = $postgresArguments.Add('-SubscriptionId')
                            $null = $postgresArguments.Add($SubscriptionId)
                        }

                        if ($Apply.IsPresent) {
                            $null = $postgresArguments.Add('-Apply')
                        }

                        $postgresLogPath = Get-StagePath -StageName $stageName -FileName 'postgres-fault.log'
                        $postgresOutput = & pwsh -NoProfile @postgresArguments 2>&1
                        $postgresExitCode = $LASTEXITCODE
                        Write-TextArtifact -Path $postgresLogPath -Content (($postgresOutput | ForEach-Object { "$_" }) -join [Environment]::NewLine)

                        $details.postgresFault = [ordered]@{
                            enabled      = $true
                            scriptPath   = $PostgresFaultScriptPath
                            serverName   = $PostgresServerName
                            faultMode    = $PostgresFaultMode
                            exitCode     = $postgresExitCode
                            outputLog    = $postgresLogPath
                            outputFolder = (Join-Path $runDirectory -ChildPath 'postgres-chaos')
                        }

                        if ($postgresExitCode -ne 0) {
                            $status = 'Failed'
                        }
                    }
                }

                Write-JsonArtifact -InputObject $details -Path (Get-StagePath -StageName $stageName -FileName 'chaos.json')
            }

            'collect' {
                $collectHealth = Invoke-HealthCheck -Uri "$($ApiBaseUrl.TrimEnd('/'))/health"
                $details.healthCheck = $collectHealth

                $collectCommands = [System.Collections.Generic.List[object]]::new()
                if (-not [string]::IsNullOrWhiteSpace($chaosWorkspaceNameResolved) -and -not [string]::IsNullOrWhiteSpace($chaosScenarioNameResolved)) {
                    $collectCommands.Add((Invoke-AzCommand -StageName $stageName -CommandName 'chaos-scenario-run-list-collect' -Arguments @('chaos', 'scenario', 'run', 'list', '--resource-group', $ResourceGroupName, '--workspace-name', $chaosWorkspaceNameResolved, '--scenario-name', $chaosScenarioNameResolved) -ExpectJson).Result)
                }
                $collectCommands.Add((Invoke-AzCommand -StageName $stageName -CommandName 'servicebus-queue-show-collect' -Arguments @('servicebus', 'queue', 'show', '--name', $ServiceBusQueueName, '--namespace-name', $ServiceBusNamespaceName, '--resource-group', $ResourceGroupName) -ExpectJson).Result)
                $collectCommands.Add((Invoke-AzCommand -StageName $stageName -CommandName 'containerapp-api-show-collect' -Arguments @('containerapp', 'show', '--name', $ContainerAppApiName, '--resource-group', $ResourceGroupName) -ExpectJson).Result)
                $collectCommands.Add((Invoke-AzCommand -StageName $stageName -CommandName 'containerapp-worker-show-collect' -Arguments @('containerapp', 'show', '--name', $ContainerAppWorkerName, '--resource-group', $ResourceGroupName) -ExpectJson).Result)
                $collectCommands.Add((Invoke-AzCommand -StageName $stageName -CommandName 'containerapp-scheduler-show-collect' -Arguments @('containerapp', 'show', '--name', $ContainerAppSchedulerName, '--resource-group', $ResourceGroupName) -ExpectJson).Result)
                $collectCommands.Add((Invoke-AzCommand -StageName $stageName -CommandName 'activity-log-list' -Arguments @('monitor', 'activity-log', 'list', '--resource-group', $ResourceGroupName, '--offset', "${CollectActivityLogLookbackHours}h") -ExpectJson).Result)
                $details.commands = $collectCommands
                if ($chaosRunId) {
                    $details.chaosRunId = $chaosRunId
                }
                if ($chaosWorkspaceNameResolved) {
                    $details.chaosWorkspaceName = $chaosWorkspaceNameResolved
                }
                if ($chaosScenarioNameResolved) {
                    $details.chaosScenarioName = $chaosScenarioNameResolved
                }

                $hasCommandFailure = $collectCommands.Exists({ param($entry) -not $entry.succeeded })
                if (-not $collectHealth.succeeded -or $hasCommandFailure) {
                    $status = 'Failed'
                }

                Write-JsonArtifact -InputObject $details -Path (Get-StagePath -StageName $stageName -FileName 'collect.json')
            }
        }
    }
    catch {
        $status = 'Failed'
        $details.error = $_.Exception.Message
        Write-JsonArtifact -InputObject $details -Path (Get-StagePath -StageName $stageName -FileName "$stageName-error.json")
    }

    $stageFinished = Get-Date
    $stageResults.Add([ordered]@{
            stageName     = $stageName
            status        = $status
            startedAtUtc  = $stageStarted.ToUniversalTime().ToString('o')
            finishedAtUtc = $stageFinished.ToUniversalTime().ToString('o')
            durationMs    = [int]([System.Math]::Round(($stageFinished - $stageStarted).TotalMilliseconds))
            details       = $details
        })
}

$runFinished = Get-Date
$hasFailedStage = $false
foreach ($entry in $stageResults) {
    if ([string]::Equals([string]$entry.status, 'Failed', [System.StringComparison]::OrdinalIgnoreCase)) {
        $hasFailedStage = $true
        break
    }
}

$overallStatus = if ($hasFailedStage) { 'Failed' } else { 'Succeeded' }

$summary = [ordered]@{
    runId          = $runId
    startedAtUtc   = $runTimestamp.ToUniversalTime().ToString('o')
    finishedAtUtc  = $runFinished.ToUniversalTime().ToString('o')
    durationMs     = [int]([System.Math]::Round(($runFinished - $runTimestamp).TotalMilliseconds))
    overallStatus  = $overallStatus
    selectedStages = @($normalizedStages)
    apply          = $Apply.IsPresent
    metadata       = $metadata
    stages         = $stageResults
}

$metadataPath = Join-Path $runDirectory -ChildPath 'run-metadata.json'
$summaryPath = Join-Path $runDirectory -ChildPath 'summary.json'
$summaryTextPath = Join-Path $runDirectory -ChildPath 'summary.md'

Write-JsonArtifact -InputObject $metadata -Path $metadataPath
Write-JsonArtifact -InputObject $summary -Path $summaryPath

$summaryLines = [System.Collections.Generic.List[string]]::new()
$summaryLines.Add('# Operational proof run summary')
$summaryLines.Add('')
$summaryLines.Add("Run ID: $runId")
$summaryLines.Add("Overall status: $overallStatus")
$summaryLines.Add("Started (UTC): $($summary.startedAtUtc)")
$summaryLines.Add("Finished (UTC): $($summary.finishedAtUtc)")
$summaryLines.Add("Output directory: $runDirectory")
$summaryLines.Add('')
$summaryLines.Add('Stage results:')
$summaryLines.Add('')
foreach ($stage in $stageResults) {
    $summaryLines.Add("- $($stage.stageName): $($stage.status)")
}
$summaryLines.Add('')
$summaryLines.Add('Machine-readable outputs:')
$summaryLines.Add("- $metadataPath")
$summaryLines.Add("- $summaryPath")

Write-TextArtifact -Path $summaryTextPath -Content ($summaryLines -join [Environment]::NewLine)

Write-Information "Operational proof run completed with status '$overallStatus'."
Write-Information "Artifacts: $runDirectory"

if ($hasFailedStage) {
    exit 1
}

exit 0
