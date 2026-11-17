$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Split-Path -Parent $scriptDir
$ErrorActionPreference = "Stop"

. (Join-Path $scriptDir "env.ps1")

$cargoExe = $script:QqUiaCargoExe
if (-not $cargoExe -or -not (Test-Path -LiteralPath $cargoExe -PathType Leaf)) {
    Write-Error "Cargo was not found. Check the environment first, then set QQ_UIA_TOOLCHAIN_BIN or QQ_UIA_CARGO_EXE, or add the intended toolchain to PATH."
    exit 1
}

$cwd = (Get-Location).Path
$pushed = $false
if (-not ($cwd.Equals($repoRoot, [System.StringComparison]::OrdinalIgnoreCase) -or
          $cwd.StartsWith($repoRoot + [IO.Path]::DirectorySeparatorChar, [System.StringComparison]::OrdinalIgnoreCase))) {
    Push-Location $repoRoot
    $pushed = $true
}

try {
    & $cargoExe @args
    $code = $LASTEXITCODE
} finally {
    if ($pushed) {
        Pop-Location
    }
}

exit $code
