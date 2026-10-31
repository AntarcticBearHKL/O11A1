# f199 shots: the login drag, the fish drag and the two 历史 states.
# Developer tool, not product code.
#
#   powershell -File scripts/qa-f199-shots.ps1 -Exe target/release/o11a1.exe
#
# Three launches, each with its own QA hook and its own log:
#   * login-drag: BILI_F199_SHOT=login-drag forces the logged-out card; a real
#     left-button drag on the bare page is injected with SendInput/mouse_event
#     and the app captures the frame WHILE the drag is in flight.
#   * fish-drag: the same, on 养鱼's left margin strip.
#   * history: the app arms 清空 (the confirm state), captures it, then switches
#     显示条数 to 50 through the real picker callback and captures the trimmed
#     list.
#
# Every run prints the grep-able log lines the report cites:
#   window drag start / window drag move / window drag end
#   f199 shot tag=... size=...
#   history limit pick=... limit=... kept=...
param(
    [string]$Exe = "target/release/o11a1.exe",
    [string]$OutDir = "docs",
    [int]$BootMs = 16000,
    [string]$Only = ""
)

$ErrorActionPreference = "Stop"
if (-not ("F199Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public class F199Win {
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
    public static List<IntPtr> Windows(uint pid) {
        var list = new List<IntPtr>();
        EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == pid && IsWindowVisible(h)) list.Add(h); return true; }, IntPtr.Zero);
        return list;
    }
    public static string Class(IntPtr h) { var sb = new StringBuilder(256); GetClassName(h, sb, 256); return sb.ToString(); }
}
"@
}

$root = (Resolve-Path -LiteralPath ".").Path
$dir = Join-Path $root $OutDir
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }

function Find-MainWindow([int]$ProcessId, [int]$TimeoutMs) {
    $deadline = (Get-Date).AddMilliseconds($TimeoutMs)
    while ((Get-Date) -lt $deadline) {
        foreach ($h in [F199Win]::Windows([uint32]$ProcessId)) {
            if ([F199Win]::Class($h) -eq "O11A1DcompPlayer") { continue }
            $r = New-Object F199Win+RECT
            [void][F199Win]::GetWindowRect($h, [ref]$r)
            if (($r.Right - $r.Left) -ge 600 -and ($r.Bottom - $r.Top) -ge 400) { return $h }
        }
        Start-Sleep -Milliseconds 300
    }
    return [IntPtr]::Zero
}

function Invoke-DragRun([string]$Kind, [int]$RelX, [int]$RelY, [int]$Dx, [int]$Dy) {
    $log = Join-Path $root ("target/qa/f199_" + $Kind + ".log")
    if (Test-Path $log) { Remove-Item $log -Force }
    $env:BILI_DEBUG_LOG = $log
    $env:BILI_CAPTURE_DIR = $dir
    $env:BILI_F199_SHOT = $Kind
    $proc = Start-Process -FilePath $Exe -PassThru
    try {
        $hwnd = Find-MainWindow $proc.Id ($BootMs + 20000)
        if ($hwnd -eq [IntPtr]::Zero) { throw "no main window" }
        Start-Sleep -Milliseconds $BootMs
        [void][F199Win]::SetForegroundWindow($hwnd)
        Start-Sleep -Milliseconds 400
        $r = New-Object F199Win+RECT
        [void][F199Win]::GetWindowRect($hwnd, [ref]$r)
        $x = $r.Left + $RelX
        $y = $r.Top + $RelY
        Write-Host ("{0} hwnd={1} rect={2},{3} {4}x{5} drag_from={6},{7}" -f $Kind, $hwnd, $r.Left, $r.Top, ($r.Right - $r.Left), ($r.Bottom - $r.Top), $x, $y)
        [void][F199Win]::SetCursorPos($x, $y)
        Start-Sleep -Milliseconds 250
        [F199Win]::mouse_event([F199Win]::LEFT_DOWN, 0, 0, 0, [UIntPtr]::Zero)
        for ($i = 1; $i -le 8; $i++) {
            Start-Sleep -Milliseconds 70
            [void][F199Win]::SetCursorPos(($x + [int]($Dx * $i / 8)), ($y + [int]($Dy * $i / 8)))
        }
        Start-Sleep -Milliseconds 500
        [F199Win]::mouse_event([F199Win]::LEFT_UP, 0, 0, 0, [UIntPtr]::Zero)
        Start-Sleep -Milliseconds 900
        $after = New-Object F199Win+RECT
        [void][F199Win]::GetWindowRect($hwnd, [ref]$after)
        $movedX = $after.Left - $r.Left
        $movedY = $after.Top - $r.Top
        Write-Host ("{0} moved={1},{2} (asked {3},{4})" -f $Kind, $movedX, $movedY, $Dx, $Dy)
        if ([Math]::Abs($movedX - $Dx) -gt 16 -or [Math]::Abs($movedY - $Dy) -gt 16) {
            Write-Host ("FAIL drag {0}: window did not follow" -f $Kind)
        } else {
            Write-Host ("PASS drag {0}" -f $Kind)
        }
    } finally {
        if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
        Start-Sleep -Milliseconds 800
        Get-Content $log -ErrorAction SilentlyContinue |
            Select-String -Pattern "window drag|f199 shot|history clear|history limit" |
            ForEach-Object { Write-Host ("LOG " + $_.Line) }
        Remove-Item Env:BILI_F199_SHOT -ErrorAction SilentlyContinue
    }
}

function Invoke-HistoryRun() {
    $log = Join-Path $root "target/qa/f199_history.log"
    if (Test-Path $log) { Remove-Item $log -Force }
    $env:BILI_DEBUG_LOG = $log
    $env:BILI_CAPTURE_DIR = $dir
    $env:BILI_F199_SHOT = "history"
    $proc = Start-Process -FilePath $Exe -PassThru
    try {
        $hwnd = Find-MainWindow $proc.Id ($BootMs + 20000)
        if ($hwnd -eq [IntPtr]::Zero) { throw "no main window" }
        [void][F199Win]::SetForegroundWindow($hwnd)
        Start-Sleep -Milliseconds 16000
        Get-Content $log -ErrorAction SilentlyContinue |
            Select-String -Pattern "f199 shot|history limit|history load" |
            ForEach-Object { Write-Host ("LOG " + $_.Line) }
    } finally {
        if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
        Start-Sleep -Milliseconds 600
        Remove-Item Env:BILI_F199_SHOT -ErrorAction SilentlyContinue
    }
}

if ($Only -eq "" -or $Only -eq "login") {
    # The card is 432 wide and centred: (40, height-60) is bare page.
    Invoke-DragRun "login-drag" 40 660 120 90
}
if ($Only -eq "" -or $Only -eq "fish") {
    # 养鱼's left margin is the 64px gutter left of the 1120px content column.
    Invoke-DragRun "fish-drag" 70 320 -120 90
}
if ($Only -eq "" -or $Only -eq "history") {
    Invoke-HistoryRun
}
