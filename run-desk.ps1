# flowlab/run-desk.ps1
# ------------------------------------------------------------------
# Starts the full FLOWLAB pipeline in TWO separate PowerShell windows:
#
#   1) flowlab-engine  (Rust, TCP telemetry on 127.0.0.1:9090)
#   2) flowlab-api     (Go,   HTTP + WS + static dashboard on :8080)
#
# The dashboard is built once (vite build) and served from the Go API
# on the SAME origin as /stream - no Vite dev server, no ws proxy, no
# cross-port shenanigans that make Firefox throttle the tab.
#
# Usage:
#   .\run-desk.ps1                    # real ITCH file @ 50x (default)
#   .\run-desk.ps1 -Feed synthetic    # synthetic feed, no ITCH needed
#   .\run-desk.ps1 -Speedup 10        # slower replay
#   .\run-desk.ps1 -Dev               # also spawn Vite dev with HMR
#   .\run-desk.ps1 -SkipDashboardBuild # reuse existing dashboard/dist
#   .\run-desk.ps1 -KillOnly          # tear everything down and exit
# ------------------------------------------------------------------

[CmdletBinding()]
param(
    [ValidateSet('engine','synthetic')]
    [string]$Feed = 'engine',

    [string]$IchFile = "$PSScriptRoot\data\itch\unpacked\20200130.BX_ITCH_50",

    [double]$Speedup = 50.0,

    [int]$EnginePort    = 9090,
    [int]$ApiPort       = 8080,
    [int]$DashboardPort = 5173,

    [switch]$Dev,
    [switch]$NoBrowser,
    [switch]$KillOnly,
    [switch]$SkipBuild,
    [switch]$SkipDashboardBuild,

    # Pin the engine to a specific NASDAQ ticker (e.g. 'TSLA', 'SPY') or
    # raw stock_locate id. Empty string => engine auto-locks.
    [string]$Symbol = ''
)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot

function Stop-FlowlabStack {
    Write-Host '[teardown] stopping previous flowlab processes...' -ForegroundColor Yellow
    Get-Process flowlab-engine, flowlab-api, esbuild, vite -ErrorAction SilentlyContinue |
        ForEach-Object {
            Write-Host "  kill $($_.Name) PID=$($_.Id)"
            Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
        }
    $ports = @($EnginePort, $ApiPort, $DashboardPort)
    Get-NetTCPConnection -State Listen -LocalPort $ports -ErrorAction SilentlyContinue |
        Sort-Object OwningProcess -Unique |
        ForEach-Object {
            Write-Host "  kill PID=$($_.OwningProcess) holding :$($_.LocalPort)"
            Stop-Process -Id $_.OwningProcess -Force -ErrorAction SilentlyContinue
        }
    Start-Sleep -Milliseconds 500
}

Stop-FlowlabStack
if ($KillOnly) { Write-Host '[done] teardown only.' -ForegroundColor Green ; return }

# ---- sanity checks -------------------------------------------------------

$engineBin = Join-Path $root 'target\release\flowlab-engine.exe'
if (-not $SkipBuild -and -not (Test-Path $engineBin)) {
    Write-Host '[build] flowlab-engine (release)...' -ForegroundColor Cyan
    Push-Location $root
    try { cargo build -p flowlab-engine --release | Out-Host } finally { Pop-Location }
}
if (-not (Test-Path $engineBin)) {
    throw "missing $engineBin. Run: cargo build -p flowlab-engine --release"
}

if ($Feed -eq 'engine' -and -not (Test-Path $IchFile)) {
    throw "ITCH file not found: $IchFile. Pass -Feed synthetic or supply -IchFile <path>."
}

$apiDir  = Join-Path $root 'api'
$dashDir = Join-Path $root 'dashboard'
$distDir = Join-Path $dashDir 'dist'
if (-not (Test-Path (Join-Path $apiDir 'go.mod'))) { throw "api dir broken: $apiDir" }
if (-not (Test-Path (Join-Path $dashDir 'package.json'))) { throw "dashboard dir broken: $dashDir" }

# ---- dashboard build (production mode, served by Go) ---------------------

