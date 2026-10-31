# QA helper for f212: the two entries that lost zero-copy in f211, N times each.
#
#   powershell -File scripts/qa-f212-entries.ps1 -Kind search -Rounds 10
#   powershell -File scripts/qa-f212-entries.ps1 -Kind reopen -Rounds 10 -Dcomp 0
#
# One process per run. Every round opens a real player window through the UI's
# own callback, holds `-HoldMs`, and closes it through the ✕ path
# (`src/qa/f212.rs`, `BILI_ENTRY_CYCLE`). The verdict of a round is whatever the
# player logged for that session: `decode=mf` (zero-copy) or
# `decode fallback reason=…` (ffmpeg).
#
# `-Kind search` searches 原神 and opens the first result's card — the entry that
# failed in f211. `-Kind reopen` opens from the 推荐流, so the only thing that
# changes between rounds is that the previous player was just closed.
param(
    [ValidateSet("search", "reopen")]
    [string]$Kind = "search",
    [int]$Rounds = 10,
    [int]$HoldMs = 4000,
    [int]$StallMs = 0,
    # "" = leave `BILI_PLAYER_DCOMP` unset, i.e. the shipped default (on since
    # f212). `0` is the winit escape valve.
    [ValidateSet("0", "1", "")]
    [string]$Dcomp = "",
    [string]$Exe = "target/release/o11a1.exe",
    [string]$Tag = "",
    [string]$OutDir = "target/qa/f212"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$OutDir = if ([System.IO.Path]::IsPathRooted($OutDir)) { $OutDir } else { Join-Path $root $OutDir }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$suffix = if ($Tag) { "_$Tag" } else { "" }
$Log = Join-Path $OutDir ("f212_entry_{0}{1}.log" -f $Kind, $suffix)
if (Test-Path $Log) { Remove-Item $Log -Force }

$exePath = if ([System.IO.Path]::IsPathRooted($Exe)) { $Exe } else { Join-Path $root $Exe }
if (-not (Test-Path $exePath)) { throw "找不到 $exePath（先 cargo build --release）" }

Get-Process o11a1 -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 400

$env:BILI_DEBUG_LOG = $Log
if ($Dcomp) { $env:BILI_PLAYER_DCOMP = $Dcomp } else { Remove-Item Env:BILI_PLAYER_DCOMP -ErrorAction SilentlyContinue }
$env:BILI_UI_STALL = "1"
$env:BILI_ENTRY_CYCLE = "$Kind,$Rounds,$HoldMs,$StallMs"
foreach ($name in @("BILI_SEARCH_LOOP", "BILI_SEARCH_TEST", "BILI_OPEN_BVID", "BILI_LIVE_TEST",
                    "BILI_PGC_TEST", "BILI_F143_TEST", "BILI_QUALITY_PICK", "BILI_DECODE",
                    "BILI_QA_MF_FILE", "BILI_F212_HOLD")) {
    Remove-Item ("Env:" + $name) -ErrorAction SilentlyContinue
}

Write-Host ("f212 entry={0} rounds={1} hold={2}ms stall={5}ms dcomp={3} log={4}" -f $Kind, $Rounds, $HoldMs, ($(if ($Dcomp) { $Dcomp } else { "default" })), $Log, $StallMs)
$proc = Start-Process -FilePath $exePath -WorkingDirectory $root -PassThru
# Each round is a search (or a feed click) + hold + close; 45 s of slack on top.
$budget = [Math]::Max(90, $Rounds * [Math]::Ceiling(($HoldMs + 9000) / 1000) + 45)
$waited = 0
while (-not $proc.HasExited -and $waited -lt $budget) {
    Start-Sleep -Seconds 1
    $waited += 1
}
if (-not $proc.HasExited) {
    Write-Host ("没有在 {0}s 内自行退出（responding={1}），结束进程" -f $budget, $proc.Responding)
    $proc.Kill()
    $proc.WaitForExit(8000) | Out-Null
}

python (Join-Path $PSScriptRoot "qa-f212-entries.py") $Log $Rounds
