# QA helper for f211: 音画同步 (A/V sync) over the phases that reset it.
#
#   powershell -File scripts/qa-f211-avsync.ps1
#   powershell -File scripts/qa-f211-avsync.ps1 -Decoder ffmpeg -Dcomp 1 -Tag ab
#
# One run, four phases, all measured by the player's own `avsync` sampler
# (`BILI_AVSYNC_TEST`, 10 samples/s, `src/qa/f211.rs`):
#
#   play    0 .. 12 s            steady state
#   resume  pause at 12 s, hold 2.5 s, release        (BILI_PAUSE_AT)
#   seek    a real seek through `PlaybackControl::seek` (BILI_SEEK_AT)
#   switch  a real 画质 pick through the menu callback  (BILI_QUALITY_SWITCH_TEST)
#
# The delta is `audio_clock − presented_picture`, i.e. positive = sound ahead.
param(
    [string]$Exe = "target/release/o11a1.exe",
    [ValidateSet("mf", "ffmpeg")]
    [string]$Decoder = "mf",
    [ValidateSet("0", "1")]
    [string]$Dcomp = "1",
    [string]$Bvid = "BV1BBnb6aEww",
    [int]$PauseAt = 12,
    [int]$SeekAt = 22,
    [int]$SeekTo = 90,
    [int]$SwitchAt = 32,
    [int]$SwitchQn = 64,
    [int]$Seconds = 44,
    [string]$Tag = "main",
    [string]$OutDir = "target/qa/f211"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$OutDir = if ([System.IO.Path]::IsPathRooted($OutDir)) { $OutDir } else { Join-Path $root $OutDir }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$Log = Join-Path $OutDir ("f211_avsync_{0}_dc{1}_{2}.log" -f $Decoder, $Dcomp, $Tag)
if (Test-Path $Log) { Remove-Item $Log -Force }

$exePath = if ([System.IO.Path]::IsPathRooted($Exe)) { $Exe } else { Join-Path $root $Exe }
if (-not (Test-Path $exePath)) { throw "找不到 $exePath（先 cargo build --release）" }

Get-Process o11a1 -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 400

$env:BILI_DEBUG_LOG = $Log
$env:BILI_OPEN_BVID = $Bvid
$env:BILI_QUALITY_PICK = "0,6000"
$env:BILI_PLAYER_DCOMP = $Dcomp
$env:BILI_AVSYNC_TEST = "avsync"
$env:BILI_PAUSE_AT = "$PauseAt,2500"
$env:BILI_SEEK_AT = "$SeekTo"
$env:BILI_SEEK_AT_DELAY_MS = "$($SeekAt * 1000)"
$env:BILI_SEEK_AT_EVERY_MS = "3000"
$env:BILI_QUALITY_SWITCH_TEST = "$SwitchQn"
$env:BILI_QUALITY_SWITCH_MS = "$($SwitchAt * 1000)"
if ($Decoder -eq "ffmpeg") { $env:BILI_DECODE = "ffmpeg" } else { Remove-Item Env:BILI_DECODE -ErrorAction SilentlyContinue }
Remove-Item Env:BILI_QA_MF_FILE -ErrorAction SilentlyContinue

Write-Host ("f211 avsync decoder={0} dcomp={1} pause={2}s seek={3}s->{4}s switch={5}s qn={6} run={7}s" -f `
    $Decoder, $Dcomp, $PauseAt, $SeekAt, $SeekTo, $SwitchAt, $SwitchQn, $Seconds)
Write-Host ("log {0}" -f $Log)

$proc = Start-Process -FilePath $exePath -WorkingDirectory $root -PassThru
try {
    Start-Sleep -Seconds $Seconds
} finally {
    if ($proc -and -not $proc.HasExited) {
        $proc.Kill()
        $proc.WaitForExit(8000) | Out-Null
    }
}

Write-Host ""
Write-Host "---- phases ----"
if (Test-Path $Log) {
    Select-String -Path $Log -Pattern "pause-at |seek served |seek trace |session first present |decode=mf " |
        ForEach-Object { Write-Host $_.Line }
    Write-Host ""
    & python (Join-Path $PSScriptRoot "qa-f211-report.py") --avsync $Log
} else {
    Write-Host "没有日志 $Log"
}
