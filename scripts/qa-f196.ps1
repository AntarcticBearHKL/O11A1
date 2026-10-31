# QA helper for f196: danmaku on its own composition layer, geometry, 1080p/120.
# Developer tool, not product code.
#
#   powershell -File scripts/qa-f196.ps1 -Stage danmaku
#   powershell -File scripts/qa-f196.ps1 -Stage panel
#   powershell -File scripts/qa-f196.ps1 -Stage fullscreen
#   powershell -File scripts/qa-f196.ps1 -Stage p1080
#   powershell -File scripts/qa-f196.ps1 -Stage winit     (-Dcomp:$false, the default path)
#
# Every capture is the real scan-out of the player window (CopyFromScreen). The
# danmaku stage also reports, per column band, how many pixels differ from the
# window's own backdrop above/below the picture rect - the f195 bug (comments on
# the letterbox bars) was visible exactly there.
param(
    [ValidateSet("danmaku", "panel", "fullscreen", "p1080", "p1080plain", "winit")]
    [string]$Stage = "danmaku",
    [string]$Exe = "target/release/o11a1.exe",
    [string]$Bvid = "BV1BBnb6aEww",
    [string]$OutDir = "target/qa",
    [bool]$Dcomp = $true,
    [int]$BootMs = 12000,
    [int]$Soak = 40
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

if (-not ("F196Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public class F196Win {
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
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);

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
$Log = Join-Path $dir ("f196_" + $Stage + ".log")
if (Test-Path $Log) { Remove-Item $Log -Force }

$env:BILI_DEBUG_LOG = $Log
$env:BILI_OPEN_BVID = $Bvid
$env:BILI_SOAK_TEST = "$Soak"
if ($Dcomp) { $env:BILI_PLAYER_DCOMP = "1" } else { Remove-Item Env:BILI_PLAYER_DCOMP -ErrorAction SilentlyContinue }
Remove-Item Env:BILI_PANEL_TEST -ErrorAction SilentlyContinue
Remove-Item Env:BILI_FULLSCREEN_TEST -ErrorAction SilentlyContinue
Remove-Item Env:BILI_INTERP_TEST -ErrorAction SilentlyContinue
Remove-Item Env:BILI_INTERP_4X -ErrorAction SilentlyContinue
switch ($Stage) {
    "panel" { $env:BILI_PANEL_TEST = "0,1,0,30000" }
    # The hook toggles fullscreen ON at this delay and off 4 s later; the capture
        # script arrives after the boot wait, so it must fire after that.
        "fullscreen" { $env:BILI_FULLSCREEN_TEST = "16000" }
    "p1080" {
        $env:BILI_INTERP_TEST = "3"
        $env:BILI_INTERP_4X = "1"
        $env:BILI_INTERP_PROBE = "1"
    }
}

function Get-PlayerWindow($proc, $timeoutMs) {
    # The player writes its own HWND (`frame player style=… hwnd=…`), which is
    # the only reliable identity when winit owns the window class.
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    $logDeadline = (Get-Date).AddMilliseconds(8000)
    while ((Get-Date) -lt $logDeadline) {
        if (Test-Path $Script:Log) {
            $hit = Select-String -Path $Script:Log -SimpleMatch "player hwnd=" | Select-Object -Last 1
            if (-not $hit) { $hit = Select-String -Path $Script:Log -SimpleMatch "player session open" | Where-Object { $_.Line -notmatch "hwnd=0 " } | Select-Object -Last 1 }
            if (-not $hit) { $hit = Select-String -Path $Script:Log -SimpleMatch "frame player style=" | Select-Object -Last 1 }
            if ($hit) {
                $m = [regex]::Match($hit.Line, "hwnd=(\d+)")
                if ($m.Success) {
                    $candidate = [IntPtr][int64]$m.Groups[1].Value
                    $cr = New-Object F196Win+RECT
                    if ([F196Win]::GetClientRect($candidate, [ref]$cr)) { return $candidate }
                }
            }
        }
        Start-Sleep -Milliseconds 300
    }
    while ((Get-Date) -lt $deadline) {
        foreach ($h in [F196Win]::Windows([uint32]$proc.Id)) {
            $c = [F196Win]::Class($h)
            if ($c -eq "ConsoleWindowClass") { continue }
            if ($c -eq "O11A1DcompPlayer") { return $h }
            if ($c -like "*Player*") { return $h }
        }
        # winit path: the player is a plain "Window Class" HWND like the main
        # window. Take the largest visible one with a real client area.
        $best = $null; $bestArea = 0
        foreach ($h in [F196Win]::Windows([uint32]$proc.Id)) {
            $c = [F196Win]::Class($h)
            if ($c -eq "ConsoleWindowClass" -or $c -eq "Shell_TrayWnd") { continue }
            # The main window is titled "O11A1 …"; the player carries the video
            # title. Take the largest window that is not the main one.
            $title = [F196Win]::Title($h)
            # The main window's title is short ASCII ("O11A1 …"); the player's is
            # the video title (Chinese). Anything short and ASCII is not it.
            if ($title.Length -le 8 -and $title -match '^[ -~]*$') { continue }
            $cr = New-Object F196Win+RECT
            [void][F196Win]::GetClientRect($h, [ref]$cr)
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
    # The app re-applies its remembered geometry shortly after show, so one
    # resize can be undone; retry until the client rect is the requested one.
    for ($pass = 0; $pass -lt 8; $pass++) {
    for ($i = 0; $i -lt 3; $i++) {
        [void][F196Win]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, $w, $h, 0x0004 -bor 0x0010)
        Start-Sleep -Milliseconds 120
        $c = New-Object F196Win+RECT
        [void][F196Win]::GetClientRect($hwnd, [ref]$c)
        $dw = $w - ($c.Right - $c.Left); $dh = $h - ($c.Bottom - $c.Top)
        if ($dw -eq 0 -and $dh -eq 0) { break }
        $r = New-Object F196Win+RECT
        [void][F196Win]::GetWindowRect($hwnd, [ref]$r)
        [void][F196Win]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, ($r.Right - $r.Left) + $dw, ($r.Bottom - $r.Top) + $dh, 0x0004 -bor 0x0010)
    }
    $c = New-Object F196Win+RECT
    [void][F196Win]::GetClientRect($hwnd, [ref]$c)
    if (($c.Right - $c.Left) -eq $w -and ($c.Bottom - $c.Top) -eq $h) { break }
    Start-Sleep -Milliseconds 500
    }
    Start-Sleep -Milliseconds 900
}

function Capture($hwnd, $name) {
    [void][F196Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0010)
    [void][F196Win]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 700
    $r = New-Object F196Win+RECT
    [void][F196Win]::GetWindowRect($hwnd, [ref]$r)
    $c = New-Object F196Win+RECT
    [void][F196Win]::GetClientRect($hwnd, [ref]$c)
    $w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
    Write-Host ("capture-target hwnd={0} rect={1},{2} {3}x{4} class={5} title={6}" -f $hwnd, $r.Left, $r.Top, $w, $h, [F196Win]::Class($hwnd), [F196Win]::Title($hwnd))
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

$Script:Log = $Log
$proc = Start-Process -FilePath $Exe -PassThru
try {
    $hwnd = Get-PlayerWindow $proc ($BootMs + 30000)
    Start-Sleep -Milliseconds $BootMs
    [void][F196Win]::SetForegroundWindow($hwnd)
    $rr = New-Object F196Win+RECT
    [void][F196Win]::GetWindowRect($hwnd, [ref]$rr)
    [void][F196Win]::SetCursorPos(($rr.Left + $rr.Right) / 2, ($rr.Top + $rr.Bottom) / 2)

    switch ($Stage) {
        "danmaku" {
            Set-ClientSize $hwnd 800 800
            Start-Sleep -Milliseconds 2000
            Capture $hwnd "f196_danmaku_in_video.png"
            Start-Sleep -Milliseconds 1500
            Capture $hwnd "f196_danmaku_in_video_b.png"
        }
        "winit" {
            Set-ClientSize $hwnd 800 800
            Start-Sleep -Milliseconds 2000
            Capture $hwnd "f196_danmaku_in_video_winit.png"
        }
        "panel" {
            $deadline = (Get-Date).AddMilliseconds(30000)
            $opened = $false
            while ((Get-Date) -lt $deadline) {
                if ((Test-Path $Log) -and (Select-String -Path $Log -SimpleMatch "paneltest set step=0" -Quiet)) { $opened = $true; break }
                Start-Sleep -Milliseconds 400
            }
            if (-not $opened) { throw "panel hook never fired" }
            Start-Sleep -Milliseconds 1500
            Set-ClientSize $hwnd 1280 800
            Capture $hwnd "f196_geom_panel.png"
        }
        "fullscreen" {
            $deadline = (Get-Date).AddMilliseconds(30000)
            $shot = $false
            while ((Get-Date) -lt $deadline) {
                $r = New-Object F196Win+RECT
                [void][F196Win]::GetWindowRect($hwnd, [ref]$r)
                if (($r.Right - $r.Left) -ge 1900 -and ($r.Bottom - $r.Top) -ge 1000) {
                    Start-Sleep -Milliseconds 600
                    Capture $hwnd "f196_geom_fullscreen.png"
                    $shot = $true
                    break
                }
                Start-Sleep -Milliseconds 250
            }
            if (-not $shot) { throw "fullscreen never covered the monitor" }
        }
        "p1080plain" {
            Set-ClientSize $hwnd 1920 1080
            Start-Sleep -Milliseconds 5000
            Capture $hwnd "f196_1080p_danmaku.png"
            Start-Sleep -Milliseconds 2500
            Capture $hwnd "f196_1080p_danmaku_b.png"
        }
        "p1080" {
            Set-ClientSize $hwnd 1920 1080
            Start-Sleep -Milliseconds 9000
            Capture $hwnd "f196_1080p120.png"
            Start-Sleep -Milliseconds 3000
            Capture $hwnd "f196_1080p120_b.png"
        }
    }
    # Let the soak log at least two 5 s windows before the process quits itself.
    Start-Sleep -Milliseconds 6000
}
finally {
    if (-not $proc.HasExited) { Start-Sleep -Milliseconds 300; Stop-Process -Id $proc.Id -Force }
    Write-Host "--- layers ---"
    if (Test-Path $Log) { Select-String -Path $Log -SimpleMatch "f196 layers" | ForEach-Object { $_.Line } }
    Write-Host "--- danmaku layer ---"
    if (Test-Path $Log) { Select-String -Path $Log -SimpleMatch "dcomp danmaku" | ForEach-Object { $_.Line } }
    Write-Host "--- geom ---"
    if (Test-Path $Log) { Select-String -Path $Log -SimpleMatch "dcomp video geom" | ForEach-Object { $_.Line } }
    Write-Host "--- interp ---"
    if (Test-Path $Log) { Select-String -Path $Log -SimpleMatch "interp mode=" | Select-Object -Last 8 | ForEach-Object { $_.Line } }
    Write-Host "--- log tail ---"
    if (Test-Path $Log) { Get-Content $Log -Tail 10 }
}
