# QA helper for f195: DComp video-layer geometry.
# Developer tool, not product code.
#
#   powershell -File scripts/qa-f195-geom.ps1 -Stage sizes
#   powershell -File scripts/qa-f195-geom.ps1 -Stage panel
#   powershell -File scripts/qa-f195-geom.ps1 -Stage fullscreen
#
# One app run per stage (the panel and fullscreen hooks are one-shot). Every
# capture is the real screen region of the player window (CopyFromScreen, i.e.
# what the compositor scanned out), plus its outer 2 px ring summarised: a video
# layer that overflowed would put picture pixels there and widen the luma range.
param(
    [ValidateSet("sizes", "panel", "fullscreen")]
    [string]$Stage = "sizes",
    [string]$Exe = "target/release/O11A1.exe",
    [string]$Bvid = "BV1BBnb6aEww",
    [string]$OutDir = "target/qa",
    [int]$BootMs = 12000
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

if (-not ("F195Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public class F195Win {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);

    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }

    public static List<IntPtr> Windows(uint pid) {
        var list = new List<IntPtr>();
        EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == pid && IsWindowVisible(h)) list.Add(h); return true; }, IntPtr.Zero);
        return list;
    }
    public static string Class(IntPtr h) { var sb = new StringBuilder(512); GetClassName(h, sb, 512); return sb.ToString(); }
}
"@
}

$dir = Join-Path (Get-Location) $OutDir
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
$Log = Join-Path $dir ("f195_" + $Stage + ".log")
if (Test-Path $Log) { Remove-Item $Log -Force }

$env:BILI_DEBUG_LOG = $Log
$env:BILI_OPEN_BVID = $Bvid
$env:BILI_PLAYER_DCOMP = "1"
$env:BILI_INTERP_TEST = "1"
switch ($Stage) {
    "panel" { $env:BILI_PANEL_TEST = "0,1,0,30000" }
    "fullscreen" { $env:BILI_FULLSCREEN_TEST = "7000" }
    default { Remove-Item Env:BILI_PANEL_TEST -ErrorAction SilentlyContinue; Remove-Item Env:BILI_FULLSCREEN_TEST -ErrorAction SilentlyContinue }
}

function Get-PlayerWindow($proc, $timeoutMs) {
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    while ((Get-Date) -lt $deadline) {
        foreach ($h in [F195Win]::Windows([uint32]$proc.Id)) {
            $c = [F195Win]::Class($h)
            if ($c -eq "ConsoleWindowClass") { continue }
            if ($c -eq "O11A1DcompPlayer") { return $h }
        }
        Start-Sleep -Milliseconds 400
    }
    throw "no O11A1DcompPlayer window within the deadline"
}

function Set-ClientSize($hwnd, $w, $h) {
    # The player is frameless with WM_NCCALCSIZE keeping the client whole, so
    # the outer size is the client size; one corrective pass covers a frame.
    for ($i = 0; $i -lt 3; $i++) {
        [void][F195Win]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, $w, $h, 0x0004 -bor 0x0010)
        Start-Sleep -Milliseconds 120
        $c = New-Object F195Win+RECT
        [void][F195Win]::GetClientRect($hwnd, [ref]$c)
        $dw = $w - ($c.Right - $c.Left); $dh = $h - ($c.Bottom - $c.Top)
        if ($dw -eq 0 -and $dh -eq 0) { break }
        $r = New-Object F195Win+RECT
        [void][F195Win]::GetWindowRect($hwnd, [ref]$r)
        [void][F195Win]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, ($r.Right - $r.Left) + $dw, ($r.Bottom - $r.Top) + $dh, 0x0004 -bor 0x0010)
    }
    Start-Sleep -Milliseconds 900
}

