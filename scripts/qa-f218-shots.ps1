# QA helper for f218: the two player-window screenshots.
#
#   powershell -File scripts/qa-f218-shots.ps1
#   powershell -File scripts/qa-f218-shots.ps1 -Stage 720 -Interp 3
#
# Produces, from the *shipped* binary and a real window:
#
#   docs/f218_player_corners.png  the four corners of the player window at 4x,
#                                 captured with a margin so the rounded cut is
#                                 visible against the desktop behind it;
#   docs/f218_player_chrome.png   the window with the chrome up and the right
#                                 panel open (the largest chrome surface), so
#                                 the wallpaper sample can be read on the pill,
#                                 the panel and its popovers.
#
# Both are SCREEN reads of the client rect of a raised player window, with the
# window placed fully on the primary monitor (`SetWindowPos` on the client
# origin, not the window rect: a frameless window still has invisible resize
# borders, and a client that runs off the screen reads black).
param(
    [ValidateSet("720", "1080")]
    [string]$Stage = "720",
    [ValidateSet("0", "3")]
    [string]$Interp = "0",
    [string]$Exe = "target/release/o11a1.exe",
    [int]$ClientW = 1000,
    [int]$ClientH = 600,
    [int]$Margin = 36,
    [string]$SamplePath = "target/f209/sample720p30.mp4",
    [string]$Bvid = "BV1BBnb6aEww",
    [string]$ShotDir = "docs",
    [string]$OutDir = "target/qa/f218",
    # Keep the window open after the captures (for a manual look).
    [switch]$KeepOpen
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

if (-not ("F218Shot" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public class F218Shot {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int cmd);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr hWnd, ref POINT point);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, IntPtr pid);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint attach, uint to, bool on);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();

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
$ShotDir = if ([System.IO.Path]::IsPathRooted($ShotDir)) { $ShotDir } else { Join-Path $root $ShotDir }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
New-Item -ItemType Directory -Force -Path $ShotDir | Out-Null

$exePath = if ([System.IO.Path]::IsPathRooted($Exe)) { $Exe } else { Join-Path $root $Exe }
if (-not (Test-Path $exePath)) { throw ("missing exe: " + $exePath) }
$sample = if ([System.IO.Path]::IsPathRooted($SamplePath)) { $SamplePath } else { Join-Path $root $SamplePath }
if (-not (Test-Path $sample)) { throw ("missing sample: " + $sample) }

$tag = "{0}p_{1}x{2}" -f $Stage, $ClientW, $ClientH
$Log = Join-Path $OutDir ("f218_shots_{0}_{1}.log" -f $tag, (Get-Date -Format "HHmmss"))

Get-Process o11a1 -ErrorAction SilentlyContinue | Stop-Process -Force
for ($wait = 0; $wait -lt 40; $wait++) {
    if (-not (Get-Process o11a1 -ErrorAction SilentlyContinue)) { break }
    Start-Sleep -Milliseconds 100
}
Start-Sleep -Milliseconds 300

$env:BILI_DEBUG_LOG = $Log
$env:BILI_OPEN_BVID = $Bvid
$env:BILI_QUALITY_PICK = "0,6000"
$env:BILI_PLAYER_DCOMP = "1"
$env:BILI_F213_TRACE = "1"
$env:BILI_QA_MF_FILE = $sample
$env:BILI_INTERP_TEST = $Interp
if ($Interp -eq "3") { $env:BILI_INTERP_4X = "1" } else { Remove-Item Env:BILI_INTERP_4X -ErrorAction SilentlyContinue }
# Open the 信息 panel (right strip) for the chrome shot: it is the largest chrome
# surface and the one a reviewer can judge the sample on.
$env:BILI_PANEL_TEST = "0,1,0,600000"
foreach ($name in @("BILI_DECODE", "BILI_FULLSCREEN_TEST", "BILI_DCOMP_UI_FORCE", "BILI_WARP_TRACE", "BILI_PACE_TRACE", "BILI_SOAK_TEST", "BILI_F214_DPI", "BILI_DANMAKU_STRIPS")) {
    Remove-Item ("Env:" + $name) -ErrorAction SilentlyContinue
}

Write-Host ("f218 exe {0} client={1}x{2} sample={3} log={4}" -f $exePath, $ClientW, $ClientH, $sample, $Log)
$proc = Start-Process -FilePath $exePath -WorkingDirectory $root -PassThru

function Find-Player($proc, $timeoutMs) {
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    while ((Get-Date) -lt $deadline) {
        foreach ($h in [F218Shot]::Windows([uint32]$proc.Id)) {
            if ([F218Shot]::Class($h) -eq "O11A1DcompPlayer") { return $h }
        }
        Start-Sleep -Milliseconds 60
    }
    return [IntPtr]::Zero
}

