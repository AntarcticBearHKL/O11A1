# QA helper for f194 M2: open a real player and capture it.
# Developer tool, not product code.
#
#   powershell -File scripts/qa-f194-player.ps1 -Shot target\qa\f194_m2_player.png
#
# Env knobs it passes through: BILI_PLAYER_DCOMP (1 default), BILI_PLAYER_DCOMP=0
# for the winit fallback run. The player window's class is set by
# `src/dcomp/player_window.rs`; a winit player window has class `Window Class`.
param(
    [string]$Shot = "target/qa/f194_m2_player.png",
    [string]$Log = "target/qa/f194_m2.log",
    [string]$Bvid = "BV1BBnb6aEww",
    [string]$Exe = "target/debug/o11a1.exe",
    [int]$BootMs = 14000,
    [double]$Seconds = 0,
    [string]$Extra = "",
    [string]$WantClass = "O11A1DcompPlayer",
    [string]$TitleLike = ""
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms

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
    [DllImport("user32.dll")] public static extern int GetWindowTextLength(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);

    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }

    public static List<IntPtr> Windows(uint pid) {
        var list = new List<IntPtr>();
        EnumWindows((h, l) => {
            uint p; GetWindowThreadProcessId(h, out p);
            if (p == pid && IsWindowVisible(h)) list.Add(h);
            return true;
        }, IntPtr.Zero);
        return list;
    }

    public static string Title(IntPtr h) {
        var sb = new StringBuilder(512); GetWindowText(h, sb, 512); return sb.ToString();
    }

    public static string Class(IntPtr h) {
        var sb = new StringBuilder(512); GetClassName(h, sb, 512); return sb.ToString();
    }
}
"@
}

$dir = Split-Path -Parent $Shot
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
if (Test-Path $Log) { Remove-Item $Log -Force }

$env:BILI_DEBUG_LOG = (Resolve-Path -LiteralPath ".").Path + "/" + $Log
$env:BILI_OPEN_BVID = $Bvid
if (-not $env:BILI_PLAYER_DCOMP) { $env:BILI_PLAYER_DCOMP = "1" }
if ($Seconds -gt 0) { $env:BILI_SOAK_TEST = "$Seconds" }
if ($Extra) {
    foreach ($pair in $Extra.Split(";")) {
        if (-not $pair) { continue }
        $i = $pair.IndexOf("=")
        if ($i -gt 0) { Set-Item -Path ("env:" + $pair.Substring(0, $i)) -Value $pair.Substring($i + 1) }
    }
}

$proc = Start-Process -FilePath $Exe -PassThru
try {
    $deadline = (Get-Date).AddMilliseconds($BootMs + 30000)
    $target = [IntPtr]::Zero
    $class = ""
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
        $best = 0
        foreach ($h in [F194Win]::Windows([uint32]$proc.Id)) {
            $c = [F194Win]::Class($h)
            if ($c -eq "ConsoleWindowClass") { continue }
            $r = New-Object F194Win+RECT
            [void][F194Win]::GetWindowRect($h, [ref]$r)
            $area = ($r.Right - $r.Left) * ($r.Bottom - $r.Top)
            Write-Host ("candidate hwnd={0} class='{1}' title='{2}' {3}x{4}" -f $h, $c, [F194Win]::Title($h), ($r.Right - $r.Left), ($r.Bottom - $r.Top))
            $title_ok = (-not $TitleLike) -or ([F194Win]::Title($h) -like ("*" + $TitleLike + "*"))
            if ($c -eq $WantClass -and $title_ok -and $area -gt 200000 -and $area -gt $best) { $best = $area; $target = $h; $class = $c }
        }
        if ($target -ne [IntPtr]::Zero) { break }
    }
    if ($target -eq [IntPtr]::Zero) { throw "no player window of class $WantClass within the deadline" }

    Start-Sleep -Milliseconds $BootMs
    [void][F194Win]::SetForegroundWindow($target)
    Start-Sleep -Milliseconds 300
    # Hover: the player's chrome (title band + control pill) only shows when the
    # real cursor is inside the window.
    $rr = New-Object F194Win+RECT
    [void][F194Win]::GetWindowRect($target, [ref]$rr)
    [void][F194Win]::SetCursorPos(($rr.Left + $rr.Right) / 2, ($rr.Top + $rr.Bottom) / 2)
    Start-Sleep -Milliseconds 1500

    $r = New-Object F194Win+RECT
    [void][F194Win]::GetWindowRect($target, [ref]$r)
    $w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    $ok = [F194Win]::PrintWindow($target, $hdc, 2)
    $g.ReleaseHdc($hdc); $g.Dispose()
    $bmp.Save($Shot, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host ("printwindow ok={0} class={1} size={2}x{3} shot={4}" -f $ok, $class, $w, $h, $Shot)

    # The same rect from the screen (what the compositor actually scanned out).
    $screen = [System.IO.Path]::ChangeExtension($Shot, ".screen.png")
    $bmp2 = New-Object System.Drawing.Bitmap($w, $h)
    $g2 = [System.Drawing.Graphics]::FromImage($bmp2)
    $g2.CopyFromScreen($r.Left, $r.Top, 0, 0, (New-Object System.Drawing.Size($w, $h)))
    $g2.Dispose()
    $bmp2.Save($screen, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp2.Dispose()
    Write-Host ("screen shot {0}" -f $screen)

    # The whole desktop, for context: what is behind the player window.
    $full = [System.IO.Path]::ChangeExtension($Shot, ".desktop.png")
    $vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $bmp3 = New-Object System.Drawing.Bitmap($vs.Width, $vs.Height)
    $g3 = [System.Drawing.Graphics]::FromImage($bmp3)
    $g3.CopyFromScreen($vs.X, $vs.Y, 0, 0, (New-Object System.Drawing.Size($vs.Width, $vs.Height)))
    $g3.Dispose()
    $bmp3.Save($full, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp3.Dispose()
    Write-Host ("desktop shot {0}" -f $full)
}
finally {
    if (-not $proc.HasExited) {
        Start-Sleep -Milliseconds 300
        if ($Seconds -gt 0) {
            # A soak run exits by itself; give it the time it needs.
            $wait = [Math]::Min(($Seconds + 30) * 1000, 120000)
            [void]$proc.WaitForExit($wait)
        }
        if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
    }
    Write-Host "--- log tail ---"
    if (Test-Path $Log) { Get-Content $Log -Tail 30 }
}
