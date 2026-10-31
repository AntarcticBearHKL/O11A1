# QA helper for f211: the five entries, on the zero-copy path.
#
#   powershell -File scripts/qa-f211-entries.ps1 -Entry live
#   powershell -File scripts/qa-f211-entries.ps1 -Entry pgc -Seconds 30
#
# One process per entry, each driven by the shipped hook for that page
# (`BILI_LIVE_TEST=room`, `BILI_PGC_TEST`, `BILI_OPEN_BVID` + `BILI_SEARCH_LOOP`,
# `BILI_F143_TEST=fish`), so what the log shows is what a user's click does.
#
# The question for every entry is the same: does the zero-copy path come up, and
# where it cannot (a live FLV/fMP4 stream) does the ffmpeg fallback happen
# without the user seeing anything different?
param(
    [ValidateSet("vod", "live", "pgc", "fish", "search")]
    [string]$Entry = "vod",
    [string]$Exe = "target/release/o11a1.exe",
    [string]$Bvid = "BV1BBnb6aEww",
    [ValidateSet("0", "1")]
    [string]$Dcomp = "1",
    [int]$Seconds = 40,
    [string]$Tag = "",
    [string]$OutDir = "target/qa/f211"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$OutDir = if ([System.IO.Path]::IsPathRooted($OutDir)) { $OutDir } else { Join-Path $root $OutDir }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$suffix = if ($Tag) { "_$Tag" } else { "" }
$Log = Join-Path $OutDir ("f211_entry_{0}{1}.log" -f $Entry, $suffix)
if (Test-Path $Log) { Remove-Item $Log -Force }

$exePath = if ([System.IO.Path]::IsPathRooted($Exe)) { $Exe } else { Join-Path $root $Exe }
if (-not (Test-Path $exePath)) { throw "找不到 $exePath（先 cargo build --release）" }

Get-Process o11a1 -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 400

$env:BILI_DEBUG_LOG = $Log
$env:BILI_PLAYER_DCOMP = $Dcomp
$env:BILI_QUALITY_PICK = "0,6000"
$env:BILI_AVSYNC_TEST = "entry"
Remove-Item Env:BILI_DECODE -ErrorAction SilentlyContinue
Remove-Item Env:BILI_QA_MF_FILE -ErrorAction SilentlyContinue
foreach ($name in @("BILI_OPEN_BVID", "BILI_LIVE_TEST", "BILI_PGC_TEST", "BILI_F143_TEST", "BILI_SEARCH_TEST", "BILI_SEARCH_LOOP", "BILI_FISH_BVID", "BILI_DETAIL_ON_START")) {
    Remove-Item ("Env:" + $name) -ErrorAction SilentlyContinue
}

switch ($Entry) {
    "vod" {
        $env:BILI_OPEN_BVID = $Bvid
    }
    "live" {
        # 直播: the real room stage — this is the FLV/fMP4 case.
        $env:BILI_LIVE_TEST = "room"
    }
    "pgc" {
        $env:BILI_PGC_TEST = "1"
    }
    "fish" {
        $env:BILI_F143_TEST = "fish"
    }
    "search" {
        # A real search, then open the first result's player from the result grid.
        $env:BILI_SEARCH_TEST = "原神"
        $env:BILI_SEARCH_LOOP = "1,1500,player"
    }
}

Write-Host ("f211 entry={0} dcomp={1} run={2}s log={3}" -f $Entry, $Dcomp, $Seconds, $Log)
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
Write-Host "---- decode ----"
if (Test-Path $Log) {
    Select-String -Path $Log -Pattern "decode=mf |decode fallback|tv stream|live-test |player running|stream qn=|startup first present|first present ms=|f200 layers" |
        Select-Object -Last 24 | ForEach-Object { Write-Host $_.Line }
} else {
    Write-Host "没有日志 $Log"
}
