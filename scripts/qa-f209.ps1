# QA helper for f209: the zero-copy (Media Foundation -> NV12 texture) decode
# path, end to end, on a local 1080p30 sample.
#
#   powershell -File scripts/qa-f209.ps1                     # 1080p, MF decoder
#   powershell -File scripts/qa-f209.ps1 -Stage 720
#   powershell -File scripts/qa-f209.ps1 -Decoder ffmpeg     # the A/B arm
#
# Why a local sample: the account on this machine cannot reach 1080p any more
# (its session expired, and the anonymous tiers cap at 480p), so the acceptance
# run cannot come from a DASH URL. The sample is *real* H.264 1080p30
# (`testsrc2` encoded with libx264, yuv420p, profile high): the same bitstream
# shape, the same decoder and the same surface pool as a stream - only the byte
# transport is a file instead of HTTP ranges.
#
# The app still opens a normal session (BILI_OPEN_BVID); BILI_QA_MF_FILE only
# redirects the video bytes, and the sample has no audio, so the pacer paces on
# the wall clock exactly like a silent stream.
param(
    [ValidateSet("720", "1080")]
    [string]$Stage = "1080",
    [string]$Exe = "target/release/o11a1.exe",
    [ValidateSet("mf", "ffmpeg")]
    [string]$Decoder = "mf",
    [string]$Bvid = "BV1BBnb6aEww",
    [int]$Seconds = 32,
    [string]$OutDir = "target/qa/f209"
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

if (-not ("F209Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public class F209Win {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int cmd);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr hWnd, ref POINT point);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int max);

    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }

    public static List<IntPtr> Windows(uint pid) {
        var list = new List<IntPtr>();
        EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == pid && IsWindowVisible(h)) list.Add(h); return true; }, IntPtr.Zero);
        return list;
    }
    public static string Class(IntPtr h) { var sb = new StringBuilder(512); GetClassName(h, sb, 512); return sb.ToString(); }
    public static int ClientWidth(IntPtr h) { RECT r; GetClientRect(h, out r); return r.Right - r.Left; }
    public static int ClientHeight(IntPtr h) { RECT r; GetClientRect(h, out r); return r.Bottom - r.Top; }
}
"@
}

$root = Split-Path -Parent $PSScriptRoot
$OutDir = if ([System.IO.Path]::IsPathRooted($OutDir)) { $OutDir } else { Join-Path $root $OutDir }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$clientW = if ($Stage -eq "1080") { 1920 } else { 1280 }
$clientH = if ($Stage -eq "1080") { 1080 } else { 720 }
$sample = Join-Path $root ("target/f209/sample{0}p30.mp4" -f $Stage)
$Log = Join-Path $OutDir ("f209_{0}_{1}.log" -f $Stage, $Decoder)
if (Test-Path $Log) { Remove-Item $Log -Force }

