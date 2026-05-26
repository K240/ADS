param(
    [string]$HoudiniRoot = $env:HFS,
    [string]$Configuration = "Release"
)

$ErrorActionPreference = "Stop"

if (-not $HoudiniRoot) {
    $candidates = Get-ChildItem -Path "C:\Program Files\Side Effects Software" -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -like "Houdini *" } |
        Sort-Object Name -Descending
    if (-not $candidates) {
        throw "HoudiniRoot was not supplied and HFS is not set."
    }
    $HoudiniRoot = $candidates[0].FullName
}

$hcustom = Join-Path $HoudiniRoot "bin\hcustom.exe"
if (-not (Test-Path -LiteralPath $hcustom)) {
    throw "hcustom.exe was not found under $HoudiniRoot"
}

$source = Join-Path $PSScriptRoot "src\AdsResolver.cpp"
$pluginRoot = Join-Path $PSScriptRoot "build\houdini"
$resources = Join-Path $pluginRoot "resources"

New-Item -ItemType Directory -Path $pluginRoot -Force | Out-Null
New-Item -ItemType Directory -Path $resources -Force | Out-Null
Copy-Item -LiteralPath (Join-Path $PSScriptRoot "resources\plugInfo.json") -Destination (Join-Path $resources "plugInfo.json") -Force

$args = @(
    "-U",
    "-i", $pluginRoot,
    $source
)

if ($Configuration -eq "Debug") {
    $args = @("-g") + $args
}

& $hcustom @args
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

Write-Host "Built ADS resolver plugin:"
Write-Host "  $pluginRoot"
Write-Host ""
Write-Host "Set PXR_PLUGINPATH_NAME to:"
Write-Host "  $resources"
