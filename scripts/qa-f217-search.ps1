# f217: search tabs (视频 / 番剧 / 影视 / 直播 / 用户).
#
# Developer tool, not product code.
#
#   powershell -File scripts/qa-f217-search.ps1                 # full run
#   powershell -File scripts/qa-f217-search.ps1 -Mode empty     # empty state only
#   powershell -File scripts/qa-f217-search.ps1 -Mode anon      # search with no account
#   powershell -File scripts/qa-f217-search.ps1 -Mode fail      # real network outage
#
# One launch, one log. Inside the app `BILI_F217_TEST` drives the REAL search
# page: `do_search`, `search_tab_changed` and `open_search_item` are the same
# callbacks a click fires, so the rows counted in the log are the rows the user
# sees. Each tab logs its desensitised request URL, the settled row count, the
# wall time and the first three titles; the 番剧 and 直播 results are opened in
# the player and checked for presented frames.
#
# Screenshots are taken by the app itself (PrintWindow) into `docs`:
#   f217_search_bangumi.png / f217_search_live.png / f217_search_empty.png
#   f217_season_player.png / f217_live_player.png
#   and, per -Mode, f217_search_anon.png / f217_search_failed.png
param(
    [string]$Exe = "target/release/o11a1.exe",
    [ValidateSet("all", "empty", "anon", "fail")]
    [string]$Mode = "all",
    [string]$OutDir = "docs",
    [string]$LogPath = "target/qa/f217_search.log",
    [string]$AppHome = "",
    [int]$TimeoutSec = 600,
    [switch]$KeepOpen
)

$ErrorActionPreference = "Stop"
$root = (Resolve-Path -LiteralPath ".").Path
$shotDir = Join-Path $root $OutDir
if (-not (Test-Path $shotDir)) { New-Item -ItemType Directory -Path $shotDir -Force | Out-Null }
$log = Join-Path $root $LogPath
$logDir = Split-Path -Parent $log
if (-not (Test-Path $logDir)) { New-Item -ItemType Directory -Path $logDir -Force | Out-Null }
if (Test-Path $log) { Remove-Item -LiteralPath $log -Force }

$env:BILI_DEBUG_LOG = $log
$env:BILI_CAPTURE_DIR = $shotDir
$env:BILI_F217_TEST = $Mode
# A build outside `target\` is not recognised as a dev build, so the saved
# account would be missed; point the app at the repository explicitly. `-Home`
# points it at a private copy of accounts/settings instead, so a parallel QA run
# cannot rewrite the evidence (and cannot be rewritten by this one).
if (-not $env:O11A1_HOME) { $env:O11A1_HOME = $root }
if ($AppHome -ne "") {
    $homeDir = Join-Path $root $AppHome
    if (-not (Test-Path $homeDir)) { New-Item -ItemType Directory -Path $homeDir -Force | Out-Null }
    $env:O11A1_HOME = $homeDir
}
if ($Mode -eq "anon") {
    # No account: a throwaway home, so `accounts.json` is not found.
    $anonHome = Join-Path $root "target/qa/f217_anon_home"
    if (-not (Test-Path $anonHome)) { New-Item -ItemType Directory -Path $anonHome -Force | Out-Null }
    $env:O11A1_HOME = $anonHome
}
if ($Mode -eq "fail") {
    # The app's own client build then points HTTPS at a closed port; see the
    # hook for why the failure is real and not a canned message.
    $env:HTTPS_PROXY = "http://127.0.0.1:1"
    $env:ALL_PROXY = "http://127.0.0.1:1"
}

$marker = "f217 done"
if ($Mode -eq "empty") { $marker = "f217 shot f217_search_empty" }
if ($Mode -eq "anon") { $marker = "f217 shot f217_search_anon" }
if ($Mode -eq "fail") { $marker = "f217 shot f217_search_failed" }

Write-Host "f217: mode=$Mode exe=$Exe log=$log shot_dir=$shotDir"
$proc = Start-Process -FilePath (Join-Path $root $Exe) -PassThru
$deadline = (Get-Date).AddSeconds($TimeoutSec)
try {
    while ((Get-Date) -lt $deadline -and -not $proc.HasExited) {
        if (Test-Path $log) {
            $hit = Select-String -LiteralPath $log -Pattern $marker -SimpleMatch -Quiet
            if ($hit) { Start-Sleep -Milliseconds 1200; break }
        }
        Start-Sleep -Milliseconds 400
    }
    if ($proc.HasExited) { Write-Host "f217: app exited early code=$($proc.ExitCode)" }
    else {
        $found = (Test-Path $log) -and (Select-String -LiteralPath $log -Pattern $marker -SimpleMatch -Quiet)
        if (-not $found) { Write-Host "f217: timeout after $TimeoutSec s without '$marker'" }
    }
}
finally {
    if (-not $KeepOpen) {
        if (-not $proc.HasExited) {
            try { $proc.CloseMainWindow() | Out-Null } catch {}
            Start-Sleep -Milliseconds 900
            if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
        }
    }
}

if (Test-Path $log) {
    $lines = Get-Content $log
    $mine = @($lines | Where-Object { $_ -match "^f217 |^search (run|ok|fail|status|media) " })
    Write-Host ""
    Write-Host ("f217: log lines={0} f217 lines={1}" -f $lines.Count, $mine.Count)
    foreach ($line in $mine) { Write-Host "  $line" }
    $shots = @($lines | Where-Object { $_ -match "capture png path=" })
    Write-Host ""
    Write-Host ("f217: captures={0}" -f $shots.Count)
}
