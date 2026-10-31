# f202: 画质增强 acceptance — screenshots and per-tier cost.
#
# The harness in `src/qa/sr.rs` owns the schedule (`BILI_SR_MODES`): it pauses on
# one frame, applies each tier for a fixed number of seconds and logs
#
#   f202 sr apply mode=2 label=Anime4K
#   f202 sr t=14s mode=2 label=Anime4K cost_us=1830 video_s=59.8 drop=0 presents=838
#
# which is what this script follows: one capture per `apply` line, taken a
# moment later so the chain has run on the paused frame.
#
#   powershell -File scripts/qa-f202-sr.ps1 -Shot          # 1216x684 screenshots
#   powershell -File scripts/qa-f202-sr.ps1 -Stage 720     # 1280x720 cost run
#   powershell -File scripts/qa-f202-sr.ps1 -Stage 1080    # 1920x1080 cost run
param(
    [ValidateSet("shot", "720", "1080", "off", "raw")]
    [string]$Stage = "shot",
    [string]$Exe = "target/release/o11a1.exe",
    [string]$Bvid = "BV1BBnb6aEww",
    [string]$OutDir = "target/qa",
    [int]$BootMs = 16000,
    [int]$HoldSeconds = 7,
    [switch]$Capture,
    [switch]$KeepPaused
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

if (-not ("F202Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public class F202Win {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out RECT rect);
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
    public static string Class(IntPtr h) { var sb = new StringBuilder(256); GetClassName(h, sb, 256); return sb.ToString(); }
}
"@
}

$dir = Join-Path (Get-Location) $OutDir
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
$Log = Join-Path $dir ("f202_" + $Stage + ".log")
if (Test-Path $Log) { Remove-Item $Log -Force }

$env:BILI_DEBUG_LOG = $Log
$env:BILI_OPEN_BVID = $Bvid
$env:BILI_PLAYER_DCOMP = "1"
$env:BILI_SR_QA = "1"
$env:BILI_SR_SETTLE_MS = "$BootMs"
$env:BILI_SR_SEEK_MS = "120000"
if ($KeepPaused) { $env:BILI_SR_PAUSE = "1" } else { Remove-Item Env:BILI_SR_PAUSE -ErrorAction SilentlyContinue }
$env:BILI_SR_MODES = "0:$HoldSeconds,1:$HoldSeconds,2:$HoldSeconds,3:$HoldSeconds,4:$HoldSeconds"
if (-not $env:BILI_QUALITY_PICK) { $env:BILI_QUALITY_PICK = "0,6000" }
foreach ($name in @("BILI_PANEL_TEST", "BILI_FULLSCREEN_TEST", "BILI_INTERP_TEST", "BILI_F200_LAYERS")) {
    Remove-Item ("Env:" + $name) -ErrorAction SilentlyContinue
}

$Script:Log = $Log

function Get-PlayerWindow($proc, $timeoutMs) {
    # The player window is the one the log names (`player hwnd=... class=dcomp`).
    # Picking "the largest window" is wrong here: the main window is visible at
    # the same time, and resizing it leaves the picture at its saved geometry.
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    while ((Get-Date) -lt $deadline) {
        if (Test-Path $Script:Log) {
            $hit = Select-String -Path $Script:Log -Pattern "player hwnd=(\d+) class=dcomp" |
                Select-Object -Last 1
            if ($hit) {
                $m = [regex]::Match($hit.Line, "player hwnd=(\d+)")
                if ($m.Success) {
                    $handle = [IntPtr][int64]$m.Groups[1].Value
                    $cr = New-Object F202Win+RECT
                    if ([F202Win]::GetClientRect($handle, [ref]$cr)) { return $handle }
                }
            }
        }
        Start-Sleep -Milliseconds 300
    }
    throw "no player window within the deadline"
}

function Set-ClientSize($hwnd, $w, $h) {
    for ($pass = 0; $pass -lt 8; $pass++) {
        for ($i = 0; $i -lt 3; $i++) {
            [void][F202Win]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, $w, $h, 0x0004 -bor 0x0010)
            Start-Sleep -Milliseconds 120
            $c = New-Object F202Win+RECT
            [void][F202Win]::GetClientRect($hwnd, [ref]$c)
            $dw = $w - ($c.Right - $c.Left); $dh = $h - ($c.Bottom - $c.Top)
            if ($dw -eq 0 -and $dh -eq 0) { break }
            $r = New-Object F202Win+RECT
            [void][F202Win]::GetWindowRect($hwnd, [ref]$r)
            [void][F202Win]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, ($r.Right - $r.Left) + $dw, ($r.Bottom - $r.Top) + $dh, 0x0004 -bor 0x0010)
        }
        $c = New-Object F202Win+RECT
        [void][F202Win]::GetClientRect($hwnd, [ref]$c)
        if (($c.Right - $c.Left) -eq $w -and ($c.Bottom - $c.Top) -eq $h) { break }
        Start-Sleep -Milliseconds 500
    }
    Start-Sleep -Milliseconds 1200
}

