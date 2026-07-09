# Launch the ADS PySide6/QML asset browser against a remote ads serve.
# Token is read from ADS_WEB_TOKEN (required) — do not hardcode secrets here.
#
# Example:
#   $env:ADS_WEB_URL = "http://td-ln10:8787"
#   $env:ADS_WEB_TOKEN = "…"
#   .\scripts\run_ads_browser.ps1

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot

if (-not $env:ADS_WEB_URL) {
    $env:ADS_WEB_URL = "http://td-ln10:8787"
}
if (-not $env:ADS_WEB_TOKEN) {
    Write-Error "Set ADS_WEB_TOKEN before launching (Bearer token for ads serve)."
}

Set-Location $Root
uv run --extra browser ads-browser @args
