# f215: the surface screenshots (light | dark montages) and the cursor-parked
# playlist capture. Developer tool, not product code.
#
#   powershell -File scripts/qa-f215-shots.ps1 -Exe target/release/o11a1.exe
#
# Three launches, each with its own log under target/qa:
#   1. cards,toolbar,sidebar,seam  - the cursor is parked in the window's
#      top-left corner, so no hover-only surface is open.
#   2. popup                       - the cursor is parked on the home bar's vinyl,
#      which is what opens the playlist (its state machine reads the REAL cursor, not
#      a synthetic event); the account popover is opened by the driver.
#   3. player                      - BILI_OPEN_BVID + the local sample file, with
#      the right strip and the volume/danmaku popovers held open by the driver.
#
# The pairs are then composed into the files the report cites:
#   docs/f215_home_cards.png  docs/f215_toolbar.png  docs/f215_popup.png
#   docs/f215_sidebar.png     docs/f215_player_chrome.png
# plus the raw `f215_*_<theme>.png` captures in the capture folder.
param(
    [string]$Exe = "target/release/o11a1.exe",
    [string]$CaptureDir = "target/qa/f215",
    [string]$ShotDir = "docs",
    [string]$Bvid = "BV1BBnb6aEww",
    [int]$BootMs = 12000,
    # all | surfaces | popup | player | montage
    [string]$Only = "all",
    [switch]$KeepRaw
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
if (-not ("F215Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public class F215Win {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT point);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int max);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    public static List<IntPtr> Windows(uint pid) {
        var list = new List<IntPtr>();
        EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == pid && IsWindowVisible(h)) list.Add(h); return true; }, IntPtr.Zero);
        return list;
    }
    public static string Class(IntPtr h) { var sb = new StringBuilder(512); GetClassName(h, sb, 512); return sb.ToString(); }
}
"@
}

$root = Split-Path -Parent $PSScriptRoot
$exePath = if ([System.IO.Path]::IsPathRooted($Exe)) { $Exe } else { Join-Path $root $Exe }
$capDir = if ([System.IO.Path]::IsPathRooted($CaptureDir)) { $CaptureDir } else { Join-Path $root $CaptureDir }
$shotDir = if ([System.IO.Path]::IsPathRooted($ShotDir)) { $ShotDir } else { Join-Path $root $ShotDir }
New-Item -ItemType Directory -Force -Path $capDir | Out-Null
New-Item -ItemType Directory -Force -Path $shotDir | Out-Null
if (-not (Test-Path $exePath)) { throw "missing $exePath (run cargo build --release first)" }

$saved = New-Object F215Win+POINT
[void][F215Win]::GetCursorPos([ref]$saved)

function Find-MainWindow([int]$ProcessId, [int]$TimeoutMs) {
    $deadline = (Get-Date).AddMilliseconds($TimeoutMs)
    while ((Get-Date) -lt $deadline) {
        foreach ($h in [F215Win]::Windows([uint32]$ProcessId)) {
            if ([F215Win]::Class($h) -eq "O11A1DcompPlayer") { continue }
            $r = New-Object F215Win+RECT
            [void][F215Win]::GetWindowRect($h, [ref]$r)
            if (($r.Right - $r.Left) -ge 600 -and ($r.Bottom - $r.Top) -ge 400) { return $h }
        }
        Start-Sleep -Milliseconds 300
    }
    return [IntPtr]::Zero
}

