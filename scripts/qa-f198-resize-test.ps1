param([string]$Exe = "target/release/o11a1.exe", [int]$BootMs = 14000)
$ErrorActionPreference = "Stop"
Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public class F198W2 {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll", SetLastError=true)] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr hWnd, int idx);
    [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr hWnd, uint msg, IntPtr w, IntPtr l);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    public static List<IntPtr> Windows(uint pid) {
        var list = new List<IntPtr>();
        EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == pid && IsWindowVisible(h)) list.Add(h); return true; }, IntPtr.Zero);
        return list;
    }
    public static string Class(IntPtr h) { var sb = new StringBuilder(512); GetClassName(h, sb, 512); return sb.ToString(); }
}
"@
$log = (Join-Path (Get-Location) "target/qa/f198_probe2.log")
Remove-Item $log -ErrorAction SilentlyContinue
$env:BILI_DEBUG_LOG = $log
$env:BILI_OPEN_BVID = "BV1BBnb6aEww"
$env:BILI_SOAK_TEST = "40"
$env:BILI_PLAYER_DCOMP = "1"
$proc = Start-Process -FilePath $Exe -PassThru
Start-Sleep -Milliseconds $BootMs
$hwnd = [IntPtr]::Zero
foreach ($h in [F198W2]::Windows([uint32]$proc.Id)) {
    if ([F198W2]::Class($h) -eq "O11A1DcompPlayer") { $hwnd = $h }
}
"hwnd=$hwnd class=$([F198W2]::Class($hwnd))"
$c = New-Object F198W2+RECT; [void][F198W2]::GetClientRect($hwnd, [ref]$c)
"client before = $(($c.Right-$c.Left))x$(($c.Bottom-$c.Top))"
$r = New-Object F198W2+RECT; [void][F198W2]::GetWindowRect($hwnd, [ref]$r)
"rect before = $($r.Left),$($r.Top) $(($r.Right-$r.Left))x$(($r.Bottom-$r.Top))"
$ok = [F198W2]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, 1920, 1080, 0x0004 -bor 0x0010)
"SetWindowPos ok=$ok err=$([ComponentModel.Win32Exception]::new([Runtime.InteropServices.Marshal]::GetLastWin32Error()).Message)"
Start-Sleep -Milliseconds 600
$c = New-Object F198W2+RECT; [void][F198W2]::GetClientRect($hwnd, [ref]$c)
"client after = $(($c.Right-$c.Left))x$(($c.Bottom-$c.Top))"
$r = New-Object F198W2+RECT; [void][F198W2]::GetWindowRect($hwnd, [ref]$r)
"rect after = $($r.Left),$($r.Top) $(($r.Right-$r.Left))x$(($r.Bottom-$r.Top))"
Start-Sleep -Milliseconds 2000
$c = New-Object F198W2+RECT; [void][F198W2]::GetClientRect($hwnd, [ref]$c)
"client +2s = $(($c.Right-$c.Left))x$(($c.Bottom-$c.Top))"
$proc | Stop-Process -Force
