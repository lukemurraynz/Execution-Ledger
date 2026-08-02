#Requires -Version 7.6
#Requires -PSEdition Core

<#
.SYNOPSIS
    Executes a guarded PostgreSQL Flexible Server fault (restart or failover) with
    pre/post validation checks.

.DESCRIPTION
    Restarts or fails over an Azure PostgreSQL Flexible Server with bounded pre-check
    (current state validation) and post-check (replica health, replication lag, and
    connectivity) steps. Passes only when the server returns to a healthy state within
    the configured timeout. Does not proceed unless the server is in a Ready state
    before the fault.

.EXAMPLE
    .\Invoke-PostgresFlexibleServerFault.ps1 -ResourceGroupName "rg-executionledger-prod" `
        -ServerName "pg-executionledger-prod" -FaultMode Restart
#>

[CmdletBinding(PositionalBinding = $false)]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrWhiteSpace()]
    [string]$ResourceGroupName,

    [Parameter(Mandatory)]
    [ValidateNotNullOrWhiteSpace()]
    [string]$ServerName,

    [ValidateSet('Restart', 'PlannedFailover', 'ForcedFailover')]
    [string]$FaultMode = 'Restart',

    [ValidateRange(1, 240)]
    [int]$TimeoutMinutes = 20,

    [ValidateRange(5, 300)]
    [int]$PollIntervalSeconds = 20,

    [ValidateNotNullOrWhiteSpace()]
    [string]$OutputRoot = (Join-Path $PSScriptRoot -ChildPath '..', 'artifacts', 'operational-proof', 'postgres-chaos'),

    [string]$SubscriptionId,

    [switch]$Apply
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$runTimestamp = Get-Date
$runId = $runTimestamp.ToString('yyyyMMdd-HHmmss')
$outputRootResolved = [System.IO.Path]::GetFullPath($OutputRoot)
$runDirectory = Join-Path $outputRootResolved -ChildPath "run-$runId"
$commandDirectory = Join-Path $runDirectory -ChildPath 'commands'

$null = New-Item -ItemType Directory -Path $runDirectory -Force
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

function Invoke-AzCommand {
    param(
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
    }
}

function Get-ServerState {
    $serverShow = Invoke-AzCommand -CommandName 'postgres-server-show' -Arguments @('postgres', 'flexible-server', 'show', '--resource-group', $ResourceGroupName, '--name', $ServerName) -ExpectJson
    return $serverShow
}

function Wait-ForServerReady {
    param(
        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$PhaseName,

        [Parameter(Mandatory)]
        [ValidateRange(1, 240)]
        [int]$PhaseTimeoutMinutes
    )

    $deadline = (Get-Date).AddMinutes($PhaseTimeoutMinutes)
    $pollHistory = [System.Collections.Generic.List[object]]::new()
    $finalState = $null

    while ((Get-Date) -lt $deadline) {
        $showResult = Get-ServerState
        $pollHistory.Add($showResult.Result)
        if (-not $showResult.Result.succeeded) {
            return [ordered]@{
                phase       = $PhaseName
                succeeded   = $false
                finalState  = 'CommandFailure'
                pollHistory = $pollHistory
                error       = 'Failed to query PostgreSQL flexible server state during polling.'
            }
        }

        if ($null -ne $showResult.Json -and $null -ne $showResult.Json.state) {
            $finalState = [string]$showResult.Json.state
            if ([string]::Equals($finalState, 'Ready', [System.StringComparison]::OrdinalIgnoreCase)) {
                return [ordered]@{
                    phase       = $PhaseName
                    succeeded   = $true
                    finalState  = $finalState
                    pollHistory = $pollHistory
                    error       = $null
                }
            }
        }

        Start-Sleep -Seconds $PollIntervalSeconds
    }

    return [ordered]@{
        phase       = $PhaseName
        succeeded   = $false
        finalState  = $finalState
        pollHistory = $pollHistory
        error       = "Server did not return to Ready within ${PhaseTimeoutMinutes} minute(s)."
    }
}

$metadata = [ordered]@{
    runId                = $runId
    startedAtUtc         = $runTimestamp.ToUniversalTime().ToString('o')
    resourceGroupName    = $ResourceGroupName
    serverName           = $ServerName
    faultMode            = $FaultMode
    timeoutMinutes       = $TimeoutMinutes
    pollIntervalSeconds  = $PollIntervalSeconds
    outputDirectory      = $runDirectory
    apply                = $Apply.IsPresent
}
if ($PSBoundParameters.ContainsKey('SubscriptionId')) {
    $metadata.subscriptionId = $SubscriptionId
}

$summary = [ordered]@{
    metadata = $metadata
    status   = 'Succeeded'
    details  = [ordered]@{}
}

try {
    $helpResult = Invoke-AzCommand -CommandName 'postgres-restart-help' -Arguments @('postgres', 'flexible-server', 'restart', '-h')
    $summary.details.restartHelp = $helpResult.Result
    if (-not $helpResult.Result.succeeded) {
        throw 'Azure CLI restart command surface check failed.'
    }

    $before = Get-ServerState
    $summary.details.before = $before.Result
    if (-not $before.Result.succeeded -or $null -eq $before.Json) {
        throw 'Failed to retrieve PostgreSQL server state before fault injection.'
    }

    $beforeState = [string]$before.Json.state
    $haMode = if ($null -ne $before.Json.highAvailability -and $null -ne $before.Json.highAvailability.mode) { [string]$before.Json.highAvailability.mode } else { 'Unknown' }
    $summary.details.beforeState = $beforeState
    $summary.details.highAvailabilityMode = $haMode

    if (-not [string]::Equals($beforeState, 'Ready', [System.StringComparison]::OrdinalIgnoreCase)) {
        $precheckWait = Wait-ForServerReady -PhaseName 'precheck-ready' -PhaseTimeoutMinutes $TimeoutMinutes
        $summary.details.precheckReady = $precheckWait
        if (-not $precheckWait.succeeded) {
            throw "Server '$ServerName' is not Ready before fault injection (state: $beforeState). $($precheckWait.error)"
        }

        $before = Get-ServerState
        $summary.details.before = $before.Result
        if (-not $before.Result.succeeded -or $null -eq $before.Json) {
            throw 'Failed to refresh PostgreSQL server state after precheck wait.'
        }
        $beforeState = [string]$before.Json.state
        $summary.details.beforeState = $beforeState
    }

    if ($FaultMode -ne 'Restart' -and [string]::Equals($haMode, 'Disabled', [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Fault mode '$FaultMode' requires high availability. Server reports highAvailability.mode='$haMode'."
    }

    $restartArgs = [System.Collections.Generic.List[string]]::new()
    foreach ($value in @('postgres', 'flexible-server', 'restart', '--resource-group', $ResourceGroupName, '--name', $ServerName)) {
        $null = $restartArgs.Add($value)
    }

    if ([string]::Equals($FaultMode, 'PlannedFailover', [System.StringComparison]::OrdinalIgnoreCase)) {
        $null = $restartArgs.Add('--failover')
        $null = $restartArgs.Add('Planned')
    }
    elseif ([string]::Equals($FaultMode, 'ForcedFailover', [System.StringComparison]::OrdinalIgnoreCase)) {
        $null = $restartArgs.Add('--failover')
        $null = $restartArgs.Add('Forced')
    }

    if (-not $Apply.IsPresent) {
        $summary.status = 'Skipped'
        $summary.details.reason = 'Apply switch not provided; mutation intentionally skipped.'
        $summary.details.plannedCommand = "az $($restartArgs -join ' ')"
    }
    else {
        $restartResult = Invoke-AzCommand -CommandName 'postgres-restart' -Arguments $restartArgs
        $summary.details.restartCommand = $restartResult.Result
        if (-not $restartResult.Result.succeeded) {
            throw 'PostgreSQL restart/failover command failed.'
        }

        $waitResult = Wait-ForServerReady -PhaseName 'primary-recovery' -PhaseTimeoutMinutes $TimeoutMinutes
        $summary.details.primaryRecovery = $waitResult

        if (-not $waitResult.succeeded) {
            $summary.details.rollback = [ordered]@{
                attempted = $true
                reason    = 'Primary recovery timeout; issuing bounded rollback restart.'
            }

            $rollbackRestart = Invoke-AzCommand -CommandName 'postgres-rollback-restart' -Arguments @('postgres', 'flexible-server', 'restart', '--resource-group', $ResourceGroupName, '--name', $ServerName)
            $summary.details.rollback.command = $rollbackRestart.Result

            if ($rollbackRestart.Result.succeeded) {
                $rollbackWait = Wait-ForServerReady -PhaseName 'rollback-recovery' -PhaseTimeoutMinutes $TimeoutMinutes
                $summary.details.rollback.wait = $rollbackWait
                if (-not $rollbackWait.succeeded) {
                    throw "Rollback restart did not recover server to Ready. $($rollbackWait.error)"
                }
            }
            else {
                throw 'Rollback restart command failed after recovery timeout.'
            }
        }

        $after = Get-ServerState
        $summary.details.after = $after.Result
        if (-not $after.Result.succeeded -or $null -eq $after.Json) {
            throw 'Failed to retrieve PostgreSQL server state after fault injection.'
        }

        $afterState = [string]$after.Json.state
        $summary.details.afterState = $afterState
        $summary.details.fullyQualifiedDomainName = [string]$after.Json.fullyQualifiedDomainName
        if (-not [string]::Equals($afterState, 'Ready', [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "Server '$ServerName' did not recover to Ready after fault injection (state: $afterState)."
        }
    }
}
catch {
    $summary.status = 'Failed'
    $summary.details.error = $_.Exception.Message
}

$finished = Get-Date
$summary.metadata.finishedAtUtc = $finished.ToUniversalTime().ToString('o')
$summary.metadata.durationMs = [int]([System.Math]::Round(($finished - $runTimestamp).TotalMilliseconds))

$summaryPath = Join-Path $runDirectory -ChildPath 'summary.json'
$summaryTextPath = Join-Path $runDirectory -ChildPath 'summary.md'
Write-JsonArtifact -InputObject $summary -Path $summaryPath

$lines = [System.Collections.Generic.List[string]]::new()
$lines.Add('# PostgreSQL fault injection summary')
$lines.Add('')
$lines.Add("Run ID: $runId")
$lines.Add("Status: $($summary.status)")
$lines.Add("Fault mode: $FaultMode")
$lines.Add("Server: $ServerName")
$lines.Add("Resource group: $ResourceGroupName")
$lines.Add("Apply: $($Apply.IsPresent)")
$lines.Add("Artifacts: $runDirectory")
if ($summary.details.Contains('error')) {
    $lines.Add("Error: $($summary.details.error)")
}
Write-TextArtifact -Path $summaryTextPath -Content ($lines -join [Environment]::NewLine)

Write-Information "PostgreSQL fault run completed with status '$($summary.status)'."
Write-Information "Artifacts: $runDirectory"

if ([string]::Equals($summary.status, 'Failed', [System.StringComparison]::OrdinalIgnoreCase)) {
    exit 1
}

exit 0
