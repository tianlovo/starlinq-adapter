@echo off
setlocal
if defined QQ_UIA_MINGW_BIN set "PATH=%QQ_UIA_MINGW_BIN%;%PATH%"
if "%~1"=="" if exist "%~dp0.cache\target\release\qq-uia-adapter.exe" (
    "%~dp0.cache\target\release\qq-uia-adapter.exe" start-daemon
    exit /b %ERRORLEVEL%
)
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\start-daemon.ps1" %*
exit /b %ERRORLEVEL%
