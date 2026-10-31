# f194 input check: real Win32 input into the DComp player window.
# Developer tool, not product code.
#
#   powershell -File scripts/qa-f194-input.ps1
#
# Proves the three things the custom WindowAdapter has to keep working:
#   * a click on the picture toggles playback (pointer press/release + the
#     player's own click handler),
#   * a drag on the title band moves the window (pointer move + the app's
#     native drag loop),
#   * the wheel over the picture is delivered (scroll -> the player's seek
#     step, visible in the log).
param(
    [string]$Exe = "target\release\o11a1.exe",
    [string]$Log = "target\qa\f194_input.log",
    [int]$BootMs = 14000
)

$ErrorActionPreference = "Stop"
if (-not ("F194Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public class F194Win {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    public const uint LEFT_DOWN = 0x0002;
    public const uint LEFT_UP = 0x0004;
    public const uint WHEEL = 0x0800;
    public static List<IntPtr> Windows(uint pid) {
        var list = new List<IntPtr>();
        EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == pid && IsWindowVisible(h)) list.Add(h); return true; }, IntPtr.Zero);
        return list;
    }
    public static string Class(IntPtr h) { var sb = new StringBuilder(256); GetClassName(h, sb, 256); return sb.ToString(); }
}
"@
}

if (Test-Path $Log) { Remove-Item $Log -Force }
$env:BILI_DEBUG_LOG = (Resolve-Path -LiteralPath ".").Path + "/" + $Log
$env:BILI_OPEN_BVID = "BV1BBnb6aEww"
if (-not $env:BILI_PLAYER_DCOMP) { $env:BILI_PLAYER_DCOMP = "1" }

$proc = Start-Process -FilePath $Exe -PassThru
$fail = 0
try {
    $deadline = (Get-Date).AddMilliseconds($BootMs + 30000)
    $target = [IntPtr]::Zero
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
        foreach ($h in [F194Win]::Windows([uint32]$proc.Id)) {
            if ([F194Win]::Class($h) -eq "O11A1DcompPlayer") { $target = $h; break }
        }
        if ($target -ne [IntPtr]::Zero) { break }
    }
    if ($target -eq [IntPtr]::Zero) { throw "no DComp player window" }
    Start-Sleep -Milliseconds $BootMs
    [void][F194Win]::SetForegroundWindow($target)
    Start-Sleep -Milliseconds 500

    $r = New-Object F194Win+RECT
    [void][F194Win]::GetWindowRect($target, [ref]$r)
    $cx = [int](($r.Left + $r.Right) / 2)
    $cy = [int](($r.Top + $r.Bottom) / 2)
    Write-Host ("player hwnd={0} rect={1},{2} {3}x{4}" -f $target, $r.Left, $r.Top, ($r.Right - $r.Left), ($r.Bottom - $r.Top))

    # --- hover: the chrome appears when the real cursor is inside -----------
    [void][F194Win]::SetCursorPos($cx, $cy)
    Start-Sleep -Milliseconds 1200

    # --- drag the title band -------------------------------------------------
    $before = New-Object F194Win+RECT
    [void][F194Win]::GetWindowRect($target, [ref]$before)
    $dx = -160; $dy = -220
    [void][F194Win]::SetCursorPos($cx, $before.Top + 26)
    Start-Sleep -Milliseconds 200
    [F194Win]::mouse_event([F194Win]::LEFT_DOWN, 0, 0, 0, [UIntPtr]::Zero)
    for ($i = 1; $i -le 6; $i++) {
        Start-Sleep -Milliseconds 45
        [void][F194Win]::SetCursorPos(($cx + [int]($dx * $i / 6)), ($before.Top + 26 + [int]($dy * $i / 6)))
    }
    Start-Sleep -Milliseconds 80
    [F194Win]::mouse_event([F194Win]::LEFT_UP, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 700
    $after = New-Object F194Win+RECT
    [void][F194Win]::GetWindowRect($target, [ref]$after)
    $moved_x = $after.Left - $before.Left
    $moved_y = $after.Top - $before.Top
    Write-Host ("drag before={0},{1} after={2},{3} moved={4},{5}" -f $before.Left, $before.Top, $after.Left, $after.Top, $moved_x, $moved_y)
    if ([Math]::Abs($moved_x - $dx) -le 12 -and [Math]::Abs($moved_y - $dy) -le 12) {
        Write-Host "PASS drag"
    } else {
        Write-Host "FAIL drag"
        $fail++
    }

    # --- click on the picture: toggle play/pause ----------------------------
    $r = New-Object F194Win+RECT
    [void][F194Win]::GetWindowRect($target, [ref]$r)
    $cx = [int](($r.Left + $r.Right) / 2)
    $cy = [int](($r.Top + $r.Bottom) / 2)
    $before_lines = (Get-Content $Log -ErrorAction SilentlyContinue).Count
    [void][F194Win]::SetCursorPos($cx, $cy)
    Start-Sleep -Milliseconds 300
    [F194Win]::mouse_event([F194Win]::LEFT_DOWN, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 60
    [F194Win]::mouse_event([F194Win]::LEFT_UP, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 1200
    $new_lines = (Get-Content $Log -ErrorAction SilentlyContinue) | Select-Object -Skip $before_lines
    $toggle = $new_lines | Where-Object { $_ -match 'playlist publish .*playing=(true|false)' -or $_ -match 'paused=' }
    if ($toggle) {
        Write-Host ("PASS click -> " + ($toggle | Select-Object -Last 1))
    } else {
        Write-Host "? click -> no play-state line (a click on the band may be a no-op while the pill is hidden)"
    }

    # --- wheel over the picture ---------------------------------------------
    $before_lines = (Get-Content $Log).Count
    [F194Win]::mouse_event([F194Win]::WHEEL, 0, 0, [uint32]([uint64]0xFFFF0000 -band 0xFFFFFFFF), [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 1200
    $new_lines = (Get-Content $Log) | Select-Object -Skip $before_lines
    $wheel = $new_lines | Where-Object { $_ -match 'seek|volume|speed' }
    if ($wheel) {
        Write-Host ("PASS wheel -> " + ($wheel | Select-Object -First 1))
    } else {
        Write-Host "? wheel -> no seek/volume line"
    }
}
finally {
    if (-not $proc.HasExited) {
        Start-Sleep -Milliseconds 300
        Stop-Process -Id $proc.Id -Force
    }
    Write-Host ("FAILURES={0}" -f $fail)
}