function Capture($hwnd, $name) {
    [void][F202Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0010)
    [void][F202Win]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 500
    $r = New-Object F202Win+RECT
    [void][F202Win]::GetWindowRect($hwnd, [ref]$r)
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

$names = @{ 0 = "off"; 1 = "cas"; 2 = "anime4k"; 3 = "fsr"; 4 = "rtxvsr" }
$clientW = 1280; $clientH = 720
switch ($Stage) {
    "shot" { $clientW = 1216; $clientH = 684; $env:BILI_FRAME_SCALE = "0" }
    "720" { $clientW = 1280; $clientH = 720; $env:BILI_FRAME_SCALE = "1" }
    "1080" { $clientW = 1216; $clientH = 684; $env:BILI_FRAME_SCALE = "0" }
    "off" { $clientW = 1280; $clientH = 720; $env:BILI_FRAME_SCALE = "1" }
    # "raw" leaves the window at its saved layout and lets the decoder follow it,
    # so the video layer is exactly the window: the tier's output reaches the
    # capture 1:1, with no compositor scaling in between. Used to verify the
    # ported math against the same chain re-implemented in Python.
    "raw" { $clientW = 0; $clientH = 0; $env:BILI_FRAME_SCALE = "1" }
}
if ($Stage -ne "shot") {
    # The 720 / 1080 stages measure the picture the tier actually filters.
    # `BILI_FRAME_SCALE=1` makes the decoder follow the window, so a 1280x720
    # window is a genuine 720p tier; `=0` keeps the stream's own size, so the
    # 1080 stage filters 1920x1080 no matter how big the window is (the desktop's
    # work area is 1920x1032, so a 1080-tall window would be clamped anyway).
    Write-Host ("measuring frame scale: window {0}x{1}" -f $clientW, $clientH)
} else {
    Remove-Item Env:BILI_FRAME_SCALE -ErrorAction SilentlyContinue
}

$proc = Start-Process -FilePath $Exe -PassThru
try {
    $hwnd = Get-PlayerWindow $proc ($BootMs + 40000)
    Start-Sleep -Milliseconds 4000
    if ($clientW -gt 0) { Set-ClientSize $hwnd $clientW $clientH }
    $rr = New-Object F202Win+RECT
    [void][F202Win]::GetWindowRect($hwnd, [ref]$rr)
    [void][F202Win]::SetCursorPos([int](($rr.Left + $rr.Right) / 2), [int](($rr.Top + $rr.Bottom) / 2))

    $seen = @{}
    $deadline = (Get-Date).AddSeconds($BootMs / 1000 + 40 + 5 * $HoldSeconds * 2)
    while ((Get-Date) -lt $deadline) {
        if (Test-Path $Log) {
            $hits = Select-String -Path $Log -SimpleMatch "f202 sr apply mode="
            foreach ($hit in $hits) {
                $line = $hit.Line
                if ($seen.ContainsKey($line)) { continue }
                $seen[$line] = $true
                $m = [regex]::Match($line, "apply mode=(\d) label=(\S+)")
                if (-not $m.Success) { continue }
                $mode = [int]$m.Groups[1].Value
                $label = $m.Groups[2].Value
                Write-Host ("tier mode={0} label={1}" -f $mode, $label)
                if ($mode -eq 4) {
                    # RTX VSR is never enabled by this build (see ngx.rs), so
                    # there is no enhanced picture to capture: the log line above
                    # is the evidence, and no screenshot is faked.
                    Write-Host "RTX VSR: no capture (tier unavailable, see log)"
                    continue
                }
                if ($Capture) {
                    Start-Sleep -Milliseconds 1400
                    Capture $hwnd ("f202_sr_" + $names[$mode] + ".png")
                }
                if ($env:BILI_SR_MODES -eq "") { break }
            }
            if ($seen.Count -ge 5) { break }
        }
        Start-Sleep -Milliseconds 400
    }
    # A short soak after the last tier so the per-second lines exist for all five.
    Start-Sleep -Milliseconds 2000
} finally {
    Start-Sleep -Milliseconds 1000
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    Write-Host "log: $Log"
    if (Test-Path $Log) {
        Write-Host "--- f202 sr lines ---"
        Select-String -Path $Log -SimpleMatch "f202 sr " | ForEach-Object { $_.Line }
        Write-Host "--- enhance lines ---"
        Select-String -Path $Log -SimpleMatch "enhance " | Select-Object -First 20 | ForEach-Object { $_.Line }
        Write-Host "--- rtx vsr ---"
        Select-String -Path $Log -SimpleMatch "sr rtx vsr" | ForEach-Object { $_.Line }
    }
}