function Capture($hwnd, $name) {
    # Bring the player above whatever is on screen: the capture is the real
    # scan-out, so it must not photograph the app behind it.
    [void][F195Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0010)
    [void][F195Win]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 600
    $r = New-Object F195Win+RECT
    [void][F195Win]::GetWindowRect($hwnd, [ref]$r)
    $c = New-Object F195Win+RECT
    [void][F195Win]::GetClientRect($hwnd, [ref]$c)
    $w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($r.Left, $r.Top, 0, 0, (New-Object System.Drawing.Size($w, $h)))
    $g.Dispose()
    $path = Join-Path $dir $name
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)

    # Outer 2 px ring: colours + luma range. Uniform == no picture reached the
    # window edge (the backdrop is a flat tint/letterbox).
    $colors = @{}
    $lumaMin = 255.0; $lumaMax = 0.0
    $ym1 = $h - 1; $ym2 = $h - 2; $xm1 = $w - 1; $xm2 = $w - 2
    for ($x = 0; $x -lt $w; $x++) {
        foreach ($y in @(0, 1, $ym2, $ym1)) {
            $p = $bmp.GetPixel($x, $y)
            $key = "$($p.R),$($p.G),$($p.B)"
            $colors[$key] = 1
            $l = 0.2126 * $p.R + 0.7152 * $p.G + 0.0722 * $p.B
            if ($l -lt $lumaMin) { $lumaMin = $l }
            if ($l -gt $lumaMax) { $lumaMax = $l }
        }
    }
    for ($y = 0; $y -lt $h; $y++) {
        foreach ($x in @(0, 1, $xm2, $xm1)) {
            $p = $bmp.GetPixel($x, $y)
            $key = "$($p.R),$($p.G),$($p.B)"
            $colors[$key] = 1
            $l = 0.2126 * $p.R + 0.7152 * $p.G + 0.0722 * $p.B
            if ($l -lt $lumaMin) { $lumaMin = $l }
            if ($l -gt $lumaMax) { $lumaMax = $l }
        }
    }
    $bmp.Dispose()
    Write-Host ("capture {0} window={1}x{2} client={3}x{4} edge_colors={5} edge_luma={6:N1}..{7:N1}" -f `
        $name, $w, $h, ($c.Right - $c.Left), ($c.Bottom - $c.Top), $colors.Count, $lumaMin, $lumaMax)
}

$proc = Start-Process -FilePath $Exe -PassThru
try {
    $hwnd = Get-PlayerWindow $proc ($BootMs + 30000)
    # The fullscreen hook fires 7 s after the session starts (and turns off 4 s
    # later), so that stage must start polling immediately instead of after the
    # usual boot wait.
    if ($Stage -eq "fullscreen") { Start-Sleep -Milliseconds 1500 } else { Start-Sleep -Milliseconds $BootMs }
    [void][F195Win]::SetForegroundWindow($hwnd)
    $rr = New-Object F195Win+RECT
    [void][F195Win]::GetWindowRect($hwnd, [ref]$rr)
    [void][F195Win]::SetCursorPos(($rr.Left + $rr.Right) / 2, ($rr.Top + $rr.Bottom) / 2)

    switch ($Stage) {
        "sizes" {
            foreach ($size in @(@(960, 540), @(1280, 800), @(1920, 1080))) {
                Set-ClientSize $hwnd $size[0] $size[1]
                Capture $hwnd ("f195_geom_" + $size[0] + ".png")
            }
        }
        "panel" {
            # Wait for the panel hook, then shrink the window back so the band
            # really is narrower than the window (the reserved mode grows it).
            $deadline = (Get-Date).AddMilliseconds(30000)
            $opened = $false
            while ((Get-Date) -lt $deadline) {
                if ((Test-Path $Log) -and (Select-String -Path $Log -SimpleMatch "paneltest set step=0" -Quiet)) { $opened = $true; break }
                Start-Sleep -Milliseconds 400
            }
            if (-not $opened) { throw "panel hook never fired" }
            Start-Sleep -Milliseconds 1500
            Set-ClientSize $hwnd 1280 800
            Capture $hwnd "f195_geom_panel.png"
        }
        "fullscreen" {
            # The hook toggles on at 7 s and off 4 s later; wait for the window
            # to cover the monitor, then grab.
            $deadline = (Get-Date).AddMilliseconds(30000)
            $shot = $false
            while ((Get-Date) -lt $deadline) {
                $r = New-Object F195Win+RECT
                [void][F195Win]::GetWindowRect($hwnd, [ref]$r)
                if (($r.Right - $r.Left) -ge 1900 -and ($r.Bottom - $r.Top) -ge 1000) {
                    Start-Sleep -Milliseconds 600
                    Capture $hwnd "f195_geom_fullscreen.png"
                    $shot = $true
                    break
                }
                Start-Sleep -Milliseconds 250
            }
            if (-not $shot) { throw "fullscreen never covered the monitor" }
        }
    }
}
finally {
    if (-not $proc.HasExited) { Start-Sleep -Milliseconds 300; Stop-Process -Id $proc.Id -Force }
    Write-Host "--- geom lines ---"
    if (Test-Path $Log) { Select-String -Path $Log -SimpleMatch "dcomp video geom" | ForEach-Object { $_.Line } }
    Write-Host "--- log tail ---"
    if (Test-Path $Log) { Get-Content $Log -Tail 12 }
}
