$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Split-Path -Parent $scriptDir

$cacheCargo = Join-Path $repoRoot ".cache\cargo"
if ($env:QQ_UIA_CARGO_HOME) { $cacheCargo = [IO.Path]::GetFullPath($env:QQ_UIA_CARGO_HOME) }
$userCargo = [IO.Path]::GetFullPath((Join-Path $env:USERPROFILE ".cargo"))
if ($cacheCargo.Equals($userCargo, [StringComparison]::OrdinalIgnoreCase) -or
    $cacheCargo.StartsWith($userCargo + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw "QQ_UIA_CARGO_HOME must not point into the user .cargo directory."
}
$cacheTarget = Join-Path $repoRoot ".cache\target"
$cacheTmp = Join-Path $repoRoot ".cache\tmp"
foreach ($dir in @($cacheCargo, $cacheTarget, $cacheTmp)) {
    if (-not (Test-Path -LiteralPath $dir)) {
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
    }
}
$env:CARGO_HOME = $cacheCargo
$env:CARGO_TARGET_DIR = $cacheTarget
$env:TMP = $cacheTmp
$env:TEMP = $cacheTmp

if ($env:QQ_UIA_RUSTUP_HOME) {
    $env:RUSTUP_HOME = [IO.Path]::GetFullPath($env:QQ_UIA_RUSTUP_HOME)
} elseif (-not $env:RUSTUP_HOME) {
    $existingRustup = Join-Path $env:USERPROFILE ".rustup"
    if (Test-Path -LiteralPath (Join-Path $existingRustup "settings.toml")) {
        $env:RUSTUP_HOME = $existingRustup
    } else {
        $env:RUSTUP_HOME = Join-Path $repoRoot ".cache\rustup"
    }
}

$prefixParts = @()
foreach ($configuredBin in @($env:QQ_UIA_TOOLCHAIN_BIN, $env:QQ_UIA_MINGW_BIN)) {
    if ($configuredBin) {
        if (-not (Test-Path -LiteralPath $configuredBin -PathType Container)) {
            throw "Configured tool directory does not exist: $configuredBin"
        }
        $prefixParts += [IO.Path]::GetFullPath($configuredBin)
    }
}
$existingParts = $env:PATH -split ";" | Where-Object { $_ -and ($prefixParts -notcontains $_) }
$env:PATH = (($prefixParts + $existingParts) -join ";")

$script:QqUiaCargoExe = $env:QQ_UIA_CARGO_EXE
if (-not $script:QqUiaCargoExe) {
    $cargoCommand = Get-Command cargo.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($cargoCommand) { $script:QqUiaCargoExe = $cargoCommand.Source }
}
