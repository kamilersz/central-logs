param(
    [string]$DataDir = (Join-Path $env:TEMP "central-logs-smoke"),
    [int]$Port = 18080
)

if (Test-Path $DataDir) { Remove-Item -Recurse -Force $DataDir }
New-Item -ItemType Directory -Path $DataDir | Out-Null

$exe = Join-Path $PSScriptRoot "target\debug\central-logs.exe"
if (-not (Test-Path $exe)) {
    Write-Error "binary not found at $exe — run cargo build first"
    exit 1
}

$proc = Start-Process -FilePath $exe -ArgumentList @(
    "--data-dir", $DataDir,
    "--http-port", $Port,
    "--no-syslog-udp",
    "--no-syslog-tcp",
    "--mcp-mode", "off",
    "--hot-attribute", "user_id:bigint",
    "--hot-attribute", "env:varchar"
) -PassThru -WindowStyle Hidden -RedirectStandardOutput "$DataDir\stdout.log" -RedirectStandardError "$DataDir\stderr.log"

$pidStr = $proc.Id
Write-Host "server pid $pidStr, waiting 4s for startup..."
Start-Sleep -Seconds 4

if ($proc.HasExited) {
    Write-Error "server exited early; last 30 lines:"
    Get-Content "$DataDir\stderr.log" -Tail 30
    exit 1
}

function Probe {
    param([string]$Label, [string]$Url, [int]$Expect = 200)
    try {
        $r = Invoke-WebRequest -Uri $Url -UseBasicParsing -TimeoutSec 5
        $match = $r.StatusCode -eq $Expect
        $hint = ""
        if ($r.StatusCode -eq 200 -and $r.Content.Length -lt 1000) { $hint = "(placeholder?)" }
        $mark = if ($match) { "OK  " } else { "FAIL" }
        Write-Host ("[{0}] {1,-40} status={2} bytes={3} {4}" -f $mark, $Label, $r.StatusCode, $r.Content.Length, $hint)
    } catch {
        Write-Host ("[FAIL] {0,-40} {1}" -f $Label, $_.Exception.Message)
    }
}

Write-Host ""
Write-Host "=== probes ==="
Probe "/ (SPA root)"              "http://127.0.0.1:$Port/"
Probe "/health"                   "http://127.0.0.1:$Port/health"
Probe "/api/schema/hot"           "http://127.0.0.1:$Port/api/schema/hot"
Probe "/api/counters"             "http://127.0.0.1:$Port/api/counters"
Probe "/api/dashboard/error-rate" "http://127.0.0.1:$Port/api/dashboard/error-rate"
Probe "/index.html (raw)"         "http://127.0.0.1:$Port/index.html"

Write-Host ""
Write-Host "server still running, pid $pidStr. To stop: Stop-Process -Id $pidStr"