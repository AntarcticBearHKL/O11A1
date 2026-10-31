# f198 geometry evidence: three window sizes with the panel open, then
# fullscreen. Developer tool, not product code.
#
#   powershell -File scripts/qa-f198-geom.ps1
param(
    [string]$Exe = "target/release/o11a1.exe",
    [string]$Bvid = "BV1BBnb6aEww",
    [int]$BootMs = 14000,
    [string]$OutDir = "target/qa"
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
if (-not ("F198Geom" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public class F198Geom {
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
$settingsPath = Join-Path (Get-Location) "settings.json"
$settingsBackup = [IO.File]::ReadAllText($settingsPath, [Text.Encoding]::UTF8)

function Find-Player($proc, $timeoutMs) {
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    while ((Get-Date) -lt $deadline) {
        foreach ($h in [F198Geom]::Windows([uint32]$proc.Id)) {
            if ([F198Geom]::Class($h) -eq "O11A1DcompPlayer") { return $h }
        }
        Start-Sleep -Milliseconds 300
    }
    throw "no player window"
}

function Set-ClientSize($hwnd, $w, $h) {
    for ($i = 0; $i -lt 20; $i++) {
        $c = New-Object F198Geom+RECT
        [void][F198Geom]::GetClientRect($hwnd, [ref]$c)
        $dw = $w - ($c.Right - $c.Left); $dh = $h - ($c.Bottom - $c.Top)
        if ($dw -eq 0 -and $dh -eq 0) { Start-Sleep -Milliseconds 700; return }
        $r = New-Object F198Geom+RECT
        [void][F198Geom]::GetWindowRect($hwnd, [ref]$r)
        [void][F198Geom]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, ($r.Right - $r.Left) + $dw, ($r.Bottom - $r.Top) + $dh, 0x0004 -bor 0x0010)
        Start-Sleep -Milliseconds 300
    }
}

function Save-Shot($hwnd, $name) {
    # Topmost + foreground before the scan-out copy: another app on this desktop
    # would otherwise be what CopyFromScreen captures.
    [void][F198Geom]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0010)
    [void][F198Geom]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 900
    $r = New-Object F198Geom+RECT
    [void][F198Geom]::GetWindowRect($hwnd, [ref]$r)
    $w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
    if ($w -le 0 -or $h -le 0) { throw "empty rect $name" }
    $bmp = New-Object System.Drawing.Bitmap -ArgumentList ([int]$w), ([int]$h)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($r.Left, $r.Top, 0, 0, (New-Object System.Drawing.Size([int]$w, [int]$h)))
    $g.Dispose()
    $path = Join-Path $dir $name
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host ("shot {0} {1}x{2}" -f $name, $w, $h)
}

function Nudge($hwnd) {
    $r = New-Object F198Geom+RECT
    [void][F198Geom]::GetWindowRect($hwnd, [ref]$r)
    $Script:NX = 1 - $Script:NX
    [void][F198Geom]::SetCursorPos([int](($r.Left + $r.Right) / 2) + $Script:NX, [int](($r.Top + $r.Bottom) / 2))
}
$Script:NX = 0

# ---- pass 1: three sizes, panel open -------------------------------------
$log1 = Join-Path $dir "f198_geom_panels.log"
Remove-Item $log1 -ErrorAction SilentlyContinue
$patched = [regex]::Replace($settingsBackup, '"player_layout"\s*:\s*(\[[^\]]*\]|null)', '"player_layout": [0, 0, 1280, 720]')
[IO.File]::WriteAllText($settingsPath, $patched, (New-Object Text.UTF8Encoding($false)))

$env:BILI_DEBUG_LOG = $log1
$env:BILI_OPEN_BVID = $Bvid
$env:BILI_SOAK_TEST = "80"
$env:BILI_PLAYER_DCOMP = "1"
$env:BILI_DCOMP_DANMAKU = "1"
$env:BILI_INTERP_TEST = "3"
$env:BILI_INTERP_4X = "1"
# Right strip, then the left one, 20 s apart: the band shrinks twice.
$env:BILI_PANEL_TEST = "1,1,0,20000"

$proc = Start-Process -FilePath $Exe -PassThru
try {
    $hwnd = Find-Player $proc ($BootMs + 30000)
    Start-Sleep -Milliseconds $BootMs
    [void][F198Geom]::SetForegroundWindow($hwnd)
    Set-ClientSize $hwnd 1280 720
    Start-Sleep -Milliseconds 1500
    Save-Shot $hwnd "f198_geom_1280x720.png"
    Set-ClientSize $hwnd 960 540
    Start-Sleep -Milliseconds 1500
    Save-Shot $hwnd "f198_geom_960x540.png"
    Set-ClientSize $hwnd 800 800
    # Wait for the panel hook's first step, then capture the shrunken band.
    $deadline = (Get-Date).AddMilliseconds(40000)
    while ((Get-Date) -lt $deadline) {
        if (Select-String -Path $log1 -SimpleMatch "paneltest set step=0" -Quiet) { break }
        Start-Sleep -Milliseconds 300
    }
    Start-Sleep -Milliseconds 4000
    $env:BILI_PANEL_TEST = $null
    Save-Shot $hwnd "f198_geom_panel.png"
} finally {
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    Start-Sleep -Milliseconds 900
}

# ---- pass 2: fullscreen ---------------------------------------------------
$log2 = Join-Path $dir "f198_geom_fullscreen.log"
Remove-Item $log2 -ErrorAction SilentlyContinue
Remove-Item Env:BILI_PANEL_TEST -ErrorAction SilentlyContinue
$env:BILI_DEBUG_LOG = $log2
$env:BILI_FULLSCREEN_TEST = "9000"
$proc = Start-Process -FilePath $Exe -PassThru
try {
    $hwnd = Find-Player $proc ($BootMs + 30000)
    Start-Sleep -Milliseconds ($BootMs + 1000)
    $deadline = (Get-Date).AddMilliseconds(25000)
    $ok = $false
    while ((Get-Date) -lt $deadline) {
        $r = New-Object F198Geom+RECT
        [void][F198Geom]::GetWindowRect($hwnd, [ref]$r)
        if (($r.Right - $r.Left) -ge 1900 -and ($r.Bottom - $r.Top) -ge 1000) { $ok = $true; break }
        Start-Sleep -Milliseconds 250
    }
    if (-not $ok) { Write-Host "fullscreen never covered the monitor" }
    Save-Shot $hwnd "f198_geom_fullscreen.png"
} finally {
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    Start-Sleep -Milliseconds 900
    Remove-Item Env:BILI_FULLSCREEN_TEST -ErrorAction SilentlyContinue
    [IO.File]::WriteAllText($settingsPath, $settingsBackup, (New-Object Text.UTF8Encoding($false)))
}

# ---- montage --------------------------------------------------------------
Add-Type -AssemblyName System.Drawing
$tiles = @("f198_geom_1280x720.png", "f198_geom_960x540.png", "f198_geom_panel.png") |
    ForEach-Object { Join-Path $dir $_ }
$imgs = @()
foreach ($t in $tiles) { if (Test-Path $t) { $imgs += [System.Drawing.Image]::FromFile($t) } }
if ($imgs.Count -gt 0) {
    $w = 0; $h = 0
    foreach ($i in $imgs) { $w += [int]$i.Width; if ([int]$i.Height -gt $h) { $h = [int]$i.Height } }
    $w += 12 * ($imgs.Count - 1)
    $bmp = New-Object System.Drawing.Bitmap -ArgumentList ([int]$w), ([int]$h)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::FromArgb(24, 24, 28))
    $x = 0
    foreach ($i in $imgs) { $g.DrawImage($i, $x, 0, [int]$i.Width, [int]$i.Height); $x += [int]$i.Width + 12 }
    $g.Dispose()
    $bmp.Save((Join-Path $dir "f198_geom_3sizes.png"), [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    foreach ($i in $imgs) { $i.Dispose() }
    Write-Host "montage f198_geom_3sizes.png"
}

Write-Host "---- geometry (panels) ----"
Select-String -Path $log1 -Pattern "dcomp video geom|paneltest" | ForEach-Object { $_.Line }
Write-Host "---- geometry (fullscreen) ----"
Select-String -Path $log2 -Pattern "dcomp video geom|fullscreen" | ForEach-Object { $_.Line }