function Invoke-ShotRun([string]$Name, [string]$Stages, [string]$Cursor, [hashtable]$ExtraEnv) {
    $log = Join-Path $root ("target/qa/f215_" + $Name + ".log")
    if (Test-Path $log) { Remove-Item $log -Force }
    $env:BILI_DEBUG_LOG = $log
    $env:BILI_CAPTURE_DIR = $capDir
    $env:BILI_F215_SHOT = $Stages
    $env:BILI_F215_SIZE = "1280x800"
    foreach ($key in $ExtraEnv.Keys) { Set-Item -Path ("Env:" + $key) -Value $ExtraEnv[$key] }
    $proc = Start-Process -FilePath $exePath -WorkingDirectory $root -PassThru
    try {
        $hwnd = Find-MainWindow $proc.Id 60000
        if ($hwnd -eq [IntPtr]::Zero) { throw "no main window for $Name" }
        $r = New-Object F215Win+RECT
        [void][F215Win]::GetWindowRect($hwnd, [ref]$r)
        # Park the pointer BEFORE the settle sleep: the driver starts its stages
        # as soon as login lands, and 播放列表 needs the vinyl hover to be seen
        # from the first of them (a later SetCursorPos races the capture).
        if ($Cursor -eq "vinyl") {
            # Home bar vinyl: window centre, 44px above the bottom edge.
            [void][F215Win]::SetCursorPos([int](($r.Left + $r.Right) / 2), ($r.Bottom - 44))
        } elseif ($Cursor -eq "away") {
            [void][F215Win]::SetCursorPos($r.Left + 6, $r.Top + 6)
        }
        Start-Sleep -Milliseconds $BootMs
        Write-Host ("  run {0} stages={1} hwnd={2} rect={3},{4} {5}x{6} cursor={7}" -f `
            $Name, $Stages, $hwnd, $r.Left, $r.Top, ($r.Right - $r.Left), ($r.Bottom - $r.Top), $Cursor)
        # The driver's own timeline (login wait + stages); a popup run needs the
        # vinyl hover to be seen before the capture, hence the longer wait.
        Start-Sleep -Seconds $(if ($Name -eq "player") { 95 } elseif ($Name -eq "popup") { 60 } else { 80 })
        Get-Content $log -ErrorAction SilentlyContinue |
            Select-String -Pattern "f215 shot|capture png|f215 perf" |
            ForEach-Object { Write-Host ("  LOG " + $_.Line) }
    } finally {
        if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
        Start-Sleep -Milliseconds 800
        foreach ($key in $ExtraEnv.Keys) { Remove-Item -Path ("Env:" + $key) -ErrorAction SilentlyContinue }
    }
}

function Build-Pair([string]$stem, [string]$target, [int]$height) {
    $paths = @()
    $labels = @()
    foreach ($theme in @("light", "dark")) {
        $path = Join-Path $capDir ("{0}_{1}.png" -f $stem, $theme)
        if (Test-Path $path) { $paths += $path; $labels += $theme }
    }
    if ($paths.Count -lt 2) { Write-Host ("  skip {0} (missing captures)" -f $target); return }
    $images = @()
    foreach ($path in $paths) { $images += [System.Drawing.Image]::FromFile($path) }
    $widths = @()
    foreach ($image in $images) { $widths += [int][Math]::Round($image.Width * ($height / $image.Height)) }
    $pad = 14
    $title = 26
    $totalW = [int](($widths | Measure-Object -Sum).Sum + $pad * ($images.Count + 1))
    $canvas = New-Object System.Drawing.Bitmap -ArgumentList $totalW, ($height + $title + $pad * 2)
    $g = [System.Drawing.Graphics]::FromImage($canvas)
    $g.Clear([System.Drawing.Color]::FromArgb(12, 12, 16))
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $font = New-Object System.Drawing.Font("Consolas", 12)
    $brush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(235, 235, 240))
    $x = $pad
    for ($i = 0; $i -lt $images.Count; $i++) {
        $g.DrawImage($images[$i], $x, $pad, $widths[$i], $height)
        $g.DrawString($labels[$i], $font, $brush, [single]$x, [single]($pad + $height + 4))
        $x += $widths[$i] + $pad
    }
    $out = Join-Path $shotDir $target
    $canvas.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose()
    $canvas.Dispose()
    foreach ($image in $images) { $image.Dispose() }
    Write-Host ("  montage {0} ({1})" -f $out, ($labels -join " | "))
}

$want = { param($stage) $Only -eq "all" -or $Only -eq $stage }
try {
    if (& $want "surfaces") {
        Write-Host "`n=== surfaces (cards / toolbar / sidebar / seam)"
        Invoke-ShotRun "surfaces" "cards,toolbar,sidebar,seam" "away" @{}
    }

    if (& $want "popup") {
        Write-Host "`n=== popup (playlist + account popover), cursor parked on the vinyl"
        Invoke-ShotRun "popup" "popup" "vinyl" @{}
    }

    if (& $want "player") {
        Write-Host "`n=== player chrome + info strip + volume/danmaku popovers"
        Invoke-ShotRun "player" "player" "away" @{
            BILI_OPEN_BVID = $Bvid
            BILI_QUALITY_PICK = "0,6000"
            BILI_QA_MF_FILE = Join-Path $root "target/f209/sample1080p30.mp4"
        }
    }
} finally {
    [void][F215Win]::SetCursorPos($saved.X, $saved.Y)
}

if ($Only -eq "montage" -or $Only -eq "all") {
    Write-Host "`n=== montages"
    Build-Pair "f215_home_cards" "f215_home_cards.png" 480
    Build-Pair "f215_toolbar" "f215_toolbar.png" 480
    Build-Pair "f215_popup" "f215_popup.png" 480
    Build-Pair "f215_sidebar" "f215_sidebar.png" 480
    Build-Pair "f215_player_chrome" "f215_player_chrome.png" 430
}
if (-not $KeepRaw) { Write-Host ("  raw captures are in {0}" -f $capDir) }
Write-Host "`nf215 shots done"
