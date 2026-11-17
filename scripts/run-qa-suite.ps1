param(
    [ValidateSet("L0", "L1", "L2", "L3", "L4", "All")]
    [string]$Tier = "All",
    [switch]$SkipUnitTests,
    [switch]$SkipLiveSmoke,
    [string]$LiveUrl = "http://127.0.0.1:18770",
    [string]$ReportDir = ".runtime/qa-reports/latest"
)

$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot

function Invoke-Step {
    param(
        [string]$Label,
        [scriptblock]$Command
    )
    Write-Host "[QA] ==> $Label" -ForegroundColor Cyan
    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "QA stage failed ($Label), exit code = $LASTEXITCODE"
    }
}

Invoke-Step "Coding style check (crates)" {
    python scripts\check_coding_style.py crates
}

Invoke-Step "Cargo fmt check" {
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\cargo-local.ps1 fmt --all -- --check
}

Invoke-Step "Cargo clippy (-D warnings)" {
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\cargo-local.ps1 clippy --workspace --all-targets -- -D warnings
}

if (-not $SkipUnitTests -and ($Tier -eq "L1" -or $Tier -eq "L2" -or $Tier -eq "All")) {
    Invoke-Step "Workspace unit & integration tests" {
        powershell -NoProfile -ExecutionPolicy Bypass -File scripts\cargo-local.ps1 test --workspace
    }
}

Invoke-Step "Build release qq-qa-harness" {
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\cargo-local.ps1 build --release --bin qq-qa-harness
}

$HarnessExe = Join-Path $RepoRoot ".cache\target\release\qq-qa-harness.exe"
if (-not (Test-Path $HarnessExe)) {
    throw "Missing release binary: $HarnessExe"
}

$HarnessArgs = @("--tier", $Tier, "--live-url", $LiveUrl, "--out-dir", $ReportDir)
if ($SkipLiveSmoke) {
    $HarnessArgs += "--skip-live"
}

Invoke-Step "Run qq-qa-harness (--tier $Tier)" {
    & $HarnessExe @HarnessArgs
}

$HtmlPath = Join-Path $RepoRoot (Join-Path $ReportDir "index.html")
$JsonPath = Join-Path $RepoRoot (Join-Path $ReportDir "qa-report.json")
Write-Host ""
Write-Host "[QA] Completed tier '$Tier' cleanly." -ForegroundColor Green
Write-Host "[QA] HTML Dashboard : $HtmlPath"
Write-Host "[QA] JSON Telemetry : $JsonPath"
