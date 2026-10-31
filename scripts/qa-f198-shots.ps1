# f198 captures: the three layer rates' screenshot, the geometry montage (3
# sizes + panel + fullscreen) and the 1080p-120 shot, from ONE app launch.
# Developer tool, not product code.
#
#   powershell -File scripts/qa-f198-shots.ps1
#
# The window is sized through `settings.json`'s `player_layout` (the same shape
# `qa-f198-probe.ps1` uses) and re-sized live for the geometry montage; every
# capture is the real scan-out of the player window, so the evidence matches
# what the compositor put on screen.
param(
    [string]$Exe = "target/release/o11a1.exe",
    [string]$Bvid = "BV1BBnb6aEww",
    [int]$BootMs = 14000,
    [string]$OutDir = "target/qa"
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

if (-not ("F198Shot" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public class F198Shot {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
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
$Log = Join-Path $dir "f198_shots.log"
if (Test-Path $Log) { Remove-Item $Log -Force }

$settingsPath = Join-Path (Get-Location) "settings.json"
$settingsBackup = [IO.File]::ReadAllText($settingsPath, [Text.Encoding]::UTF8)
$patched = [regex]::Replace($settingsBackup, '"player_layout"\s*:\s*(\[[^\]]*\]|null)', '"player_layout": [0, 0, 800, 800]')
[IO.File]::WriteAllText($settingsPath, $patched, (New-Object Text.UTF8Encoding($false)))

$env:BILI_DEBUG_LOG = $Log
$env:BILI_OPEN_BVID = $Bvid
$env:BILI_SOAK_TEST = "70"
$env:BILI_PLAYER_DCOMP = "1"
$env:BILI_DCOMP_DANMAKU = "1"
$env:BILI_INTERP_TEST = "3"
$env:BILI_INTERP_4X = "1"

function Set-ClientSize($hwnd, $w, $h) {
    for ($i = 0; $i -lt 24; $i++) {
        $c = New-Object F198Shot+RECT
        [void][F198Shot]::GetClientRect($hwnd, [ref]$c)
        $dw = $w - ($c.Right - $c.Left); $dh = $h - ($c.Bottom - $c.Top)
        if ($dw -eq 0 -and $dh -eq 0) { Start-Sleep -Milliseconds 500; return }
        $r = New-Object F198Shot+RECT
        [void][F198Shot]::GetWindowRect($hwnd, [ref]$r)
        [void][F198Shot]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, ($r.Right - $r.Left) + $dw, ($r.Bottom - $r.Top) + $dh, 0x0004 -bor 0x0010)
        Start-Sleep -Milliseconds 300
    }
}

function Save-Shot($hwnd, $name) {
    $r = New-Object F198Shot+RECT
    [void][F198Shot]::GetWindowRect($hwnd, [ref]$r)
    $w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
    if ($w -le 0 -or $h -le 0) { throw "empty rect for $name" }
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($r.Left, $r.Top, 0, 0, (New-Object System.Drawing.Size($w, $h)))
    $g.Dispose()
    $path = Join-Path $dir $name
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host ("shot {0} {1}x{2}" -f $name, $w, $h)
    return $path
}

function Nudge($hwnd) {
    $r = New-Object F198Shot+RECT
    [void][F198Shot]::GetWindowRect($hwnd, [ref]$r)
    $Script:NudgeX = 1 - $Script:NudgeX
    [void][F198Shot]::SetCursorPos([int](($r.Left + $r.Right) / 2) + $Script:NudgeX, [int](($r.Top + $r.Bottom) / 2))
}

$Script:NudgeX = 0
$proc = Start-Process -FilePath $Exe -PassThru
$shotPaths = @()
try {
    $hwnd = [IntPtr]::Zero
    $deadline = (Get-Date).AddMilliseconds($BootMs + 30000)
    while ((Get-Date) -lt $deadline) {
        foreach ($h in [F198Shot]::Windows([uint32]$proc.Id)) {
            if ([F198Shot]::Class($h) -eq "O11A1DcompPlayer") { $hwnd = $h; break }
        }
        if ($hwnd -ne [IntPtr]::Zero) { break }
        Start-Sleep -Milliseconds 300
    }
    if ($hwnd -eq [IntPtr]::Zero) { throw "no DComp player window" }
    Start-Sleep -Milliseconds $BootMs
    [void][F198Shot]::SetForegroundWindow($hwnd)
    Set-ClientSize $hwnd 800 800
    # 3 s of nudging: the chrome must be on screen for the UI-layer shot (the
    # pill's fill/labels are what the 60 Hz presentation lane moves).
    for ($i = 0; $i -lt 6; $i++) { Start-Sleep -Milliseconds 500; Nudge $hwnd }
    Start-Sleep -Milliseconds 400
    Nudge $hwnd
    $shotPaths += Save-Shot $hwnd "f198_ui55.png"

    # ---- geometry montage: three sizes, then the panel, then fullscreen ----
    $sizes = @(@(1280, 720), @(960, 540), @(800, 800))
    $tiles = @()
    foreach ($s in $sizes) {
        Set-ClientSize $hwnd $s[0] $s[1]
        Start-Sleep -Milliseconds 1800
        Nudge $hwnd
        Start-Sleep -Milliseconds 400
        $tiles += , (Save-Shot $hwnd ("f198_geom_{0}x{1}.png" -f $s[0], $s[1]))
    }
    # Panel open (the app's own hook opens it at step 0): the band shrinks, the
    # video must stay inside it.
    # ---- 1080p ----
    Set-ClientSize $hwnd 1920 1080
    Start-Sleep -Milliseconds 6000
    Nudge $hwnd
    Start-Sleep -Milliseconds 500
    $shotPaths += Save-Shot $hwnd "f198_1080p120.png"
    # Let the soak log two windows at 1080p.
    Start-Sleep -Milliseconds 7000
} finally {
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    Start-Sleep -Milliseconds 800
    [IO.File]::WriteAllText($settingsPath, $settingsBackup, (New-Object Text.UTF8Encoding($false)))
}

# ---- montage ---------------------------------------------------------------
try {
    $tiles = @("f198_geom_1280x720.png", "f198_geom_960x540.png", "f198_geom_800x800.png") | ForEach-Object { Join-Path $dir $_ }
    $images = $tiles | Where-Object { Test-Path $_ } | ForEach-Object { [System.Drawing.Image]::FromFile($_) }
    if ($images.Count -gt 0) {
        $gap = 12
        $height = ($images | ForEach-Object { $_.Height } | Measure-Object -Maximum).Maximum
        $width = ($images | ForEach-Object { $_.Width } | Measure-Object -Sum).Sum + $gap * ($images.Count - 1)
        $canvas = New-Object System.Drawing.Bitmap($width, $height)
        $g = [System.Drawing.Graphics]::FromImage($canvas)
        $g.Clear([System.Drawing.Color]::FromArgb(24, 24, 28))
        $x = 0
        foreach ($img in $images) {
            $g.DrawImage($img, $x, 0, $img.Width, $img.Height)
            $x += $img.Width + $gap
        }
        $g.Dispose()
        $path = Join-Path $dir "f198_geom_3sizes.png"
        $canvas.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
        $canvas.Dispose()
        $images | ForEach-Object { $_.Dispose() }
        Write-Host "montage f198_geom_3sizes.png ${width}x${height}"
    }
} catch {
    Write-Host ("montage failed: " + $_.Exception.Message)
}

Write-Host "---- f196 layers ----"
Select-String -Path $Log -Pattern "f196 layers" | ForEach-Object { $_.Line }
Write-Host "---- geometry ----"
Select-String -Path $Log -Pattern "dcomp video geom" | ForEach-Object { $_.Line }
Write-Host "---- ui paint ----"
Select-String -Path $Log -Pattern "dcomp ui paint" | Select-Object -Last 6 | ForEach-Object { $_.Line }
Write-Host "---- danmaku paint ----"
Select-String -Path $Log -Pattern "dcomp danmaku paint" | Select-Object -Last 4 | ForEach-Object { $_.Line }
Write-Host "---- interp ----"
Select-String -Path $Log -Pattern "interp mode=" | Select-Object -Last 5 | ForEach-Object { $_.Line }
