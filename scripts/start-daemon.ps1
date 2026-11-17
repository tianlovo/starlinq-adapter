param(
    [switch]$Rebuild,
    [switch]$DryRun
)
$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$releaseBin = Join-Path $repoRoot ".cache\target\release\qq-uia-adapter.exe"
. (Join-Path $PSScriptRoot "env.ps1")
if ($DryRun) { exit 0 }
if ($Rebuild -or -not (Test-Path $releaseBin)) {
    & (Join-Path $repoRoot "scripts\cargo-local.ps1") build --release -p qq-cli
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
& $releaseBin start-daemon
exit $LASTEXITCODE
