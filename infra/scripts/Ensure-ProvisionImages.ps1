#Requires -PSEdition Core

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..' '..'))
$acrEndpoint = $env:AZURE_CONTAINER_REGISTRY_ENDPOINT

if ([string]::IsNullOrWhiteSpace($acrEndpoint)) {
    Write-Information 'Skipping provision image build: AZURE_CONTAINER_REGISTRY_ENDPOINT is not set yet.' -InformationAction Continue
    return
}

$acrName = $acrEndpoint.Split('.')[0]

try {
    $null = az acr show --name $acrName --only-show-errors
}
catch {
    Write-Information "Skipping provision image build: ACR '$acrName' does not exist yet." -InformationAction Continue
    return
}

$services = @(
    [PSCustomObject]@{
        Name = 'api'
        ImageName = $env:SERVICE_API_IMAGE_NAME
        Dockerfile = 'crates/execution-ledger-api/Dockerfile'
        BuildContext = Join-Path $repoRoot 'execution-ledger'
    },
    [PSCustomObject]@{
        Name = 'worker'
        ImageName = $env:SERVICE_WORKER_IMAGE_NAME
        Dockerfile = 'crates/execution-ledger-worker/Dockerfile'
        BuildContext = Join-Path $repoRoot 'execution-ledger'
    },
    [PSCustomObject]@{
        Name = 'scheduler'
        ImageName = $env:SERVICE_SCHEDULER_IMAGE_NAME
        Dockerfile = 'crates/execution-ledger-scheduler/Dockerfile'
        BuildContext = Join-Path $repoRoot 'execution-ledger'
    },
    [PSCustomObject]@{
        Name = 'drasi'
        ImageName = $env:SERVICE_DRASI_IMAGE_NAME
        Dockerfile = 'crates/execution-ledger-drasi/Dockerfile'
        BuildContext = Join-Path $repoRoot 'execution-ledger'
    }
)

foreach ($service in $services) {
    if ([string]::IsNullOrWhiteSpace($service.ImageName)) {
        throw "SERVICE_$($service.Name.ToUpperInvariant())_IMAGE_NAME is not set."
    }

    if (-not $service.ImageName.StartsWith("$acrEndpoint/", [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Image '$($service.ImageName)' for service '$($service.Name)' is not in ACR '$acrEndpoint'."
    }

    $dockerfilePath = Join-Path $service.BuildContext $service.Dockerfile
    if (-not (Test-Path -LiteralPath $dockerfilePath)) {
        throw "Dockerfile not found for '$($service.Name)': $dockerfilePath"
    }

    $imageReference = $service.ImageName.Substring($acrEndpoint.Length + 1)
    $separatorIndex = $imageReference.LastIndexOf(':')
    if ($separatorIndex -lt 1 -or $separatorIndex -ge $imageReference.Length - 1) {
        throw "Image '$imageReference' must include an explicit tag."
    }

    $repositoryName = $imageReference.Substring(0, $separatorIndex)
    $tagName = $imageReference.Substring($separatorIndex + 1)

    $imageExists = $false
    try {
        $tagPresent = az acr repository show-tags `
            --name $acrName `
            --repository $repositoryName `
            --query "contains(@, '$tagName')" `
            --output tsv `
            --only-show-errors

        if (($tagPresent | Out-String).Trim().ToLowerInvariant() -eq 'true') {
            $imageExists = $true
        }
    }
    catch {
        $imageExists = $false
    }

    if ($imageExists) {
        Write-Information "Provision image already exists for '$($service.Name)': $imageReference" -InformationAction Continue
        continue
    }

    Write-Information "Building missing provision image for '$($service.Name)': $imageReference" -InformationAction Continue
    Push-Location -Path $service.BuildContext
    try {
        $null = az acr build `
            --registry $acrName `
            --image $imageReference `
            --file $service.Dockerfile `
            . `
            --only-show-errors
    }
    finally {
        Pop-Location
    }
}
