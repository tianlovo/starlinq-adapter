@echo off
setlocal
pushd "%~dp0"

set "BIN=.cache\target\release\qq-uia-adapter.exe"
if defined QQ_UIA_MINGW_BIN set "PATH=%QQ_UIA_MINGW_BIN%;%PATH%"

if not exist "%BIN%" (
    echo [qq-uia-adapter] Release binary not found, building qq-cli...
    "%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\cargo-local.ps1" build --release -p qq-cli
    if errorlevel 1 (
        echo [qq-uia-adapter] Build failed with exit code %ERRORLEVEL%.
        popd
        exit /b %ERRORLEVEL%
    )
)

"%BIN%" --config "%~dp0config\default.toml" inspect --depth 5 --out ".runtime\uia-tree.json" %*
set "RC=%ERRORLEVEL%"
popd
exit /b %RC%
