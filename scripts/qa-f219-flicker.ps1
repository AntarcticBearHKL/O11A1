# f219 QA: does every presented slot draw the frame it picked?
#
#   powershell -File scripts/qa-f219-flicker.ps1                # 1080p, 4x, 40 frames + 0.8 -> 0 pair check
#   powershell -File scripts/qa-f219-flicker.ps1 -Interp 0      # 插帧关
#   powershell -File scripts/qa-f219-flicker.ps1 -Soak -Seconds 70 -Frames 4
#
# This is `qa-f218-flicker.ps1 -Slots` (the same ramp sample, the same
# per-slot `pts`/`luma` trace, the same 40-frame luma table) plus the two things
# f219 is judged on:
#
#   * `scripts/qa-f219-pairs.py` over the run's log: every decoded pair's `pts`
#     must match the luma its slots actually drew. Before f219 the decoder was
#     free to refill a surface the sink still held, so ~0.8 % of pairs drew a
#     frame 10-20 positions away (measured: 10 of 1178 on the f218 binary).
#   * `f200 layers` per-second line: `video_s`, `dm_s`, `mf_drop`, so the fix is
#     not paid for with stutter or with a starved decoder (`-Soak` keeps the
#     slots trace off and just plays for `-Seconds`).
param(
    [ValidateSet("720", "1080")]
    [string]$Stage = "1080",
    [ValidateSet("0", "3")]
    [string]$Interp = "3",
    [string]$Exe = "target/release/o11a1.exe",
    [int]$Seconds = 30,
    [int]$Frames = 40,
    [int]$Interval = 30,
    # A 120 s ramp, not the f218 script's 30 s one: the 40-frame capture starts
    # at `Seconds - 6`, so a 30 s sample would already be at EOS and the "before
    # vs after" luma table would compare two frozen end frames.
    [string]$SamplePath = "target/f219/ramp1080p30-120s.mp4",
    [string]$OutDir = "target/qa/f219",
    # 0 = read the window with `PrintWindow(2)` (works unattended, DWM renders
    # the composition); 1 = read the screen and require the window on top.
    [switch]$Screen,
    # `BILI_OPEN_BVID` for the child script: the player window is opened by the
    # real feed path, and `BILI_QA_MF_FILE` then makes it play the local sample.
    [string]$Bvid = "BV1BBnb6aEww",
    # The luma tolerance for "the right frame". One ramp step is ~9.3 grey
    # levels, so 5 is half a frame.
    [double]$Tolerance = 5.0,
    # Don't run the pair analysis (a plain playback soak: slots trace off).
    [switch]$Soak
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

$samplePath = if ([System.IO.Path]::IsPathRooted($SamplePath)) { $SamplePath } else { Join-Path $root $SamplePath }
$outDir = if ([System.IO.Path]::IsPathRooted($OutDir)) { $OutDir } else { Join-Path $root $OutDir }
if (-not (Test-Path $samplePath)) {
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $samplePath) | Out-Null
    Write-Host ("f219 ramp generating {0} (1920x1080, 120 s)" -f $samplePath)
    & ffmpeg -hide_banner -loglevel error -y -f lavfi -i "color=c=black:s=1920x1080:r=30:d=120,format=yuv420p,geq=lum='8+8*mod(N\,30)':cb=128:cr=128" `
        -c:v libx264 -pix_fmt yuv420p -profile:v high -preset veryfast -crf 18 -g 30 $samplePath
}
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
$started = Get-Date

# The layer sampler: `video_s` / `dm_s` / `mf_drop` per second. The child
# `qa-f218-flicker.ps1` does not clear this variable, so it survives the run.
$env:BILI_F200_LAYERS = [string]([math]::Max(60, $Seconds + 30))

# PowerShell drops an empty string from an argument list, so "no bvid" is a
# single space: the app trims it and skips the hook.
$bvidArg = if ($Bvid.Trim().Length -eq 0) { " " } else { $Bvid }

$forward = @(
    "-Stage", $Stage,
    "-Interp", $Interp,
    "-Exe", $Exe,
    "-Seconds", [string]$Seconds,
    "-Frames", [string]$Frames,
    "-Interval", [string]$Interval,
    "-SamplePath", $samplePath,
    "-OutDir", $outDir,
    "-Bvid", $bvidArg
)
if (-not $Soak) { $forward += "-Slots" }
if (-not $Screen) { $forward += "-Print" }

& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot "qa-f218-flicker.ps1") @forward
$code = $LASTEXITCODE

$log = Get-ChildItem -Path $outDir -Filter "*.log" -ErrorAction SilentlyContinue |
    Where-Object { $_.LastWriteTime -ge $started } |
    Sort-Object LastWriteTime |
    Select-Object -Last 1
if (-not $log) {
    Write-Host "f219: 这一轮没有日志，无法判定"
    exit 1
}
Write-Host ("f219 log {0}" -f $log.FullName)

# Per-second layer numbers: steady state only (the first two lines include the
# open/prime of the session).
$layers = Select-String -Path $log.FullName -Pattern "f200 layers" | ForEach-Object { $_.Line }
if ($layers) {
    $videos = @()
    $dms = @()
    $drops = @()
    foreach ($line in $layers) {
        if ($line -match "video_s=([\d.]+)") { $videos += [double]$Matches[1] }
        if ($line -match "dm_s=([\d.]+)") { $dms += [double]$Matches[1] }
        if ($line -match "mf_drop=([\d.]+)") { $drops += [double]$Matches[1] }
    }
    if ($videos.Count -gt 2) {
        $steady = $videos[2..($videos.Count - 1)]
        $vmin = ($steady | Measure-Object -Minimum).Minimum
        $vmed = ($steady | Sort-Object)[[int]($steady.Count / 2)]
        $dsteady = if ($dms.Count -gt 2) { $dms[2..($dms.Count - 1)] } else { $dms }
        $dmin = ($dsteady | Measure-Object -Minimum).Minimum
        $dmed = ($dsteady | Sort-Object)[[int]($dsteady.Count / 2)]
        $dropSum = ($drops | Measure-Object -Sum).Sum
        Write-Host ("f219 layers video_s med={0:N1} min={1:N1} | dm_s med={2:N1} min={3:N1} | mf_drop sum={4:N1}" -f $vmed, $vmin, $dmed, $dmin, $dropSum)
        Write-Host ("f219 layers samples={0} (per second; 修前基线 video_s>=115 dm_s>=55)" -f $videos.Count)
    }
} else {
    Write-Host "f219: 日志里没有 f200 layers（BILI_F200_LAYERS 没生效？）"
}

if ($Soak) {
    Write-Host ""
    Write-Host "---- soak: check the playback rate never dips ----"
    if (-not $layers) {
        Write-Host "f219 soak: 没有 f200 layers 采样，这一轮不可判定"
        exit 1
    }
    $slow = @()
    $steady = @()
    foreach ($line in $layers) {
        # Two separate matches: `$Matches` only holds the *last* one, so reading
        # both groups after an `-and` chain reads the wrong line's numbers.
        if ($line -notmatch "t=(\d+)s") { continue }
        $t = [int]$Matches[1]
        if ($line -notmatch "video_s=([\d.]+)") { continue }
        $v = [double]$Matches[1]
        # The harness itself resizes/raises the window during the first seconds,
        # which costs presents in any build; the soak is about playback after it.
        if ($t -ge 10) {
            $steady += $v
            if ($v -lt 110) { $slow += $line }
        }
    }
    if ($steady.Count -lt 20) {
        Write-Host ("f219 soak: only {0} steady seconds sampled, 不可判定" -f $steady.Count)
        exit 1
    }
    if ($slow.Count -gt 0) {
        Write-Host ("f219 soak: {0}/{1} 秒 video_s < 110（最低 {2:N1}）" -f $slow.Count, $steady.Count, (($steady | Measure-Object -Minimum).Minimum))
        $slow | Select-Object -First 5 | ForEach-Object { Write-Host $_.Substring(0, [math]::Min(160, $_.Length)) }
        exit 1
    }
    Write-Host ("f219 soak: {0} 个稳定秒 video_s 全部 >= 110（最低 {1:N1}，中位 {2:N1}）" -f $steady.Count, (($steady | Measure-Object -Minimum).Minimum), (($steady | Sort-Object)[[int]($steady.Count / 2)]))
    exit $code
}

Write-Host ""
Write-Host "---- f219 pairs (pts <-> luma) ----"
& python (Join-Path $PSScriptRoot "qa-f219-pairs.py") $log.FullName --tolerance $Tolerance
$pairs = $LASTEXITCODE
if ($code -ne 0 -and $pairs -eq 0) { $pairs = $code }
exit $pairs
