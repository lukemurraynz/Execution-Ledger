#Requires -Version 7.6
#Requires -PSEdition Core

[CmdletBinding(PositionalBinding = $false)]
param(
    [ValidateNotNullOrWhiteSpace()]
    [string]$BaselinePath = (Join-Path $PSScriptRoot -ChildPath '..', 'artifacts', 'operational-proof', 'baseline-latest.json'),

    [ValidateNotNullOrWhiteSpace()]
    [string]$OutputDirectory = (Join-Path $PSScriptRoot -ChildPath '..', 'artifacts', 'operational-proof', 'dr-integrity-pass'),

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

    [Parameter(Mandatory)]
    [ValidateNotNullOrWhiteSpace()]
    [string]$PostgresServerName,

    [string]$ChaosWorkspaceName,

    [ValidateNotNullOrWhiteSpace()]
    [string]$ChaosScenarioName = 'MessagingDisruption-1.0',

    [string]$ChaosScenarioConfigName,

    [ValidateNotNullOrWhiteSpace()]
    [string]$OperationalProofScriptPath = (Join-Path $PSScriptRoot -ChildPath 'Invoke-OperationalProofRun.ps1'),

    [ValidateNotNullOrWhiteSpace()]
    [string]$PostgresFaultScriptPath = (Join-Path $PSScriptRoot -ChildPath 'Invoke-PostgresFlexibleServerFault.ps1'),

    [string]$SubscriptionId,

    [switch]$Apply
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$runTimestamp = Get-Date
$resolvedOutputDirectory = [System.IO.Path]::GetFullPath($OutputDirectory)
$rawDirectory = Join-Path $resolvedOutputDirectory -ChildPath 'raw'
$checksDirectory = Join-Path $resolvedOutputDirectory -ChildPath 'checks'
$commandsDirectory = Join-Path $resolvedOutputDirectory -ChildPath 'commands'
$harnessOutputDirectory = Join-Path $resolvedOutputDirectory -ChildPath 'harness-output'
$postgresOutputDirectory = Join-Path $resolvedOutputDirectory -ChildPath 'postgres-fault'

if (Test-Path -LiteralPath $resolvedOutputDirectory) {
    Remove-Item -LiteralPath $resolvedOutputDirectory -Recurse -Force
}

$null = New-Item -ItemType Directory -Path $rawDirectory -Force
$null = New-Item -ItemType Directory -Path $checksDirectory -Force
$null = New-Item -ItemType Directory -Path $commandsDirectory -Force
$null = New-Item -ItemType Directory -Path $harnessOutputDirectory -Force
$null = New-Item -ItemType Directory -Path $postgresOutputDirectory -Force

function Write-JsonArtifact {
    param(
        [Parameter(Mandatory)]
        [ValidateNotNull()]
        [object]$InputObject,

        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$Path
    )

    $json = $InputObject | ConvertTo-Json -Depth 64
    Set-Content -LiteralPath $Path -Value $json -Encoding utf8
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

    Set-Content -LiteralPath $Path -Value $Content -Encoding utf8
}

function Read-JsonArtifact {
    param(
        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$Path
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Required JSON artifact not found: $Path"
    }

    $text = Get-Content -LiteralPath $Path -Raw -Encoding utf8
    return $text | ConvertFrom-Json -Depth 128
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
    $baseFileName = '{0}-{1}' -f (Get-Date -Format 'yyyyMMddHHmmssfff'), $CommandName
    $logPath = Join-Path $commandsDirectory -ChildPath "$baseFileName.log"
    Write-TextArtifact -Path $logPath -Content $rawText

    $jsonPath = $null
    if ($null -ne $parsedJson) {
        $jsonPath = Join-Path $commandsDirectory -ChildPath "$baseFileName.json"
        Write-JsonArtifact -InputObject $parsedJson -Path $jsonPath
    }

    return [pscustomobject]@{
        commandName    = $CommandName
        commandText    = "az $($finalArguments -join ' ')"
        startedAtUtc   = $started.ToUniversalTime().ToString('o')
        finishedAtUtc  = $finished.ToUniversalTime().ToString('o')
        durationMs     = [int]([System.Math]::Round(($finished - $started).TotalMilliseconds))
        exitCode       = $exitCode
        succeeded      = ($exitCode -eq 0)
        outputFile     = $logPath
        parsedJsonFile = $jsonPath
        json           = $parsedJson
    }
}

function Resolve-LatestRunDirectory {
    param(
        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$RootPath
    )

    if (-not (Test-Path -LiteralPath $RootPath -PathType Container)) {
        return $null
    }

    $latestDirectory = Get-ChildItem -LiteralPath $RootPath -Directory | Where-Object { $_.Name -like 'run-*' } | Sort-Object -Property LastWriteTimeUtc -Descending | Select-Object -First 1
    if ($null -eq $latestDirectory) {
        return $null
    }

    return $latestDirectory.FullName
}

function Save-Snapshot {
    param(
        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$Suffix
    )

    $records = [ordered]@{}

    $queue = Invoke-AzCommand -CommandName "queue-$Suffix" -Arguments @('servicebus', 'queue', 'show', '--name', $ServiceBusQueueName, '--namespace-name', $ServiceBusNamespaceName, '--resource-group', $ResourceGroupName) -ExpectJson
    $records.queue = $queue
    if ($queue.succeeded -and $null -ne $queue.json) {
        Write-JsonArtifact -InputObject $queue.json -Path (Join-Path $rawDirectory -ChildPath "queue-$Suffix.json")
    }

    $postgres = Invoke-AzCommand -CommandName "postgres-$Suffix" -Arguments @('postgres', 'flexible-server', 'show', '--resource-group', $ResourceGroupName, '--name', $PostgresServerName) -ExpectJson
    $records.postgres = $postgres
    if ($postgres.succeeded -and $null -ne $postgres.json) {
        Write-JsonArtifact -InputObject $postgres.json -Path (Join-Path $rawDirectory -ChildPath "postgres-$Suffix.json")
    }

    $api = Invoke-AzCommand -CommandName "api-show-$Suffix" -Arguments @('containerapp', 'show', '--name', $ContainerAppApiName, '--resource-group', $ResourceGroupName) -ExpectJson
    $records.api = $api
    if ($api.succeeded -and $null -ne $api.json) {
        Write-JsonArtifact -InputObject $api.json -Path (Join-Path $rawDirectory -ChildPath "api-$Suffix.json")
    }

    $worker = Invoke-AzCommand -CommandName "worker-show-$Suffix" -Arguments @('containerapp', 'show', '--name', $ContainerAppWorkerName, '--resource-group', $ResourceGroupName) -ExpectJson
    $records.worker = $worker
    if ($worker.succeeded -and $null -ne $worker.json) {
        Write-JsonArtifact -InputObject $worker.json -Path (Join-Path $rawDirectory -ChildPath "worker-$Suffix.json")
    }

    $scheduler = Invoke-AzCommand -CommandName "scheduler-show-$Suffix" -Arguments @('containerapp', 'show', '--name', $ContainerAppSchedulerName, '--resource-group', $ResourceGroupName) -ExpectJson
    $records.scheduler = $scheduler
    if ($scheduler.succeeded -and $null -ne $scheduler.json) {
        Write-JsonArtifact -InputObject $scheduler.json -Path (Join-Path $rawDirectory -ChildPath "scheduler-$Suffix.json")
    }

    $replicas = Invoke-AzCommand -CommandName "worker-replicas-$Suffix" -Arguments @('containerapp', 'replica', 'list', '--name', $ContainerAppWorkerName, '--resource-group', $ResourceGroupName) -ExpectJson
    $records.workerReplicas = $replicas
    if ($replicas.succeeded -and $null -ne $replicas.json) {
        Write-JsonArtifact -InputObject $replicas.json -Path (Join-Path $rawDirectory -ChildPath "worker-replicas-$Suffix.json")
    }

    foreach ($serviceName in @(
            [ordered]@{ Key = 'api'; AppName = $ContainerAppApiName },
            [ordered]@{ Key = 'worker'; AppName = $ContainerAppWorkerName },
            [ordered]@{ Key = 'scheduler'; AppName = $ContainerAppSchedulerName }
        )) {
        $logResult = Invoke-AzCommand -CommandName "$($serviceName.Key)-logs-$Suffix" -Arguments @('containerapp', 'logs', 'show', '--name', $serviceName.AppName, '--resource-group', $ResourceGroupName, '--tail', '200')
        $records["$($serviceName.Key)Logs"] = $logResult
        $rawLogPath = Join-Path $rawDirectory -ChildPath "$($serviceName.Key)-logs-$Suffix.txt"
        $logText = Get-Content -LiteralPath $logResult.outputFile -Raw -Encoding utf8
        Write-TextArtifact -Path $rawLogPath -Content $logText
    }

    return $records
}

function Convert-InvariantState {
    param(
        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$Status
    )

    return [ordered]@{
        status  = $Status
        pass    = [string]::Equals($Status, 'Pass', [System.StringComparison]::OrdinalIgnoreCase)
        fail    = [string]::Equals($Status, 'Fail', [System.StringComparison]::OrdinalIgnoreCase)
        blocked = [string]::Equals($Status, 'Blocked', [System.StringComparison]::OrdinalIgnoreCase)
    }
}

function Get-RelativeArtifactPath {
    param(
        [Parameter(Mandatory)]
        [ValidateNotNullOrWhiteSpace()]
        [string]$Path
    )

    return [System.IO.Path]::GetRelativePath($resolvedOutputDirectory, $Path).Replace('/', '\')
}

$result = [ordered]@{
    generatedAtUtc = (Get-Date).ToUniversalTime().ToString('o')
    artifactRoot   = $resolvedOutputDirectory
    runMode        = if ($Apply.IsPresent) { 'Apply' } else { 'InspectOnly' }
    baselinePath   = [System.IO.Path]::GetFullPath($BaselinePath)
}

try {
    if (-not (Test-Path -LiteralPath $BaselinePath -PathType Leaf)) {
        throw "Baseline precondition file was not found at '$BaselinePath'."
    }

    if (-not (Test-Path -LiteralPath $OperationalProofScriptPath -PathType Leaf)) {
        throw "Operational proof script was not found at '$OperationalProofScriptPath'."
    }

    if (-not (Test-Path -LiteralPath $PostgresFaultScriptPath -PathType Leaf)) {
        throw "PostgreSQL fault script was not found at '$PostgresFaultScriptPath'."
    }

    $baseline = Read-JsonArtifact -Path $BaselinePath
    $result.baseline = $baseline

    $preconditions = [ordered]@{
        baselineHealthSucceeded = [bool]$baseline.health.succeeded
        apiRunning              = [string]::Equals([string]$baseline.containerApps.api.runningStatus, 'Running', [System.StringComparison]::OrdinalIgnoreCase)
        workerRunning           = [string]::Equals([string]$baseline.containerApps.worker.runningStatus, 'Running', [System.StringComparison]::OrdinalIgnoreCase)
        schedulerRunning        = [string]::Equals([string]$baseline.containerApps.scheduler.runningStatus, 'Running', [System.StringComparison]::OrdinalIgnoreCase)
        queueActive             = [string]::Equals([string]$baseline.serviceBusQueue.status, 'Active', [System.StringComparison]::OrdinalIgnoreCase)
    }
    $result.preconditions = $preconditions

    $preconditionFailed = @($preconditions.Values | Where-Object { -not $_ }).Count -gt 0
    if ($preconditionFailed) {
        throw 'Baseline preconditions failed. Refresh baseline-latest.json before DR integrity run.'
    }

    if (-not $Apply.IsPresent) {
        throw 'DR integrity pass automation requires -Apply to run end-to-end mutations.'
    }

    $preSnapshot = Save-Snapshot -Suffix 'pre'
    $result.snapshotPre = $preSnapshot

    $harnessArguments = [System.Collections.Generic.List[string]]::new()
    foreach ($item in @(
            '-File', $OperationalProofScriptPath,
            '-Stages', 'baseline,chaos,collect',
            '-ResourceGroupName', $ResourceGroupName,
            '-ContainerAppApiName', $ContainerAppApiName,
            '-ContainerAppWorkerName', $ContainerAppWorkerName,
            '-ContainerAppSchedulerName', $ContainerAppSchedulerName,
            '-ServiceBusNamespaceName', $ServiceBusNamespaceName,
            '-ServiceBusQueueName', $ServiceBusQueueName,
            '-ChaosScenarioName', $ChaosScenarioName,
            '-PostgresFaultScriptPath', $PostgresFaultScriptPath,
            '-PostgresServerName', $PostgresServerName,
            '-RunPostgresFault',
            '-OutputRoot', $harnessOutputDirectory,
            '-Apply'
        )) {
        $null = $harnessArguments.Add($item)
    }

    if (-not [string]::IsNullOrWhiteSpace($ChaosWorkspaceName)) {
        $null = $harnessArguments.Add('-ChaosWorkspaceName')
        $null = $harnessArguments.Add($ChaosWorkspaceName)
    }

    if (-not [string]::IsNullOrWhiteSpace($ChaosScenarioConfigName)) {
        $null = $harnessArguments.Add('-ChaosScenarioConfigName')
        $null = $harnessArguments.Add($ChaosScenarioConfigName)
    }

    if (-not [string]::IsNullOrWhiteSpace($SubscriptionId)) {
        $null = $harnessArguments.Add('-SubscriptionId')
        $null = $harnessArguments.Add($SubscriptionId)
    }

    if (-not [string]::IsNullOrWhiteSpace($ApiBaseUrl)) {
        $null = $harnessArguments.Add('-ApiBaseUrl')
        $null = $harnessArguments.Add($ApiBaseUrl)
    }

    $harnessOutput = & pwsh -NoProfile @harnessArguments 2>&1
    $harnessExitCode = $LASTEXITCODE
    $harnessLogPath = Join-Path $commandsDirectory -ChildPath 'invoke-operational-proof.log'
    Write-TextArtifact -Path $harnessLogPath -Content ((($harnessOutput | ForEach-Object { "$_" }) -join [Environment]::NewLine) + [Environment]::NewLine + "EXIT_CODE: $harnessExitCode")

    $harnessRunDirectory = Resolve-LatestRunDirectory -RootPath $harnessOutputDirectory
    if ([string]::IsNullOrWhiteSpace($harnessRunDirectory)) {
        throw 'Operational proof run directory was not produced.'
    }

    $harnessSummaryPath = Join-Path $harnessRunDirectory -ChildPath 'summary.json'
    $harnessSummary = Read-JsonArtifact -Path $harnessSummaryPath
    $harnessSummaryRelativePath = Get-RelativeArtifactPath -Path $harnessSummaryPath
    $result.exercise = [ordered]@{
        harnessExitCode   = $harnessExitCode
        harnessRunPath    = $harnessRunDirectory
        harnessSummary    = $harnessSummaryPath
        harnessOverallStatus = [string]$harnessSummary.overallStatus
    }

    $chaosStage = $harnessSummary.stages | Where-Object { [string]::Equals([string]$_.stageName, 'chaos', [System.StringComparison]::OrdinalIgnoreCase) } | Select-Object -First 1
    $postgresSummaryPath = $null
    $postgresFault = $null
    if ($null -ne $chaosStage -and $null -ne $chaosStage.details) {
        $postgresFaultProperty = $chaosStage.details.PSObject.Properties['postgresFault']
        if ($null -ne $postgresFaultProperty) {
            $postgresFault = $postgresFaultProperty.Value
        }
    }
    if ($null -ne $postgresFault -and $null -ne $postgresFault.outputFolder) {
        $postgresRunDirectory = Resolve-LatestRunDirectory -RootPath ([string]$postgresFault.outputFolder)
        if (-not [string]::IsNullOrWhiteSpace($postgresRunDirectory)) {
            $postgresSummaryPath = Join-Path $postgresRunDirectory -ChildPath 'summary.json'
        }
    }

    if (-not [string]::IsNullOrWhiteSpace($postgresSummaryPath) -and (Test-Path -LiteralPath $postgresSummaryPath -PathType Leaf)) {
        $result.exercise.postgresSummary = $postgresSummaryPath
    }
    $postgresSummaryRelativePath = if (-not [string]::IsNullOrWhiteSpace($postgresSummaryPath) -and (Test-Path -LiteralPath $postgresSummaryPath -PathType Leaf)) {
        Get-RelativeArtifactPath -Path $postgresSummaryPath
    }
    else {
        $null
    }

    $postSnapshot = Save-Snapshot -Suffix 'post'
    $result.snapshotPost = $postSnapshot

    $workerReplicasPath = Join-Path $rawDirectory -ChildPath 'worker-replicas-post.json'
    $workerReplicas = @()
    if (Test-Path -LiteralPath $workerReplicasPath -PathType Leaf) {
        $workerReplicas = @(Read-JsonArtifact -Path $workerReplicasPath)
    }

    $workerRestartCount = 0
    $workerCrashLoopDetected = $false
    $workerReplicaState = 'Unknown'
    $workerReplicaStateDetails = ''
    foreach ($replica in $workerReplicas) {
        if ($null -eq $replica -or $null -eq $replica.properties -or $null -eq $replica.properties.containers) {
            continue
        }

        foreach ($container in $replica.properties.containers) {
            if ($null -eq $container) {
                continue
            }

            $restartCountValue = 0
            if ($null -ne $container.restartCount) {
                $restartCountValue = [int]$container.restartCount
            }
            if ($restartCountValue -gt $workerRestartCount) {
                $workerRestartCount = $restartCountValue
            }

            if ($null -ne $container.runningState -and -not [string]::IsNullOrWhiteSpace([string]$container.runningState)) {
                $workerReplicaState = [string]$container.runningState
            }
            if ($null -ne $container.runningStateDetails -and -not [string]::IsNullOrWhiteSpace([string]$container.runningStateDetails)) {
                $workerReplicaStateDetails = [string]$container.runningStateDetails
            }

            $containerStateText = "$($container.runningState) $($container.runningStateDetails)"
            if ($containerStateText -match 'CrashLoopBackOff') {
                $workerCrashLoopDetected = $true
            }
        }
    }

    $workerLogPostPath = Join-Path $rawDirectory -ChildPath 'worker-logs-post.txt'
    $apiLogPostPath = Join-Path $rawDirectory -ChildPath 'api-logs-post.txt'
    $schedulerLogPostPath = Join-Path $rawDirectory -ChildPath 'scheduler-logs-post.txt'
    $workerLogText = if (Test-Path -LiteralPath $workerLogPostPath -PathType Leaf) { Get-Content -LiteralPath $workerLogPostPath -Raw -Encoding utf8 } else { '' }
    $apiLogText = if (Test-Path -LiteralPath $apiLogPostPath -PathType Leaf) { Get-Content -LiteralPath $apiLogPostPath -Raw -Encoding utf8 } else { '' }
    $schedulerLogText = if (Test-Path -LiteralPath $schedulerLogPostPath -PathType Leaf) { Get-Content -LiteralPath $schedulerLogPostPath -Raw -Encoding utf8 } else { '' }

    $postgresDependencyCrashCount = [regex]::Matches($workerLogText, 'dependency postgres failed').Count
    $apiPostgresDependencyCrashCount = [regex]::Matches($apiLogText, 'dependency postgres failed').Count
    $schedulerPostgresDependencyCrashCount = [regex]::Matches($schedulerLogText, 'dependency postgres failed').Count

    $workerStartupHealth = [ordered]@{
        checkedAtUtc                          = (Get-Date).ToUniversalTime().ToString('o')
        healthy                               = (-not $workerCrashLoopDetected) -and [string]::Equals($workerReplicaState, 'Running', [System.StringComparison]::OrdinalIgnoreCase)
        postgresDependencyCrashCount          = $postgresDependencyCrashCount
        apiPostgresDependencyCrashCount       = $apiPostgresDependencyCrashCount
        schedulerPostgresDependencyCrashCount = $schedulerPostgresDependencyCrashCount
        workerReplicaState                    = $workerReplicaState
        workerReplicaStateDetails             = $workerReplicaStateDetails
        workerRestartCount                    = $workerRestartCount
    }
    Write-JsonArtifact -InputObject $workerStartupHealth -Path (Join-Path $checksDirectory -ChildPath 'worker-startup-health.json')

    $queuePostPath = Join-Path $rawDirectory -ChildPath 'queue-post.json'
    $queuePost = Read-JsonArtifact -Path $queuePostPath
    $queueStatus = [string]$queuePost.status
    $queueActive = if ($null -ne $queuePost.countDetails -and $null -ne $queuePost.countDetails.activeMessageCount) { [int]$queuePost.countDetails.activeMessageCount } else { [int]$queuePost.messageCount }
    $queueDeadLetter = if ($null -ne $queuePost.countDetails -and $null -ne $queuePost.countDetails.deadLetterMessageCount) { [int]$queuePost.countDetails.deadLetterMessageCount } else { 0 }

    $postgresPostPath = Join-Path $rawDirectory -ChildPath 'postgres-post.json'
    $postgresPost = Read-JsonArtifact -Path $postgresPostPath
    $postgresState = [string]$postgresPost.state

    $apiRunningStatus = ''
    $schedulerRunningStatus = ''
    $workerRunningStatus = ''
    if ($postSnapshot.api.succeeded -and $null -ne $postSnapshot.api.json -and $null -ne $postSnapshot.api.json.properties) {
        $apiRunningStatus = [string]$postSnapshot.api.json.properties.runningStatus
    }
    if ($postSnapshot.scheduler.succeeded -and $null -ne $postSnapshot.scheduler.json -and $null -ne $postSnapshot.scheduler.json.properties) {
        $schedulerRunningStatus = [string]$postSnapshot.scheduler.json.properties.runningStatus
    }
    if ($postSnapshot.worker.succeeded -and $null -ne $postSnapshot.worker.json -and $null -ne $postSnapshot.worker.json.properties) {
        $workerRunningStatus = [string]$postSnapshot.worker.json.properties.runningStatus
    }

    $workloadBlocked = -not [string]::Equals($workerRunningStatus, 'Running', [System.StringComparison]::OrdinalIgnoreCase) -or
        -not [string]::Equals($apiRunningStatus, 'Running', [System.StringComparison]::OrdinalIgnoreCase) -or
        -not [string]::Equals($schedulerRunningStatus, 'Running', [System.StringComparison]::OrdinalIgnoreCase) -or
        $workerCrashLoopDetected -or
        ($postgresDependencyCrashCount -gt 0) -or
        ($apiPostgresDependencyCrashCount -gt 0) -or
        ($schedulerPostgresDependencyCrashCount -gt 0)

    $invariants = [System.Collections.Generic.List[object]]::new()

    if ($workloadBlocked) {
        $state = Convert-InvariantState -Status 'Blocked'
        $invariants.Add([ordered]@{
                name     = 'no unacceptable lost outcomes'
                status   = $state.status
                pass     = $state.pass
                fail     = $state.fail
                blocked  = $state.blocked
                reason   = 'Platform workload processing unavailable across worker/API/scheduler evidence, so outcome continuity cannot be observed.'
                evidence = @(
                    'raw\worker-logs-post.txt',
                    'raw\api-logs-post.txt',
                    'raw\scheduler-logs-post.txt',
                    'raw\worker-replicas-post.json'
                )
            })

        $invariants.Add([ordered]@{
                name     = 'no unacceptable duplicate outcomes beyond designed idempotency model'
                status   = $state.status
                pass     = $state.pass
                fail     = $state.fail
                blocked  = $state.blocked
                reason   = 'No trustworthy completed-outcome stream while workload is unhealthy; duplicate evaluation is blocked.'
                evidence = @(
                    'raw\worker-replicas-post.json',
                    'raw\worker-logs-post.txt',
                    $harnessSummaryRelativePath
                )
            })
    }
    else {
        $state = if ([string]::Equals([string]$harnessSummary.overallStatus, 'Succeeded', [System.StringComparison]::OrdinalIgnoreCase)) {
            Convert-InvariantState -Status 'Pass'
        }
        else {
            Convert-InvariantState -Status 'Fail'
        }

        $reason = if ($state.pass) { 'Harness completed successfully and workload remained healthy throughout evidence capture.' } else { "Harness status was '$($harnessSummary.overallStatus)' while workload remained available." }

        $invariants.Add([ordered]@{
                name     = 'no unacceptable lost outcomes'
                status   = $state.status
                pass     = $state.pass
                fail     = $state.fail
                blocked  = $state.blocked
                reason   = $reason
                evidence = @(
                    $harnessSummaryRelativePath,
                    'raw\worker-logs-post.txt',
                    'raw\api-logs-post.txt',
                    'raw\scheduler-logs-post.txt'
                )
            })

        $invariants.Add([ordered]@{
                name     = 'no unacceptable duplicate outcomes beyond designed idempotency model'
                status   = $state.status
                pass     = $state.pass
                fail     = $state.fail
                blocked  = $state.blocked
                reason   = $reason
                evidence = @(
                    $harnessSummaryRelativePath,
                    'raw\worker-replicas-post.json'
                )
            })
    }

    $converged = [string]::Equals($queueStatus, 'Active', [System.StringComparison]::OrdinalIgnoreCase) -and
        ($queueActive -eq 0) -and
        ($queueDeadLetter -eq 0) -and
        [string]::Equals($postgresState, 'Ready', [System.StringComparison]::OrdinalIgnoreCase) -and
        (-not $workloadBlocked)

    $convergenceState = if ($converged) { Convert-InvariantState -Status 'Pass' } else { Convert-InvariantState -Status 'Fail' }
    $convergenceReason = if ($converged) {
        'Queue transport, PostgreSQL state, and workload runtime converged to steady state.'
    }
    else {
        "Convergence failed: queue status=$queueStatus active=$queueActive deadLetter=$queueDeadLetter postgresState=$postgresState workerState=$workerReplicaState apiRunning=$apiRunningStatus schedulerRunning=$schedulerRunningStatus."
    }

    $convergenceEvidence = [System.Collections.Generic.List[string]]::new()
    foreach ($reference in @(
            'raw\queue-post.json',
            'raw\postgres-post.json',
            'raw\worker-replicas-post.json',
            $harnessSummaryRelativePath
        )) {
        $null = $convergenceEvidence.Add($reference)
    }
    if (-not [string]::IsNullOrWhiteSpace($postgresSummaryRelativePath)) {
        $null = $convergenceEvidence.Add($postgresSummaryRelativePath)
    }

    $invariants.Add([ordered]@{
            name     = 'queue backlog and DB run-state convergence'
            status   = $convergenceState.status
            pass     = $convergenceState.pass
            fail     = $convergenceState.fail
            blocked  = $convergenceState.blocked
            reason   = $convergenceReason
            evidence = $convergenceEvidence
        })

    $passCount = @($invariants | Where-Object { $_.pass }).Count
    $failCount = @($invariants | Where-Object { $_.fail }).Count
    $blockedCount = @($invariants | Where-Object { $_.blocked }).Count
    $overallResult = if ($failCount -gt 0) { 'Fail' } elseif ($blockedCount -gt 0) { 'Blocked' } else { 'Pass' }

    $invariantResult = [ordered]@{
        generatedAtUtc    = (Get-Date).ToUniversalTime().ToString('o')
        artifactRoot      = $resolvedOutputDirectory
        baselinePath      = $BaselinePath
        harnessSummary    = (Join-Path $harnessRunDirectory -ChildPath 'summary.json')
        postgresSummary   = $postgresSummaryPath
        workerStartup     = $workerStartupHealth
        measured          = [ordered]@{
            queuePost     = [ordered]@{ status = $queueStatus; active = $queueActive; deadLetter = $queueDeadLetter }
            postgresState = $postgresState
            workerState   = $workerReplicaState
            apiStatus     = $apiRunningStatus
            schedulerStatus = $schedulerRunningStatus
        }
        invariantCounts   = [ordered]@{
            pass    = $passCount
            fail    = $failCount
            blocked = $blockedCount
        }
        overallResult     = $overallResult
        invariants        = $invariants
        evidenceReferences = [ordered]@{
            worker    = @('raw\worker-post.json', 'raw\worker-replicas-post.json', 'raw\worker-logs-post.txt')
            api       = @('raw\api-post.json', 'raw\api-logs-post.txt')
            scheduler = @('raw\scheduler-post.json', 'raw\scheduler-logs-post.txt')
            queue     = @('raw\queue-pre.json', 'raw\queue-post.json')
            postgres  = @('raw\postgres-pre.json', 'raw\postgres-post.json')
            harness   = @($harnessSummaryRelativePath)
        }
    }

    Write-JsonArtifact -InputObject $invariantResult -Path (Join-Path $checksDirectory -ChildPath 'invariant-results.json')

    $markdown = [System.Collections.Generic.List[string]]::new()
    $markdown.Add('# DR integrity pass invariant results')
    $markdown.Add('')
    $markdown.Add("Generated (UTC): $($invariantResult.generatedAtUtc)")
    $markdown.Add("Artifact root: $($invariantResult.artifactRoot)")
    $markdown.Add("Overall result: $overallResult")
    $markdown.Add('')
    $markdown.Add('| Invariant | Status | pass | fail | blocked |')
    $markdown.Add('| --- | --- | --- | --- | --- |')
    foreach ($item in $invariants) {
        $markdown.Add("| $($item.name) | $($item.status) | $($item.pass) | $($item.fail) | $($item.blocked) |")
    }
    Write-TextArtifact -Path (Join-Path $checksDirectory -ChildPath 'invariant-results.md') -Content ($markdown -join [Environment]::NewLine)

    $runMetadata = [ordered]@{
        runStartedAtUtc   = $runTimestamp.ToUniversalTime().ToString('o')
        runFinishedAtUtc  = (Get-Date).ToUniversalTime().ToString('o')
        outputDirectory   = $resolvedOutputDirectory
        baselinePath      = $BaselinePath
        apply             = $Apply.IsPresent
        overallResult     = $overallResult
        commandToRerun    = "pwsh -NoProfile -File execution-ledger\scripts\Invoke-DrIntegrityPass.ps1 -Apply"
    }
    Write-JsonArtifact -InputObject $runMetadata -Path (Join-Path $resolvedOutputDirectory -ChildPath 'run-metadata.json')

    if ($failCount -gt 0 -or $blockedCount -gt 0) {
        Write-Error "DR integrity invariants did not pass. fail=$failCount blocked=$blockedCount"
        exit 1
    }

    Write-Information 'DR integrity invariants passed.'
    exit 0
}
catch {
    $fatalInvariantState = Convert-InvariantState -Status 'Blocked'
    $fatal = [ordered]@{
        generatedAtUtc  = (Get-Date).ToUniversalTime().ToString('o')
        artifactRoot    = $resolvedOutputDirectory
        overallResult   = 'Blocked'
        invariantCounts = [ordered]@{
            pass    = 0
            fail    = 0
            blocked = 1
        }
        invariants      = @(
            [ordered]@{
                name     = 'dr-integrity automation preconditions'
                status   = $fatalInvariantState.status
                pass     = $fatalInvariantState.pass
                fail     = $fatalInvariantState.fail
                blocked  = $fatalInvariantState.blocked
                reason   = $_.Exception.Message
                evidence = @()
            }
        )
        status          = 'Failed'
        error           = $_.Exception.Message
    }
    Write-JsonArtifact -InputObject $fatal -Path (Join-Path $checksDirectory -ChildPath 'invariant-results.json')
    Write-Error $_.Exception.Message
    exit 1
}
