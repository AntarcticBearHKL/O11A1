# f215: the scroll / morph table for the wallpaper-sampled surfaces.
# Developer tool, not product code.
#
#   powershell -File scripts/qa-f215-perf.ps1
#   powershell -File scripts/qa-f215-perf.ps1 -Sizes 1920x1080 -Reps 12
#
# One launch per size, both card variants inside the SAME process (so the machine
# load is shared): `flat` = cards_flat (the pre-f215 plate, the "before"), then
# `sample` = the shipped wallpaper-sampled card ("after"). Each variant runs the
# f199 scroll protocol (3 x 3 s of real wheel input, pooled) and the f159 morph
# protocol (N x open/hold/close on the 180 ms morph clock).
#
# Every number is the 4 ms UI-thread probe's gap distribution:
#   scroll  p50 / p95 / p99 / max + the count above 33 ms
#   morph   p50 / p95 / p99 / max of the pooled gaps + the median per-rep p95
#
# The wallpaper sample is ON for the whole scroll in BOTH variants (-Only the
# card plate changes), so the "after" row is what a user gets; `flat` is the
# same build with `BILI_CARD_FLAT=1`.
param(
    [string]$Exe = "target/release/o11a1.exe",
    [string]$Sizes = "1280x800,1920x1080",
    [int]$Reps = 12,
    [string]$Variants = "flat,sample",
    [string]$LogDir = "target/qa"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$exePath = if ([System.IO.Path]::IsPathRooted($Exe)) { $Exe } else { Join-Path $root $Exe }
if (-not (Test-Path $exePath)) { throw "missing $exePath (run cargo build --release first)" }
$logDirPath = if ([System.IO.Path]::IsPathRooted($LogDir)) { $LogDir } else { Join-Path $root $LogDir }
New-Item -ItemType Directory -Force -Path $logDirPath | Out-Null

$rows = New-Object System.Collections.Generic.List[object]

foreach ($size in ($Sizes -split ",")) {
    $size = $size.Trim()
    if ($size -eq "") { continue }
    $log = Join-Path $logDirPath ("f215_perf_" + ($size -replace "[^0-9]", "x") + ".log")
    if (Test-Path $log) { Remove-Item $log -Force }
    $env:BILI_DEBUG_LOG = $log
    $env:BILI_F215_PERF = "both"
    $env:BILI_F215_VARIANTS = $Variants
    $env:BILI_F215_SIZE = $size
    $env:BILI_F215_REPS = "$Reps"
    Write-Host ("`n=== " + $size + " variants=" + $Variants + " reps=" + $Reps)
    $proc = Start-Process -FilePath $exePath -WorkingDirectory $root -PassThru
    # login + feed + per-variant (scroll 3x3s + morph) with generous slack
    $wait = 120 + ($Variants.Split(",").Count * ($Reps * 2 + 30))
    for ($i = 0; $i -lt $wait; $i++) {
        Start-Sleep -Seconds 1
        if ($proc.HasExited) { break }
        if (Test-Path $log) {
            $done = Get-Content $log -ErrorAction SilentlyContinue | Select-String -Pattern "f215 perf done"
            if ($done) { Start-Sleep -Seconds 1; break }
        }
    }
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
    Get-Content $log -ErrorAction SilentlyContinue |
        Select-String -Pattern "f215 perf phase" |
        ForEach-Object {
            $line = $_.Line
            Write-Host ("  " + $line)
            $m = [regex]::Match($line, 'phase=(\w+) variant=(\w+) size=(\S+) samples=(\d+) p50_ms=([\d.]+) p95_ms=([\d.]+) p99_ms=([\d.]+) max_ms=([\d.]+)(?: over33ms=(\d+))?')
            if ($m.Success) {
                $rows.Add([pscustomobject]@{
                    size = $m.Groups[3].Value
                    phase = $m.Groups[1].Value
                    variant = $m.Groups[2].Value
                    samples = [int]$m.Groups[4].Value
                    p50 = [double]$m.Groups[5].Value
                    p95 = [double]$m.Groups[6].Value
                    p99 = [double]$m.Groups[7].Value
                    max = [double]$m.Groups[8].Value
                    over33 = if ($m.Groups[9].Success) { [int]$m.Groups[9].Value } else { "" }
                })
            }
        }
}

Write-Host "`n==== f215 perf ===="
$rows | Format-Table -AutoSize | Out-String | Write-Host

$bad = $rows | Where-Object { $_.phase -eq "scroll" -and $_.variant -eq "sample" -and ($_.p95 -gt 10.0 -or ($_.over33 -is [int] -and $_.over33 -gt 0)) }
if ($bad) {
    Write-Host ("BUDGET MISS on: " + (($bad | ForEach-Object { $_.size }) -join ", "))
    Write-Host "  (re-run on an idle machine before treating it as a product regression:"
    Write-Host "   the flat baseline in the same run moves with the system load too.)"
} else {
    Write-Host "budget: scroll p95 <= 10 ms and 0 frames over 33 ms on every measured size"
}
