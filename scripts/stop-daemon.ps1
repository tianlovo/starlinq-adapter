$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Split-Path -Parent $scriptDir
$connPath = Join-Path $repoRoot ".runtime\connection.json"
$configPath = Join-Path $repoRoot "config\default.toml"

$serverHost = "127.0.0.1"
$serverPort = 18770
$accessToken = $null

if (Test-Path $configPath) {
    $inServer = $false
    foreach ($rawLine in (Get-Content -Path $configPath -Encoding UTF8)) {
        $line = $rawLine.Trim()
        if ($line -match '^\[([^\]]+)\]') {
            $inServer = ($Matches[1].Trim() -eq "server")
            continue
        }
        if (-not $inServer -or $line.StartsWith("#") -or [string]::IsNullOrWhiteSpace($line)) {
            continue
        }
        if ($line -match '^host\s*=\s*["'']([^"'']+)["'']') {
            $serverHost = $Matches[1].Trim()
        } elseif ($line -match '^port\s*=\s*([0-9]+)') {
            $serverPort = [int]$Matches[1]
        } elseif ($line -match '^access_token\s*=\s*["'']([^"'']+)["'']') {
            $accessToken = $Matches[1]
        }
    }
}

if (Test-Path $connPath) {
    try {
        $conn = Get-Content -Path $connPath -Raw -Encoding UTF8 | ConvertFrom-Json
        if ($conn.host) { $serverHost = [string]$conn.host }
        if ($conn.port) { $serverPort = [int]$conn.port }
    } catch {
        # Fallback to config/default values
    }
}

$connectHost = if ($serverHost -eq "0.0.0.0" -or $serverHost -eq "::") { "127.0.0.1" } else { $serverHost }
$baseUrl = "http://$($connectHost):$($serverPort)"
$headers = @{}
if (-not [string]::IsNullOrEmpty($accessToken)) {
    $headers["Authorization"] = "Bearer $accessToken"
}

$alive = $false
try {
    $null = Invoke-RestMethod -Method Get -Uri "$baseUrl/api/health" -Headers $headers -TimeoutSec 2
    $alive = $true
} catch {
    $alive = $false
}

if (-not $alive) {
    Write-Host "[qq-uia-adapter] Daemon is not running at $baseUrl."
    exit 0
}

Write-Host "[qq-uia-adapter] Sending shutdown to $baseUrl..."
try {
    $null = Invoke-RestMethod -Method Post -Uri "$baseUrl/api/shutdown" -Headers $headers -TimeoutSec 3
} catch {
    # Server may terminate while closing socket
}

$stopped = $false
for ($i = 0; $i -lt 15; $i++) {
    Start-Sleep -Milliseconds 200
    try {
        $null = Invoke-RestMethod -Method Get -Uri "$baseUrl/api/health" -Headers $headers -TimeoutSec 1
    } catch {
        $stopped = $true
        break
    }
}

if (-not $stopped) {
    Write-Error "[qq-uia-adapter] Daemon at $baseUrl did not stop within 3s."
    exit 1
}

Write-Host "[qq-uia-adapter] Daemon stopped."
