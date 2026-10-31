# QA helper for f201: the rewritten 性能 panel, in three states.
#
#   powershell -File scripts/qa-f201.ps1 -Stage playing
#   powershell -File scripts/qa-f201.ps1 -Stage paused
#   powershell -File scripts/qa-f201.ps1 -Stage idle
#
# The f201 hook (src/qa/f201.rs) opens the panel on the 性能 tab through the
# same properties the UI writes, holds the chrome up, and reads every perf_*
# row back once a second. The log therefore carries three lines per second:
#
#   f201 panel ...      exactly what the window shows (the screenshot, in text)
#   perf panel ...      the counters those strings were formatted from
#   profiler ... / interp ...   the player's own fps_presented and shown_s
#
# Cross-check: 上屏 must match the same second's `shown_s` (±1), 生成 must match
# `fps_presented`. BILI_PROFILER_LOG and BILI_INTERP_LOG are on for that.
#
# The capture is the real scan-out (CopyFromScreen): the winit player has a
# Slint tree but the panel is judged as the user sees it.
param(
    [ValidateSet("playing", "paused", "idle", "baseline", "ab")]
    [string]$Stage = "playing",
    [string]$Exe = "target/release/o11a1.exe",
    [string]$Bvid = "BV1BBnb6aEww",
    [string]$OutDir = "target/qa",
    [int]$BootMs = 12000,
    [int]$ShotMs = 26000,
    [int]$Run = 44,
    [int]$ClientW = 1760,
    [int]$ClientH = 1040
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

if (-not ("F201Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public class F201Win {
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
$Log = Join-Path $dir ("f201_" + $Stage + ".log")
if (Test-Path $Log) { Remove-Item $Log -Force }

$env:BILI_DEBUG_LOG = $Log
$env:BILI_OPEN_BVID = $Bvid
# The panel's cross-check columns.
$env:BILI_PROFILER_LOG = "1"
$env:BILI_INTERP_LOG = "1"
switch ($Stage) {
    "paused" { $env:BILI_F201_TEST = "$Run,pause" }
    "idle"   {
        # A real stall: the decoder is suspended, the ring empties, the
        # buffering plate comes up and the panel publishes 0 presents. Nothing
        # is faked - the counters are the ones the stall produced. The stall
        # fires 20 s after the first frame and the resync that ends it needs a
        # couple of seconds, so the capture (below) lands in the middle.
        $env:BILI_F201_TEST = "$Run"
        $env:BILI_STALL_TEST = "20"
        $env:BILI_STALL_HOLD_MS = "9000"
    }
    # A/B: same everything, panel never opened - the "panel costs nothing"
    # baseline the 30 fps regression is measured against.
    "baseline" {
        Remove-Item Env:BILI_F201_TEST -ErrorAction SilentlyContinue
        Remove-Item Env:BILI_STALL_TEST -ErrorAction SilentlyContinue
    }
    # Same session, 性能 body swapped out halfway: the frame rate before and
    # after is the panel's own cost (see src/qa/f201.rs).
    "ab"     { $env:BILI_F201_TEST = "$Run,ab" }
    default  { $env:BILI_F201_TEST = "$Run" }
}
# Highest tier the stream offers, so the decoder runs a real 1080p source.
if (-not $env:BILI_QUALITY_PICK) { $env:BILI_QUALITY_PICK = "0,6000" }
# The shipped player is the winit/femtovg path; the DComp player is opt-in and
# its geometry is still wrong, so it is never part of this pass.
Remove-Item Env:BILI_PLAYER_DCOMP -ErrorAction SilentlyContinue
Remove-Item Env:BILI_DCOMP_DANMAKU -ErrorAction SilentlyContinue
Remove-Item Env:BILI_PANEL_TEST -ErrorAction SilentlyContinue
Remove-Item Env:BILI_FULLSCREEN_TEST -ErrorAction SilentlyContinue
# The idle stage is the one that arms the stall (set above); every other stage
# must not inherit it from the caller's shell.
if ($Stage -ne "idle") {
    Remove-Item Env:BILI_STALL_TEST -ErrorAction SilentlyContinue
    Remove-Item Env:BILI_STALL_HOLD_MS -ErrorAction SilentlyContinue
}
# The stall fires 20 s after the first frame, so its capture has to land inside
# the hold instead of at the default 26 s (13 s of sleep already passed by then).
if ($Stage -eq "idle" -and -not $PSBoundParameters.ContainsKey("ShotMs")) { $ShotMs = 9700 }

$Script:Log = $Log

function Get-PlayerWindow($proc, $timeoutMs) {
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    $logDeadline = (Get-Date).AddMilliseconds(10000)
    while ((Get-Date) -lt $logDeadline) {
        if (Test-Path $Script:Log) {
            $hit = Select-String -Path $Script:Log -SimpleMatch "player session open" | Where-Object { $_.Line -notmatch "hwnd=0 " } | Select-Object -Last 1
            if (-not $hit) { $hit = Select-String -Path $Script:Log -SimpleMatch "player hwnd=" | Select-Object -Last 1 }
            if ($hit) {
                $m = [regex]::Match($hit.Line, "hwnd=(\d+)")
                if ($m.Success) {
                    $candidate = [IntPtr][int64]$m.Groups[1].Value
                    $cr = New-Object F201Win+RECT
                    if ([F201Win]::GetClientRect($candidate, [ref]$cr)) { return $candidate }
                }
            }
        }
        Start-Sleep -Milliseconds 300
    }
    while ((Get-Date) -lt $deadline) {
        $best = $null; $bestArea = 0
        foreach ($h in [F201Win]::Windows([uint32]$proc.Id)) {
            $c = [F201Win]::Class($h)
            if ($c -eq "ConsoleWindowClass" -or $c -eq "Shell_TrayWnd") { continue }
            $cr = New-Object F201Win+RECT
            [void][F201Win]::GetClientRect($h, [ref]$cr)
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
            [void][F201Win]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, $w, $h, 0x0004 -bor 0x0010)
            Start-Sleep -Milliseconds 120
            $c = New-Object F201Win+RECT
            [void][F201Win]::GetClientRect($hwnd, [ref]$c)
            $dw = $w - ($c.Right - $c.Left); $dh = $h - ($c.Bottom - $c.Top)
            if ($dw -eq 0 -and $dh -eq 0) { break }
            $r = New-Object F201Win+RECT
            [void][F201Win]::GetWindowRect($hwnd, [ref]$r)
            [void][F201Win]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, ($r.Right - $r.Left) + $dw, ($r.Bottom - $r.Top) + $dh, 0x0004 -bor 0x0010)
        }
        $c = New-Object F201Win+RECT
        [void][F201Win]::GetClientRect($hwnd, [ref]$c)
        if (($c.Right - $c.Left) -eq $w -and ($c.Bottom - $c.Top) -eq $h) { break }
        Start-Sleep -Milliseconds 400
    }
    Start-Sleep -Milliseconds 1200
}

function Capture($hwnd, $name) {
    [void][F201Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0010)
    [void][F201Win]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 900
    $r = New-Object F201Win+RECT
    [void][F201Win]::GetWindowRect($hwnd, [ref]$r)
    $w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
    if ($w -le 0 -or $h -le 0) { Write-Host "capture skipped: empty rect"; return }
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($r.Left, $r.Top, 0, 0, (New-Object System.Drawing.Size($w, $h)))
    $g.Dispose()
    $path = Join-Path $dir $name
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host ("capture {0} window={1}x{2}" -f $name, $w, $h)
}

$proc = Start-Process -FilePath $Exe -PassThru
try {
    $hwnd = Get-PlayerWindow $proc ($BootMs + 40000)
    Start-Sleep -Milliseconds $BootMs
    [void][F201Win]::SetForegroundWindow($hwnd)
    $rr = New-Object F201Win+RECT
    [void][F201Win]::GetWindowRect($hwnd, [ref]$rr)
    [void][F201Win]::SetCursorPos([int](($rr.Left + $rr.Right) / 2), [int](($rr.Top + $rr.Bottom) / 2))
    Set-ClientSize $hwnd $ClientW $ClientH
    # The panel opened on its own when the player did; the shot lands after the
    # hook's action (pause / idle seek) has had three publishes of its own.
    Start-Sleep -Milliseconds $ShotMs
    Capture $hwnd ("f201_perf_" + $Stage + ".png")
    # The buffering state lasts about one publish (the watchdog resyncs fast), so
    # the idle stage takes a second shot one second later and the pair is
    # inspected for the one that landed inside it.
    if ($Stage -eq "idle") {
        Start-Sleep -Milliseconds 1300
        Capture $hwnd "f201_perf_idle_b.png"
    }
    Start-Sleep -Milliseconds 2500
} finally {
    Start-Sleep -Milliseconds 1000
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    Write-Host "log: $Log"
    if (Test-Path $Log) {
        Write-Host "--- f201 panel (what the window showed) ---"
        Select-String -Path $Log -SimpleMatch "f201 panel t=" | ForEach-Object { $_.Line } | Select-Object -Last 12
        Write-Host "--- perf panel (the counters behind it) ---"
        Select-String -Path $Log -SimpleMatch "perf panel t_ms=" | ForEach-Object { $_.Line } | Select-Object -Last 6
        Write-Host "--- player: fps_presented / shown_s ---"
        Select-String -Path $Log -Pattern "profiler fps_presented|interp mode=" | ForEach-Object { $_.Line } | Select-Object -Last 8
    }
}
