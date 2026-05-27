@echo off
setlocal

rem Launch Houdini with the ADS USD resolver in native remote mode.
rem This mode does not call ads.exe or curl.exe while resolving assets.
rem
rem Usage:
rem   launch_ads_remote_houdini.bat <ads-server> [profile] [auth-token] [workspace]
rem
rem Example:
rem   launch_ads_remote_houdini.bat http://127.0.0.1:8789 phase3 phase3-test-token D:\workspace
rem
rem Arguments can be omitted when the matching environment variables are already set:
rem   ADS_RESOLVER_SERVER
rem   ADS_RESOLVER_PROFILE
rem   ADS_RESOLVER_API_TOKEN
rem   ADS_RESOLVER_WORKSPACE
rem
rem Optional overrides:
rem   HOUDINI_ROOT
rem   ADS_RESOLVER_HTTP_TIMEOUT_SECONDS
rem   ADS_RESOLVER_DEBUG

set "SCRIPT_DIR=%~dp0"
for %%I in ("%SCRIPT_DIR%..") do set "ADS_REPO_ROOT=%%~fI"

if not "%~1"=="" set "ADS_RESOLVER_SERVER=%~1"
if not "%~2"=="" set "ADS_RESOLVER_PROFILE=%~2"
if not "%~3"=="" set "ADS_RESOLVER_API_TOKEN=%~3"
if not "%~4"=="" set "ADS_RESOLVER_WORKSPACE=%~4"

if not defined HOUDINI_ROOT set "HOUDINI_ROOT=C:\Program Files\Side Effects Software\Houdini 21.0.700"
if not defined ADS_RESOLVER_PROFILE set "ADS_RESOLVER_PROFILE=main"
if not defined ADS_RESOLVER_HTTP_TIMEOUT_SECONDS set "ADS_RESOLVER_HTTP_TIMEOUT_SECONDS=30"
if not defined ADS_RESOLVER_LOG_FILE set "ADS_RESOLVER_LOG_FILE=%TEMP%\ads_resolver_houdini.log"

if not defined ADS_RESOLVER_SERVER (
    echo ADS_RESOLVER_SERVER is required.
    echo Usage: %~nx0 ^<ads-server^> [profile] [auth-token] [workspace]
    exit /b 2
)

echo %ADS_RESOLVER_SERVER% | findstr /i /b /c:"http://" /c:"https://" >nul
if errorlevel 1 (
    echo ADS_RESOLVER_SERVER must start with http:// or https://
    echo Current value:
    echo   %ADS_RESOLVER_SERVER%
    echo.
    echo Native remote mode expects the ADS Web/API server, not a local store path.
    echo Example:
    echo   %~nx0 http://127.0.0.1:8789 phase3 phase3-test-token D:\workspace
    exit /b 2
)

if not exist "%HOUDINI_ROOT%\bin\houdini.exe" (
    echo Houdini executable was not found:
    echo   %HOUDINI_ROOT%\bin\houdini.exe
    echo Set HOUDINI_ROOT before running this script.
    exit /b 2
)

set "ADS_RESOLVER_MODE=remote"
set "ADS_RESOLVER_EXECUTABLE="
set "ADS_RESOLVER_HTTP_EXECUTABLE="

set "ADS_PLUGIN_RESOURCES=%ADS_REPO_ROOT%\resolver\build\houdini\resources"
set "ADS_HOUDINI_PATH=%ADS_REPO_ROOT%\houdini"
set "ADS_PYTHONPATH=%ADS_REPO_ROOT%\python\src"

if not exist "%ADS_PLUGIN_RESOURCES%\plugInfo.json" (
    echo ADS resolver plugin resources were not found:
    echo   %ADS_PLUGIN_RESOURCES%\plugInfo.json
    echo Build the resolver first:
    echo   resolver\build_houdini.ps1 -HoudiniRoot "%HOUDINI_ROOT%"
    exit /b 2
)

if defined PXR_PLUGINPATH_NAME (
    set "PXR_PLUGINPATH_NAME=%ADS_PLUGIN_RESOURCES%;%PXR_PLUGINPATH_NAME%"
) else (
    set "PXR_PLUGINPATH_NAME=%ADS_PLUGIN_RESOURCES%"
)

if defined PYTHONPATH (
    set "PYTHONPATH=%ADS_PYTHONPATH%;%PYTHONPATH%"
) else (
    set "PYTHONPATH=%ADS_PYTHONPATH%"
)

if defined HOUDINI_PATH (
    set "HOUDINI_PATH=%ADS_HOUDINI_PATH%;%HOUDINI_PATH%"
) else (
    set "HOUDINI_PATH=%ADS_HOUDINI_PATH%;&"
)

echo Launching Houdini with ADS resolver native remote mode.
echo   HOUDINI_ROOT=%HOUDINI_ROOT%
echo   ADS_RESOLVER_SERVER=%ADS_RESOLVER_SERVER%
echo   ADS_RESOLVER_PROFILE=%ADS_RESOLVER_PROFILE%
if defined ADS_RESOLVER_WORKSPACE echo   ADS_RESOLVER_WORKSPACE=%ADS_RESOLVER_WORKSPACE%
echo   ADS_RESOLVER_LOG_FILE=%ADS_RESOLVER_LOG_FILE%
echo   PXR_PLUGINPATH_NAME=%PXR_PLUGINPATH_NAME%

start "" "%HOUDINI_ROOT%\bin\houdini.exe"
