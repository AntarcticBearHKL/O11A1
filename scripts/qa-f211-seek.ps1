# QA helper for f211: seek latency, end to end (segment table + p50/p95).
#
#   powershell -File scripts/qa-f211-seek.ps1
#   powershell -File scripts/qa-f211-seek.ps1 -Decoder ffmpeg -Tag ab_ffmpeg
#   powershell -File scripts/qa-f211-seek.ps1 -Targets "20,45,90,15,60" -Every 3500
#
# Drives real user seeks through `BILI_SEEK_AT` (the same `PlaybackControl::seek`
# the UI calls) and leaves the per-seek `seek trace` / `seek served` lines in the
# log; `qa-f211-report.ps1` turns them into the table.
param(
    [string]$Exe = "target/release/o11a1.exe",
    [ValidateSet("mf", "ffmpeg")]
    [string]$Decoder = "mf",
    # 画面直出 (DComp) — the zero-copy decoder needs the sink, so the default
    # arm runs with it on; `-Dcomp 0` is the factory path (winit).
    [ValidateSet("0", "1")]
    [string]$Dcomp = "1",
    [string]$Bvid = "BV1BBnb6aEww",
    [string]$Targets = "20,45,90,15,60,110,5,150,35,75,120,190,25,65,100,140,10,50,85,170",
    [int]$DelayMs = 9000,
    [int]$EveryMs = 2800,
    [int]$TailSeconds = 8,
    [string]$Tag = "",
    [string]$OutDir = "target/qa/f211"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$OutDir = if ([System.IO.Path]::IsPathRooted($OutDir)) { $OutDir } else { Join-Path $root $OutDir }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$suffix = if ($Tag) { "_$Tag" } else { "" }
$Log = Join-Path $OutDir ("f211_seek_{0}_dc{1}{2}.log" -f $Decoder, $Dcomp, $suffix)
if (Test-Path $Log) { Remove-Item $Log -Force }

$exePath = if ([System.IO.Path]::IsPathRooted($Exe)) { $Exe } else { Join-Path $root $Exe }
if (-not (Test-Path $exePath)) { throw "找不到 $exePath（先 cargo build --release）" }

Get-Process o11a1 -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 400

$env:BILI_DEBUG_LOG = $Log
$env:BILI_OPEN_BVID = $Bvid
$env:BILI_QUALITY_PICK = "0,6000"
$env:BILI_PLAYER_DCOMP = $Dcomp
$env:BILI_SEEK_AT = $Targets
$env:BILI_SEEK_AT_DELAY_MS = "$DelayMs"
$env:BILI_SEEK_AT_EVERY_MS = "$EveryMs"
$env:BILI_AVSYNC_TEST = "seek"
if ($Decoder -eq "ffmpeg") { $env:BILI_DECODE = "ffmpeg" } else { Remove-Item Env:BILI_DECODE -ErrorAction SilentlyContinue }
Remove-Item Env:BILI_QA_MF_FILE -ErrorAction SilentlyContinue

$count = ($Targets -split ",").Count
$seconds = [math]::Ceiling($DelayMs / 1000) + $count * [math]::Ceiling($EveryMs / 1000) + $TailSeconds
Write-Host ("f211 seek decoder={0} dcomp={1} bvid={2} targets={3} run={4}s log={5}" -f $Decoder, $Dcomp, $Bvid, $Targets, $seconds, $Log)

$proc = Start-Process -FilePath $exePath -WorkingDirectory $root -PassThru
try {
    Start-Sleep -Seconds $seconds
} finally {
    if ($proc -and -not $proc.HasExited) {
        $proc.Kill()
        $proc.WaitForExit(8000) | Out-Null
    }
}

Write-Host ""
Write-Host "---- seek trace ----"
if (Test-Path $Log) {
    Select-String -Path $Log -Pattern "seek-at |seek trace |seek served |seek start |seek freeze |decode=mf seek|seek shadow|seek adopt|stream qn=|decode=mf " |
        ForEach-Object { Write-Host $_.Line }
    Write-Host ""
    Write-Host "---- decode ----"
    Select-String -Path $Log -Pattern "decode=mf |decode fallback|decode frames=" |
        Select-Object -Last 12 | ForEach-Object { Write-Host $_.Line }
    if (-not (Select-String -Path $Log -Pattern "decode=mf " -Quiet)) {
        Write-Host "（没有 decode=mf 行：这次跑的是 ffmpeg 管道）"
    }
} else {
    Write-Host "没有日志 $Log"
}
