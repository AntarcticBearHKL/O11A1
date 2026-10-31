# f198: does PrintWindow see the DComp player window at all?
# Developer tool, not product code.
param([string]$Exe = "target/release/o11a1.exe", [int]$BootMs = 14000)
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
if (-not ("F198Pw" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public class F198Pw {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll", SetLastError=true)] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);
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
$log = Join-Path (Get-Location) "target/qa/f198_printwindow.log"
Remove-Item $log -ErrorAction SilentlyContinue
$env:BILI_DEBUG_LOG = $log
$env:BILI_OPEN_BVID = "BV1BBnb6aEww"
$env:BILI_SOAK_TEST = "30"
$env:BILI_PLAYER_DCOMP = "1"
$env:BILI_DCOMP_DANMAKU = "1"
$proc = Start-Process -FilePath $Exe -PassThru
try {
    $hwnd = [IntPtr]::Zero
    $deadline = (Get-Date).AddMilliseconds($BootMs + 30000)
    while ((Get-Date) -lt $deadline) {
        foreach ($h in [F198Pw]::Windows([uint32]$proc.Id)) {
            if ([F198Pw]::Class($h) -eq "O11A1DcompPlayer") { $hwnd = $h; break }
        }
        if ($hwnd -ne [IntPtr]::Zero) { break }
        Start-Sleep -Milliseconds 300
    }
    if ($hwnd -eq [IntPtr]::Zero) { throw "no player" }
    Start-Sleep -Milliseconds ($BootMs + 3000)
    [void][F198Pw]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 800
    $r = New-Object F198Pw+RECT
    [void][F198Pw]::GetWindowRect($hwnd, [ref]$r)
    $w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
    foreach ($flags in @(0, 2)) {
        $bmp = New-Object System.Drawing.Bitmap -ArgumentList ([int]$w), ([int]$h)
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $hdc = $g.GetHdc()
        $ok = [F198Pw]::PrintWindow($hwnd, $hdc, $flags)
        $g.ReleaseHdc($hdc)
        $g.Dispose()
        $nonBlack = 0
        for ($y = 0; $y -lt $h; $y += 17) {
            for ($x = 0; $x -lt $w; $x += 17) {
                $c = $bmp.GetPixel($x, $y)
                if (($c.R + $c.G + $c.B) -gt 24) { $nonBlack++ }
            }
        }
        $path = Join-Path (Get-Location) ("target/qa/f198_printwindow_{0}.png" -f $flags)
        $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
        $bmp.Dispose()
        "PrintWindow flags={0} ok={1} nonBlackPixels={2} size={3}x{4}" -f $flags, $ok, $nonBlack, $w, $h
    }
} finally {
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
}