function Shot-Screen([int]$x, [int]$y, [int]$w, [int]$h) {
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($x, $y, 0, 0, $bmp.Size)
    $g.Dispose()
    return $bmp
}

try {
    $hwnd = Find-Player $proc 70000
    if ($hwnd -eq [IntPtr]::Zero) { throw "no player window within 70 s" }
    Write-Host ("f218 player hwnd={0}" -f $hwnd)
    [void][F218Shot]::ShowWindow($hwnd, 9)

    # Size + place the CLIENT on the primary monitor, with a margin all round so
    # the rounded corners have desktop behind them.
    $placed = $false
    for ($pass = 0; $pass -lt 12; $pass++) {
        $outer = New-Object F218Shot+RECT
        [void][F218Shot]::GetWindowRect($hwnd, [ref]$outer)
        $dw = ($outer.Right - $outer.Left) - [F218Shot]::ClientWidth($hwnd)
        $dh = ($outer.Bottom - $outer.Top) - [F218Shot]::ClientHeight($hwnd)
        [void][F218Shot]::SetWindowPos($hwnd, [IntPtr](-1), $Margin, $Margin, ($ClientW + $dw), ($ClientH + $dh), 0x0040)
        Start-Sleep -Milliseconds 250
        $probe = New-Object F218Shot+POINT
        [void][F218Shot]::ClientToScreen($hwnd, [ref]$probe)
        # Shift by the client offset so the client (not the window rect) sits at
        # the margin; a frameless window keeps invisible resize borders.
        [void][F218Shot]::SetWindowPos($hwnd, [IntPtr](-1), $Margin - ($probe.X - $Margin), $Margin - ($probe.Y - $Margin), 0, 0, 0x0001 -bor 0x0010)
        Start-Sleep -Milliseconds 150
        [void][F218Shot]::ClientToScreen($hwnd, [ref]$probe)
        if ([F218Shot]::ClientWidth($hwnd) -eq $ClientW -and [F218Shot]::ClientHeight($hwnd) -eq $ClientH -and
            $probe.X -eq $Margin -and $probe.Y -eq $Margin) {
            $placed = $true
            break
        }
    }
    if (-not $placed) { Write-Host "f218 note: the client did not settle exactly on the margin" }

    # Put the MAIN window behind the player, filling the screen: the corner cut
    # of the player is only readable if what shows through it contrasts with the
    # picture. The main window is the process's other large visible window.
    $back = [IntPtr]::Zero
    $backArea = 0
    foreach ($h in [F218Shot]::Windows([uint32]$proc.Id)) {
        if ($h -eq $hwnd) { continue }
        if ([F218Shot]::Class($h) -eq "O11A1DcompPlayer") { continue }
        $w = [F218Shot]::ClientWidth($h)
        $hgt = [F218Shot]::ClientHeight($h)
        if ($w * $hgt -gt $backArea) {
            $back = $h
            $backArea = $w * $hgt
        }
    }
    if ($back -ne [IntPtr]::Zero) {
        [void][F218Shot]::ShowWindow($back, 4)  # SW_SHOWNOACTIVATE
        # Below the (topmost) player, but above the desktop.
        [void][F218Shot]::SetWindowPos($back, [IntPtr](-2), 0, 0, 1920, 1080, 0x0010 -bor 0x0040)
        Start-Sleep -Milliseconds 400
        Write-Host "f218 main window placed behind the player"
    }

    # Foreground: attach to the current foreground thread so a background app can
    # still raise its own topmost window (the 闪烁 probe learned this the hard
    # way - without it the capture reads whatever window is on top).
    $fg = [F218Shot]::GetForegroundWindow()
    if ($fg -ne [IntPtr]::Zero -and $fg -ne $hwnd) {
        $fgThread = [F218Shot]::GetWindowThreadProcessId($fg, [IntPtr]::Zero)
        $mine = [F218Shot]::GetCurrentThreadId()
        if ($fgThread -ne 0 -and $fgThread -ne $mine) {
            [void][F218Shot]::AttachThreadInput($fgThread, $mine, $true)
            [void][F218Shot]::BringWindowToTop($hwnd)
            [void][F218Shot]::SetForegroundWindow($hwnd)
            Start-Sleep -Milliseconds 150
            [void][F218Shot]::AttachThreadInput($fgThread, $mine, $false)
        }
    }
    for ($wait = 0; $wait -lt 40; $wait++) {
        [void][F218Shot]::SetForegroundWindow($hwnd)
        if ([F218Shot]::GetForegroundWindow() -eq $hwnd) { break }
        Start-Sleep -Milliseconds 50
    }

    # Let playback settle, then keep the pointer inside so `chrome_shown` is up
    # (the app samples the REAL cursor position, so moving it here is enough).
    Write-Host "f218 settling ..."
    Start-Sleep -Seconds 12
    $origin = New-Object F218Shot+POINT
    [void][F218Shot]::ClientToScreen($hwnd, [ref]$origin)
    [void][F218Shot]::SetCursorPos($origin.X + [int]($ClientW / 2), $origin.Y + [int]($ClientH * 0.72))
    Start-Sleep -Milliseconds 1200

    $origin = New-Object F218Shot+POINT
    [void][F218Shot]::ClientToScreen($hwnd, [ref]$origin)
    $cw = [F218Shot]::ClientWidth($hwnd)
    $ch = [F218Shot]::ClientHeight($hwnd)
    Write-Host ("f218 capture client {0}x{1} at {2},{3}" -f $cw, $ch, $origin.X, $origin.Y)

    # Chrome shot: the client plus a thin margin, so the window reads as a window.
    $chrome = Shot-Screen ($origin.X - 8) ($origin.Y - 8) ($cw + 16) ($ch + 16)
    $chromePath = Join-Path $ShotDir "f218_player_chrome.png"
    $chrome.Save($chromePath, [System.Drawing.Imaging.ImageFormat]::Png)
    $chrome.Dispose()
    Write-Host ("f218 chrome -> {0}" -f $chromePath)

    # Corners: capture with the margin, then crop each corner and scale it 4x.
    $big = Shot-Screen ($origin.X - $Margin) ($origin.Y - $Margin) ($cw + 2 * $Margin) ($ch + 2 * $Margin)
    $crop = 90
    $zoom = 4
    $cell = $crop * $zoom
    $gap = 12
    $out = New-Object System.Drawing.Bitmap (2 * $cell + 3 * $gap), (2 * $cell + 3 * $gap)
    $g = [System.Drawing.Graphics]::FromImage($out)
    $g.Clear([System.Drawing.Color]::FromArgb(255, 24, 24, 28))
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::NearestNeighbor
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::Half
    # (source x, source y, cell x, cell y): the margin is what puts desktop
    # pixels around the corner, which is what makes the rounding visible.
    # Center each crop on the corner: half desktop, half window.
    $off = [Math]::Max(0, $Margin - 45)
    $w2 = $cw + 2 * $Margin
    $h2 = $ch + 2 * $Margin
    $spots = @(
        @{ sx = $off; sy = $off; cx = 1; cy = 1 },
        @{ sx = $w2 - $off - $crop; sy = $off; cx = 2; cy = 1 },
        @{ sx = $off; sy = $h2 - $off - $crop; cx = 1; cy = 2 },
        @{ sx = $w2 - $off - $crop; sy = $h2 - $off - $crop; cx = 2; cy = 2 }
    )
    foreach ($s in $spots) {
        $src = New-Object System.Drawing.Rectangle $s.sx, $s.sy, $crop, $crop
        $dst = New-Object System.Drawing.Rectangle ($gap + ($s.cx - 1) * ($cell + $gap)), ($gap + ($s.cy - 1) * ($cell + $gap)), $cell, $cell
        $g.DrawImage($big, $dst, $src, [System.Drawing.GraphicsUnit]::Pixel)
    }
    $g.Dispose()
    $big.Dispose()
    $cornerPath = Join-Path $ShotDir "f218_player_corners.png"
    $out.Save($cornerPath, [System.Drawing.Imaging.ImageFormat]::Png)
    $out.Dispose()
    Write-Host ("f218 corners -> {0}" -f $cornerPath)

    Write-Host ""
    Write-Host "---- f218 evidence ----"
    if (Test-Path $Log) {
        Select-String -Path $Log -Pattern "dcomp player corners" | Select-Object -Last 4 | ForEach-Object { Write-Host $_.Line }
        Select-String -Path $Log -Pattern "theme win=player" | Select-Object -Last 1 | ForEach-Object { Write-Host $_.Line }
        Select-String -Path $Log -Pattern "dcomp video geom" | Select-Object -Last 1 | ForEach-Object { Write-Host $_.Line }
        Select-String -Path $Log -Pattern "f213 slot" | Select-Object -Last 1 | ForEach-Object { Write-Host $_.Line }
    }
} finally {
    if (-not $KeepOpen -and $proc -and -not $proc.HasExited) {
        Start-Sleep -Milliseconds 400
        $proc.Kill()
        $proc.WaitForExit(5000) | Out-Null
    }
}
