# QA helper for f194 M1: "the hybrid platform did not change the main window".
# Developer tool, not product code.
#
#   powershell -File scripts/qa-f194-m1.ps1 -Hybrid 1 -Shot target\qa\f194_m1_main.png
#   powershell -File scripts/qa-f194-m1.ps1 -Hybrid 0 -Shot target\qa\f194_m1_main_control.png
#
# -Hybrid 0 sets BILI_HYBRID_PLATFORM=0, which skips the f194 platform install
# and leaves Slint's own backend selector in charge: the pre-f194 process. Both
# runs capture the main window with PrintWindow(PW_RENDERFULLCONTENT) and the
# two PNGs are compared pixel by pixel.
param(
    [ValidateSet("0", "1")]
    [string]$Hybrid = "1",
    [string]$Shot = "target\qa\f194_m1_main.png",
    [string]$Log = "target\qa\f194_m1.log",
    [string]$Exe = "target\debug\o11a1.exe",
    [int]$BootMs = 12000
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

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
    [DllImport("user32.dll")] public static extern int GetWindowTextLength(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);

    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }

    public static List<IntPtr> Windows(uint pid) {
        var list = new List<IntPtr>();
        EnumWindows((h, l) => {
            uint p; GetWindowThreadProcessId(h, out p);
            if (p == pid && IsWindowVisible(h)) {
                int n = GetWindowTextLength(h);
                if (n > 0) list.Add(h);
            }
            return true;
        }, IntPtr.Zero);
        return list;
    }

    public static string Title(IntPtr h) {
        var sb = new StringBuilder(512);
        GetWindowText(h, sb, 512);
        return sb.ToString();
    }

    public static string Class(IntPtr h) {
        var sb = new StringBuilder(512);
        GetClassName(h, sb, 512);
        return sb.ToString();
    }
}
"@
}

$dir = Split-Path -Parent $Shot
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
if (Test-Path $Log) { Remove-Item $Log -Force }

$env:BILI_HYBRID_PLATFORM = $Hybrid
$env:BILI_DEBUG_LOG = (Resolve-Path -LiteralPath ".").Path + "\" + $Log
if (-not $env:BILI_PLAYER_DCOMP) { $env:BILI_PLAYER_DCOMP = "0" }

$proc = Start-Process -FilePath $Exe -PassThru
try {
    $deadline = (Get-Date).AddMilliseconds($BootMs + 20000)
    $target = [IntPtr]::Zero
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 400
        $best = 0
        foreach ($h in [F194Win]::Windows([uint32]$proc.Id)) {
            $title = [F194Win]::Title($h)
            $class = [F194Win]::Class($h)
            $r = New-Object F194Win+RECT
            [void][F194Win]::GetWindowRect($h, [ref]$r)
            $area = ($r.Right - $r.Left) * ($r.Bottom - $r.Top)
            Write-Host ("candidate hwnd={0} class='{1}' title='{2}' {3}x{4}" -f $h, $class, $title, ($r.Right - $r.Left), ($r.Bottom - $r.Top))
            if ($class -ne "ConsoleWindowClass" -and $area -gt $best -and ($r.Right - $r.Left) -gt 640) {
                $best = $area
                $target = $h
            }
        }
        if ($target -ne [IntPtr]::Zero) { break }
    }
    if ($target -eq [IntPtr]::Zero) { throw "no main window within the deadline" }

    Start-Sleep -Milliseconds $BootMs
    [void][F194Win]::SetForegroundWindow($target)
    Start-Sleep -Milliseconds 600

    $r = New-Object F194Win+RECT
    [void][F194Win]::GetWindowRect($target, [ref]$r)
    $w = $r.Right - $r.Left
    $h = $r.Bottom - $r.Top
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    $graphics = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $graphics.GetHdc()
    $ok = [F194Win]::PrintWindow($target, $hdc, 2)
    $graphics.ReleaseHdc($hdc)
    $graphics.Dispose()
    $bmp.Save($Shot, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host ("printwindow ok={0} size={1}x{2} shot={3}" -f $ok, $w, $h, $Shot)
}
finally {
    if (-not $proc.HasExited) {
        Start-Sleep -Milliseconds 300
        $proc.CloseMainWindow() | Out-Null
        Start-Sleep -Milliseconds 800
        if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
    }
    Write-Host "--- log tail ---"
    if (Test-Path $Log) { Get-Content $Log -Tail 25 }
}