if (-not $SkipDashboardBuild) {
    Write-Host '[build] dashboard (vite build)...' -ForegroundColor Cyan
    Push-Location $dashDir
    try {
        npm run build | Out-Host
        if ($LASTEXITCODE -ne 0) { throw "dashboard build failed" }
    } finally { Pop-Location }
}
if (-not (Test-Path $distDir)) {
    throw "dashboard/dist missing. Run: cd dashboard && npm run build"
}

# ---- launcher ------------------------------------------------------------

function Start-Pane {
    param(
        [string]$Title,
        [string]$Cwd,
        [string]$Command
    )
    $banner = "Write-Host '=== $Title ===' -ForegroundColor Cyan"
    $full   = "$banner; $Command"
    $shell = if (Get-Command pwsh -ErrorAction SilentlyContinue) { 'pwsh' } else { 'powershell' }
    Start-Process -FilePath $shell -ArgumentList @(
        '-NoExit',
        '-NoProfile',
        '-Command',
        "Set-Location '$Cwd'; `$Host.UI.RawUI.WindowTitle='$Title'; $full"
    ) -WindowStyle Normal | Out-Null
}

if ($Feed -eq 'engine') {
    $engineArgs = @(
        '--source','ich',
        '--source-arg', $IchFile,
        '--ich-pace',   [string]$Speedup,
        '--ich-framing','binaryfile',
        '--listen',     "127.0.0.1:$EnginePort",
        '--tick-hz',    '50',
        '--book-hz',    '10',
        '--wire',       'json'
    )
    if ($Symbol -ne '') { $engineArgs += @('--symbol', $Symbol) }
    $apiArgs = @(
        '-addr',   ":$ApiPort",
        '-feed',   'engine',
        '-engine', "127.0.0.1:$EnginePort"
    )
    $feedLabel = "NASDAQ BX ITCH 5.0 @ $([string]$Speedup)x"
    if ($Symbol -ne '') { $feedLabel += " [pinned=$Symbol]" }
} else {
    $engineArgs = @(
        '--source','synthetic',
        '--listen',"127.0.0.1:$EnginePort",
        '--tick-hz','50',
        '--book-hz','10',
        '--wire','json'
    )
    # Even on synthetic, route the API through the Rust engine so that
    # ORDER BOOK and TRADE TAPE panels are populated from the real
    # state machine (the in-process Go SyntheticFeed emits ticks only,
    # leaving those two panels empty).
    $apiArgs = @(
        '-addr',   ":$ApiPort",
        '-feed',   'engine',
        '-engine', "127.0.0.1:$EnginePort"
    )
    $feedLabel = 'SYNTHETIC (via engine TCP)'
}

Write-Host "[1/2] engine pane ($feedLabel)..." -ForegroundColor Green
Start-Pane -Title 'flowlab-engine' -Cwd $root -Command `
    "`$env:RUST_LOG='info'; & '$engineBin' $($engineArgs -join ' ')"

Start-Sleep -Seconds 1

Write-Host "[2/2] api+ui pane (Go, :$ApiPort) - serves dashboard/dist on /..." -ForegroundColor Green
Start-Pane -Title 'flowlab-api + dashboard' -Cwd $apiDir -Command `
    "go run .\cmd\api $($apiArgs -join ' ')"

$openUrl = "http://localhost:$ApiPort/"

if ($Dev) {
    Start-Sleep -Seconds 2
    Write-Host "[extra] vite dev pane (:$DashboardPort) - HMR only" -ForegroundColor Green
    Start-Pane -Title 'flowlab-dashboard (vite dev)' -Cwd $dashDir -Command 'npm run dev'
}

if (-not $NoBrowser) {
    Start-Sleep -Seconds 4
    Write-Host "[open] $openUrl" -ForegroundColor Green
    Start-Process $openUrl
}

Write-Host ''
Write-Host '================= FLOWLAB LIVE DESK =================' -ForegroundColor Cyan
Write-Host "  feed      : $feedLabel"
Write-Host "  engine    : tcp 127.0.0.1:$EnginePort"
Write-Host "  api + UI  : http://localhost:$ApiPort  (WS /stream, static /)"
if ($Dev) { Write-Host "  vite dev  : http://localhost:$DashboardPort  (HMR only)" }
Write-Host ''
Write-Host '  stop everything:  .\run-desk.ps1 -KillOnly'
Write-Host '====================================================='
