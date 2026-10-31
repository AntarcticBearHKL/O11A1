# QA helper for f200: the three layers' real rates on the DComp player, the
# optical-flow GPU warp, and the five-scene regression.
#
#   powershell -File scripts/qa-f200.ps1 -Stage 720      # 1280x720, 3 layers
#   powershell -File scripts/qa-f200.ps1 -Stage 1080     # 1920x1080, 3 layers
#   powershell -File scripts/qa-f200.ps1 -Stage winit    # the default path, control
#
# Every rate in the log is the f200 sampler's per-second line
# (`f200 layers t=… video_s=… dm_s=… ui_s=…`), i.e. presents counted by each
# layer's own thread. The capture is the real scan-out (CopyFromScreen), because
# the composite path has no Slint tree to snapshot.
param(
    [ValidateSet("720", "1080", "winit", "plain")]
    [string]$Stage = "720",
    [string]$Exe = "target/release/o11a1.exe",
    [string]$Bvid = "BV1BBnb6aEww",
    [string]$OutDir = "target/qa",
    [int]$BootMs = 14000,
    [int]$Soak = 45
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

if (-not ("F200Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public class F200Win {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int max);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr hWnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);

    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }

    public static List<IntPtr> Windows(uint pid) {
        var list = new List<IntPtr>();
        EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == pid && IsWindowVisible(h)) list.Add(h); return true; }, IntPtr.Zero);
        return list;
    }
    public static string Class(IntPtr h) { var sb = new StringBuilder(512); GetClassName(h, sb, 512); return sb.ToString(); }
    public static string Title(IntPtr h) { var sb = new StringBuilder(512); GetWindowTextW(h, sb, 512); return sb.ToString(); }
}
"@
}

$dir = Join-Path (Get-Location) $OutDir
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
$Log = Join-Path $dir ("f200_" + $Stage + ".log")
if (Test-Path $Log) { Remove-Item $Log -Force }

$env:BILI_DEBUG_LOG = $Log
$env:BILI_OPEN_BVID = $Bvid
$env:BILI_SOAK_TEST = "$Soak"
$env:BILI_F200_LAYERS = "$Soak"
# Ask for the highest tier the stream offers, so the 1080p stage really runs a
# 1080p source (the frame size is logged as `ofa session frame=…` / `dcomp video
# layer frame=…`). The pick goes through the real 画质 menu.
if (-not $env:BILI_QUALITY_PICK) { $env:BILI_QUALITY_PICK = "0,6000" }
if ($env:BILI_F200_UI_FORCE -eq "1") { $env:BILI_DCOMP_UI_FORCE = "1" } else { Remove-Item Env:BILI_DCOMP_UI_FORCE -ErrorAction SilentlyContinue }
Remove-Item Env:BILI_PANEL_TEST -ErrorAction SilentlyContinue
Remove-Item Env:BILI_FULLSCREEN_TEST -ErrorAction SilentlyContinue

switch ($Stage) {
    "winit" {
        Remove-Item Env:BILI_PLAYER_DCOMP -ErrorAction SilentlyContinue
        Remove-Item Env:BILI_DCOMP_DANMAKU -ErrorAction SilentlyContinue
        $env:BILI_INTERP_TEST = "1"
        $env:BILI_INTERP_4X = "1"
    }
    "plain" {
        $env:BILI_PLAYER_DCOMP = "1"
        $env:BILI_DCOMP_DANMAKU = "1"
        Remove-Item Env:BILI_INTERP_TEST -ErrorAction SilentlyContinue
        Remove-Item Env:BILI_INTERP_4X -ErrorAction SilentlyContinue
    }
    default {
        $env:BILI_PLAYER_DCOMP = "1"
        $env:BILI_DCOMP_DANMAKU = "1"
        # Mode 3 (hardware optical flow) at 4x: the one path f200 changed.
        $env:BILI_INTERP_TEST = "3"
        $env:BILI_INTERP_4X = "1"
    }
}

$Script:Log = $Log

function Get-PlayerWindow($proc, $timeoutMs) {
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    $logDeadline = (Get-Date).AddMilliseconds(8000)
    while ((Get-Date) -lt $logDeadline) {
        if (Test-Path $Script:Log) {
            $hit = Select-String -Path $Script:Log -SimpleMatch "player session open" | Where-Object { $_.Line -notmatch "hwnd=0 " } | Select-Object -Last 1
            if (-not $hit) { $hit = Select-String -Path $Script:Log -SimpleMatch "player hwnd=" | Select-Object -Last 1 }
            if ($hit) {
                $m = [regex]::Match($hit.Line, "hwnd=(\d+)")
                if ($m.Success) {
                    $candidate = [IntPtr][int64]$m.Groups[1].Value
                    $cr = New-Object F200Win+RECT
                    if ([F200Win]::GetClientRect($candidate, [ref]$cr)) { return $candidate }
                }
            }
        }
        Start-Sleep -Milliseconds 300
    }
    while ((Get-Date) -lt $deadline) {
        foreach ($h in [F200Win]::Windows([uint32]$proc.Id)) {
            $c = [F200Win]::Class($h)
            if ($c -eq "ConsoleWindowClass") { continue }
            if ($c -eq "O11A1DcompPlayer") { return $h }
            if ($c -like "*Player*") { return $h }
        }
        $best = $null; $bestArea = 0
        foreach ($h in [F200Win]::Windows([uint32]$proc.Id)) {
            $c = [F200Win]::Class($h)
            if ($c -eq "ConsoleWindowClass" -or $c -eq "Shell_TrayWnd") { continue }
            $title = [F200Win]::Title($h)
            if ($title.Length -le 8 -and $title -match '^[ -~]*$') { continue }
            $cr = New-Object F200Win+RECT
            [void][F200Win]::GetClientRect($h, [ref]$cr)
            $w = $cr.Right - $cr.Left; $h2 = $cr.Bottom - $cr.Top
            if ($w -lt 400 -or $h2 -lt 300) { continue }
            if (($w * $h2) -gt $bestArea) { $best = $h; $bestArea = $w * $h2 }
        }
        if ($best -ne $null) { return $best }
        Start-Sleep -Milliseconds 400
    }
    throw "no player window within the deadline"
}