if (-not (Test-Path $sample)) {
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $sample) | Out-Null
    $size = "{0}x{1}" -f $clientW, $clientH
    Write-Host ("f209 sample generating {0} ({1})" -f $sample, $size)
    & ffmpeg -hide_banner -loglevel error -y -f lavfi -i "testsrc2=size=${size}:rate=30" -t 60 `
        -c:v libx264 -pix_fmt yuv420p -profile:v high -preset veryfast -crf 20 $sample
} else {
    Write-Host ("f209 sample {0}" -f $sample)
}

# Any player left from an earlier run would publish its own counters into the
# same log; this run owns the numbers it reports.
Get-Process o11a1 -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 400

$env:BILI_DEBUG_LOG = $Log
$env:BILI_OPEN_BVID = $Bvid
$env:BILI_QUALITY_PICK = "0,6000"
$env:BILI_PLAYER_DCOMP = "1"
$env:BILI_DCOMP_DANMAKU = "1"
# 光流 (mode 3) at 4x: the only interpolation the zero-copy path can do (the CPU
# blend needs frames that no longer exist).
$env:BILI_INTERP_TEST = "3"
$env:BILI_INTERP_4X = "1"
$env:BILI_F200_LAYERS = "$Seconds"
$env:BILI_QA_MF_FILE = $sample
if ($Decoder -eq "ffmpeg") {
    $env:BILI_DECODE = "ffmpeg"
} else {
    Remove-Item Env:BILI_DECODE -ErrorAction SilentlyContinue
}
foreach ($name in @("BILI_PANEL_TEST", "BILI_FULLSCREEN_TEST", "BILI_DCOMP_UI_FORCE", "BILI_WARP_TRACE", "BILI_SOAK_TEST", "BILI_PACE_TRACE")) {
    Remove-Item ("Env:" + $name) -ErrorAction SilentlyContinue
}

$exePath = if ([System.IO.Path]::IsPathRooted($Exe)) { $Exe } else { Join-Path $root $Exe }
if (-not (Test-Path $exePath)) { throw "找不到 $exePath（先 cargo build --release）" }
Write-Host ("f209 exe {0} decoder={1} log={2}" -f $exePath, $Decoder, $Log)

$proc = Start-Process -FilePath $exePath -WorkingDirectory $root -PassThru

function Find-Player($proc, $timeoutMs) {
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    while ((Get-Date) -lt $deadline) {
        foreach ($h in [F209Win]::Windows([uint32]$proc.Id)) {
            if ([F209Win]::Class($h) -eq "O11A1DcompPlayer") { return $h }
        }
        Start-Sleep -Milliseconds 300
    }
    return [IntPtr]::Zero
}

try {
    $hwnd = Find-Player $proc 70000
    if ($hwnd -eq [IntPtr]::Zero) { throw "70 秒内没有播放器窗口" }
    Write-Host ("f209 player hwnd={0}" -f $hwnd)

    # Client area exactly the stage size (the picture is one pixel per frame
    # pixel), and the window on top: the composite picture is scan-out, so the
    # capture is a screen read and anything covering it would be what the file
    # shows.
    $outer = New-Object F209Win+RECT
    [void][F209Win]::GetWindowRect($hwnd, [ref]$outer)
    $deltaW = ($outer.Right - $outer.Left) - [F209Win]::ClientWidth($hwnd)
    $deltaH = ($outer.Bottom - $outer.Top) - [F209Win]::ClientHeight($hwnd)
    for ($pass = 0; $pass -lt 10; $pass++) {
        # HWND_TOPMOST, no SWP_NOZORDER: the z-order change is the point.
        [void][F209Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, ($clientW + $deltaW), ($clientH + $deltaH), 0x0010 -bor 0x0040)
        Start-Sleep -Milliseconds 300
        if ([F209Win]::ClientWidth($hwnd) -eq $clientW -and [F209Win]::ClientHeight($hwnd) -eq $clientH) { break }
    }
    [void][F209Win]::ShowWindow($hwnd, 9)
    # HWND_TOPMOST without SWP_NOZORDER: the capture is a screen read, so the
    # player has to be the window on top when it happens (the same call
    # `qa-f207.ps1` uses).
    [void][F209Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0010)
    [void][F209Win]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 900
    Write-Host ("f209 client {0}x{1}" -f [F209Win]::ClientWidth($hwnd), [F209Win]::ClientHeight($hwnd))

    Write-Host ("f209 playing {0}s ..." -f $Seconds)
    Start-Sleep -Seconds $Seconds
    [void][F209Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0010)
    [void][F209Win]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 700

    # The window under this handle may have been recreated during the run (a
    # session restart rebuilds the player window), in which case the old handle
    # reports a nonsense rect. Re-find it by class before reading anything.
    $fresh = Find-Player $proc 3000
    if ($fresh -ne [IntPtr]::Zero) { $hwnd = $fresh }
    # Restore + size one last time: the window is placed at (0,0) and its client
    # is exactly the stage, so the capture is the screen's top-left corner - the
    # window is TOPMOST at that moment, which is what makes the read honest.
    [void][F209Win]::ShowWindow($hwnd, 9)
    [void][F209Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, ($clientW + $deltaW), ($clientH + $deltaH), 0x0010 -bor 0x0040)
    [void][F209Win]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 900
    $origin = New-Object F209Win+POINT
    [void][F209Win]::ClientToScreen($hwnd, [ref]$origin)
    $cw = [F209Win]::ClientWidth($hwnd)
    $ch = [F209Win]::ClientHeight($hwnd)
    if ($cw -ne $clientW -or $ch -ne $clientH) {
        Write-Host ("f209 warn: client {0}x{1} != stage {2}x{3}; capturing from the client origin anyway" -f $cw, $ch, $clientW, $clientH)
    }
    $cw = $clientW
    $ch = $clientH
    $shot = Join-Path $root ("docs/f209_{0}p120.png" -f $Stage)
    if ($Stage -ne "1080") { $shot = Join-Path $root ("docs/f209_{0}p.png" -f $Stage) }
    if ($Decoder -eq "ffmpeg") { $shot = Join-Path $root ("docs/f209_{0}p_ffmpeg.png" -f $Stage) }
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $shot) | Out-Null
    $bitmap = New-Object System.Drawing.Bitmap $cw, $ch
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.CopyFromScreen($origin.X, $origin.Y, 0, 0, $bitmap.Size)
    $bitmap.Save($shot, [System.Drawing.Imaging.ImageFormat]::Png)
    $graphics.Dispose()
    $bitmap.Dispose()
    Write-Host ("f209 screenshot {0} ({1}x{2})" -f $shot, $cw, $ch)
} finally {
    if ($proc -and -not $proc.HasExited) {
        Start-Sleep -Milliseconds 400
        $proc.Kill()
        $proc.WaitForExit(5000) | Out-Null
    }
}

Write-Host ""
Write-Host "---- decode / interp ----"
if (Test-Path $Log) {
    Select-String -Path $Log -Pattern "decode=mf|decode fallback|decode frames=|interp plan|interp degrade|ofa gpu warp active|dcomp warp ready|dcomp mf |nv12 convert|dcomp video layer frame" |
        Select-Object -Last 20 | ForEach-Object { Write-Host $_.Line }
    Write-Host ""
    Write-Host "---- per-second ----"
    Select-String -Path $Log -Pattern "f200 layers" | Select-Object -Last 34 | ForEach-Object { Write-Host $_.Line }
} else {
    Write-Host "没有日志 $Log"
}