function Set-ClientSize($hwnd, $w, $h) {
    for ($pass = 0; $pass -lt 8; $pass++) {
        for ($i = 0; $i -lt 3; $i++) {
            [void][F200Win]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, $w, $h, 0x0004 -bor 0x0010)
            Start-Sleep -Milliseconds 120
            $c = New-Object F200Win+RECT
            [void][F200Win]::GetClientRect($hwnd, [ref]$c)
            $dw = $w - ($c.Right - $c.Left); $dh = $h - ($c.Bottom - $c.Top)
            if ($dw -eq 0 -and $dh -eq 0) { break }
            $r = New-Object F200Win+RECT
            [void][F200Win]::GetWindowRect($hwnd, [ref]$r)
            [void][F200Win]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, ($r.Right - $r.Left) + $dw, ($r.Bottom - $r.Top) + $dh, 0x0004 -bor 0x0010)
        }
        $c = New-Object F200Win+RECT
        [void][F200Win]::GetClientRect($hwnd, [ref]$c)
        if (($c.Right - $c.Left) -eq $w -and ($c.Bottom - $c.Top) -eq $h) { break }
        Start-Sleep -Milliseconds 500
    }
    Start-Sleep -Milliseconds 1200
}

function Capture($hwnd, $name) {
    [void][F200Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0010)
    [void][F200Win]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 700
    $r = New-Object F200Win+RECT
    [void][F200Win]::GetWindowRect($hwnd, [ref]$r)
    $c = New-Object F200Win+RECT
    [void][F200Win]::GetClientRect($hwnd, [ref]$c)
    $w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
    Write-Host ("capture-target hwnd={0} rect={1},{2} {3}x{4} class={5}" -f $hwnd, $r.Left, $r.Top, $w, $h, [F200Win]::Class($hwnd))
    if ($w -le 0 -or $h -le 0) { Write-Host "capture skipped: empty rect"; return }
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($r.Left, $r.Top, 0, 0, (New-Object System.Drawing.Size($w, $h)))
    $g.Dispose()
    $path = Join-Path $dir $name
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host ("capture {0} window={1}x{2} client={3}x{4}" -f $name, $w, $h, ($c.Right - $c.Left), ($c.Bottom - $c.Top))
}

$proc = Start-Process -FilePath $Exe -PassThru
try {
    $hwnd = Get-PlayerWindow $proc ($BootMs + 30000)
    Start-Sleep -Milliseconds $BootMs
    [void][F200Win]::SetForegroundWindow($hwnd)
    $rr = New-Object F200Win+RECT
    [void][F200Win]::GetWindowRect($hwnd, [ref]$rr)
    [void][F200Win]::SetCursorPos([int](($rr.Left + $rr.Right) / 2), [int](($rr.Top + $rr.Bottom) / 2))

    switch ($Stage) {
        "720" {
            Set-ClientSize $hwnd 1280 720
            Start-Sleep -Milliseconds 12000
            Capture $hwnd "f200_720p_layers.png"
            Start-Sleep -Milliseconds 2000
            Capture $hwnd "f200_720p_layers_b.png"
        }
        "1080" {
            Set-ClientSize $hwnd 1920 1080
            Start-Sleep -Milliseconds 12000
            Capture $hwnd "f200_1080p_layers.png"
            Start-Sleep -Milliseconds 2000
            Capture $hwnd "f200_1080p_layers_b.png"
        }
        "winit" {
            Set-ClientSize $hwnd 1280 720
            Start-Sleep -Milliseconds 12000
            Capture $hwnd "f200_720p_winit.png"
        }
        "plain" {
            Set-ClientSize $hwnd 1280 720
            Start-Sleep -Milliseconds 12000
            Capture $hwnd "f200_720p_plain.png"
        }
    }
} finally {
    Start-Sleep -Milliseconds 2000
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    Write-Host "log: $Log"
    if (Test-Path $Log) {
        Write-Host "--- f200 layer lines ---"
        Select-String -Path $Log -SimpleMatch "f200 layers" | ForEach-Object { $_.Line }
        Write-Host "--- warp lines ---"
        Select-String -Path $Log -SimpleMatch "dcomp warp" | ForEach-Object { $_.Line } | Select-Object -First 20
        Write-Host "--- ofa lines ---"
        Select-String -Path $Log -SimpleMatch "ofa " | ForEach-Object { $_.Line } | Select-Object -First 20
        Write-Host "--- geom ---"
        Select-String -Path $Log -SimpleMatch "dcomp video geom" | Select-Object -Last 2 | ForEach-Object { $_.Line }
    }
}
