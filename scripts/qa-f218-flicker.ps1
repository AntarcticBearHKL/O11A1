# QA helper for f218: what the *screen* shows, frame by frame, in the player
# window.
#
#   powershell -File scripts/qa-f218-flicker.ps1                      # 1080p, 4x, ramp sample
#   powershell -File scripts/qa-f218-flicker.ps1 -Interp 0            # 插帧关
#   powershell -File scripts/qa-f218-flicker.ps1 -Frames 40 -Interval 30
#
# Why a second sampler next to `qa-f213-trace.ps1`: that script reads the window
# with `PrintWindow(2)`, which goes through DWM's redirection/thumbnail path and
# is one step removed from the pixels the user sees. This script reads the
# *screen* (`CopyFromScreen`) over the client rect of a raised, foreground
# player window - the same read the eye does - and, because the sample is a
# ramp of flat luma levels (one level per source frame), the sequence of numbers
# is a **frame-order probe**: a monotonic ramp means pairs are played in order,
# an up/down oscillation means a pair is played backwards (the f213 闪烁), and
# unrelated jumps mean the visible frames are not in decode order at all.
#
# The ramp is generated here if missing (`ffmpeg`, `geq=lum=8+8*mod(N,30)`,
# 30 fps, 900 frames): every source frame is ONE flat luma level, 8 apart, so
# the whole table can be read without a human eye.
param(
    [ValidateSet("720", "1080")]
    [string]$Stage = "1080",
    [ValidateSet("0", "3")]
    [string]$Interp = "3",
    [string]$Exe = "target/release/o11a1.exe",
    [int]$Seconds = 20,
    [int]$Frames = 40,
    [int]$Interval = 30,
    # full | bottom: `bottom` measures the picture band under the danmaku rows.
    [ValidateSet("full", "bottom")]
    [string]$Region = "full",
    [string]$SamplePath = "target/f218/ramp1080p30.mp4",
    # Play the real network stream instead of the local sample: the probe the
    # 闪烁 report is actually about.
    [switch]$Network,
    # `BILI_PLAYER_DCOMP=0`: the winit fallback window (picture as a Slint Image,
    # no composition layer). A/B for "is the sink or the sampler at fault".
    [switch]$Winit,
    [switch]$NoDanmaku,
    [string]$Bvid = "BV1BBnb6aEww",
    [string]$OutDir = "target/qa/f218",
    # Pause before sampling (space through `WM_CHAR`): a static picture isolates
    # "the sampler sees different pixels" from "the player shows different
    # frames". With a paused ramp every sample must be identical.
    [switch]$Pause,
    # `BILI_F218_SLOTS=1`: one `f218 slot pts=.. luma=..` line per presented
    # slot (the per-slot frame-order probe).
    [switch]$Slots,
    # Read the window with `PrintWindow(2)` instead of the screen. DWM renders
    # the window's composition even when it is not on top, so this is the probe
    # that works while another (fullscreen) app owns the desktop.
    [switch]$Print
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

if (-not ("F218Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public class F218Win {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int cmd);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr hWnd, ref POINT point);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hWnd, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll", SetLastError=true)] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, IntPtr pid);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint attach, uint to, bool on);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();

    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }

    public static List<IntPtr> Windows(uint pid) {
        var list = new List<IntPtr>();
        EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == pid && IsWindowVisible(h)) list.Add(h); return true; }, IntPtr.Zero);
        return list;
    }
    public static string Class(IntPtr h) { var sb = new StringBuilder(512); GetClassName(h, sb, 512); return sb.ToString(); }
    public static int ClientWidth(IntPtr h) { RECT r; GetClientRect(h, out r); return r.Right - r.Left; }
    public static int ClientHeight(IntPtr h) { RECT r; GetClientRect(h, out r); return r.Bottom - r.Top; }
}
"@
}

$root = Split-Path -Parent $PSScriptRoot
$OutDir = if ([System.IO.Path]::IsPathRooted($OutDir)) { $OutDir } else { Join-Path $root $OutDir }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$clientW = if ($Stage -eq "1080") { 1920 } else { 1280 }
$clientH = if ($Stage -eq "1080") { 1080 } else { 720 }
$SamplePath = if ([System.IO.Path]::IsPathRooted($SamplePath)) { $SamplePath } else { Join-Path $root $SamplePath }
if (-not (Test-Path $SamplePath)) {
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $SamplePath) | Out-Null
    $size = "{0}x{1}" -f $clientW, $clientH
    Write-Host ("f218 ramp generating {0} ({1})" -f $SamplePath, $size)
    & ffmpeg -hide_banner -loglevel error -y -f lavfi -i "color=c=black:s=${size}:r=30:d=30,format=yuv420p,geq=lum='8+8*mod(N\,30)':cb=128:cr=128" `
        -c:v libx264 -pix_fmt yuv420p -profile:v high -preset veryfast -crf 18 -g 30 $SamplePath
}

$tag = "{0}_{1}p_interp{2}" -f $Stage, $Stage, $Interp
# One file per run: a previous instance that outlives its script would
# otherwise keep appending to the same path and interleave two sessions.
$Log = Join-Path $OutDir ("f218_{0}_{1}.log" -f $tag, (Get-Date -Format "HHmmss"))
if (Test-Path $Log) { Remove-Item $Log -Force }

Get-Process o11a1 -ErrorAction SilentlyContinue | Stop-Process -Force
for ($wait = 0; $wait -lt 40; $wait++) {
    if (-not (Get-Process o11a1 -ErrorAction SilentlyContinue)) { break }
    Start-Sleep -Milliseconds 100
}
Start-Sleep -Milliseconds 300

function Shot-Screen([int]$x, [int]$y, [int]$w, [int]$h) {
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($x, $y, 0, 0, $bmp.Size)
    $g.Dispose()
    return $bmp
}

function Shot-Print([IntPtr]$hwnd, [int]$w, [int]$h) {
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    $ok = [F218Win]::PrintWindow($hwnd, $hdc, 2)
    $g.ReleaseHdc($hdc)
    $g.Dispose()
    if (-not $ok) { $bmp.Dispose(); return $null }
    return $bmp
}

function Pixels([System.Drawing.Bitmap]$bmp) {
    $rect = New-Object System.Drawing.Rectangle 0, 0, $bmp.Width, $bmp.Height
    $data = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $bytes = New-Object byte[] ($data.Stride * $bmp.Height)
    [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $bytes, 0, $bytes.Length)
    $stride = $data.Stride
    $bmp.UnlockBits($data)
    return @{ Bytes = $bytes; Stride = $stride; Width = $bmp.Width; Height = $bmp.Height }
}

# Mean luma of the measured box. `full` = 25 %..75 % (wide). `bottom` = the
# band under the danmaku rows (y 86..96 %, x 25..75 %), which is picture only:
# the strip the order probe needs, because dense danmaku text dominates the
# wide box and hides the video.
$Band = $Region
function Box($px) {
    if ($Band -eq "bottom") {
        return @{ X0 = [int]($px.Width * 0.25); Y0 = [int]($px.Height * 0.86)
                  X1 = [int]($px.Width * 0.75); Y1 = [int]($px.Height * 0.96) }
    }
    return @{ X0 = [int]($px.Width * 0.25); Y0 = [int]($px.Height * 0.25)
              X1 = [int]($px.Width * 0.75); Y1 = [int]($px.Height * 0.75) }
}
function Stats($px) {
    $b = Box $px
    $sum = 0.0; $n = 0
    for ($py = $b.Y0; $py -lt $b.Y1; $py += 4) {
        $row = $py * $px.Stride
        for ($col = $b.X0; $col -lt $b.X1; $col += 4) {
            $i = $row + $col * 4
            $sum += 0.114 * $px.Bytes[$i] + 0.587 * $px.Bytes[$i + 1] + 0.299 * $px.Bytes[$i + 2]
            $n++
        }
    }
    if ($n -eq 0) { return 0.0 }
    return $sum / $n
}

function FrameDiff($a, $b) {
    $bx = Box $a
    $sum = 0.0; $n = 0
    for ($py = $bx.Y0; $py -lt $bx.Y1; $py += 4) {
        $row = $py * $a.Stride
        for ($col = $bx.X0; $col -lt $bx.X1; $col += 4) {
            $i = $row + $col * 4
            $sum += ([math]::Abs($a.Bytes[$i] - $b.Bytes[$i]) +
                     [math]::Abs($a.Bytes[$i + 1] - $b.Bytes[$i + 1]) +
                     [math]::Abs($a.Bytes[$i + 2] - $b.Bytes[$i + 2])) / 3.0
            $n++
        }
    }
    if ($n -eq 0) { return 0.0 }
    return $sum / $n
}

$env:BILI_DEBUG_LOG = $Log
$env:BILI_OPEN_BVID = $Bvid
$env:BILI_QUALITY_PICK = "0,6000"
$env:BILI_PLAYER_DCOMP = if ($Winit) { "0" } else { "1" }
if ($NoDanmaku) { $env:BILI_DCOMP_DANMAKU = "0"; $env:BILI_DANMAKU_STRIPS = "0" }
else { $env:BILI_DCOMP_DANMAKU = "1"; Remove-Item Env:BILI_DANMAKU_STRIPS -ErrorAction SilentlyContinue }
$env:BILI_F213_TRACE = "1"
if ($Slots) { $env:BILI_F218_SLOTS = "1" } else { Remove-Item Env:BILI_F218_SLOTS -ErrorAction SilentlyContinue }
if ($Network) { Remove-Item Env:BILI_QA_MF_FILE -ErrorAction SilentlyContinue }
else { $env:BILI_QA_MF_FILE = $SamplePath }
$env:BILI_INTERP_TEST = $Interp
if ($Interp -eq "3") { $env:BILI_INTERP_4X = "1" } else { Remove-Item Env:BILI_INTERP_4X -ErrorAction SilentlyContinue }
foreach ($name in @("BILI_DECODE", "BILI_PANEL_TEST", "BILI_FULLSCREEN_TEST", "BILI_DCOMP_UI_FORCE", "BILI_WARP_TRACE", "BILI_PACE_TRACE", "BILI_SOAK_TEST", "BILI_F214_DPI")) {
    Remove-Item ("Env:" + $name) -ErrorAction SilentlyContinue
}

$exePath = if ([System.IO.Path]::IsPathRooted($Exe)) { $Exe } else { Join-Path $root $Exe }
if (-not (Test-Path $exePath)) { throw ("missing exe: " + $exePath) }
Write-Host ("f218 exe {0} stage={1} interp={2} sample={3} log={4}" -f $exePath, $Stage, $Interp, $SamplePath, $Log)

$proc = Start-Process -FilePath $exePath -WorkingDirectory $root -PassThru

function Find-Player($proc, $timeoutMs) {
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    $started = Get-Date
    $firstSeen = [IntPtr]::Zero
    while ((Get-Date) -lt $deadline) {
        $best = [IntPtr]::Zero
        $bestArea = 0
        $fallback = [IntPtr]::Zero
        foreach ($h in [F218Win]::Windows([uint32]$proc.Id)) {
            $class = [F218Win]::Class($h)
            # The composition path has a class of its own; that IS the player.
            if ($class -eq "O11A1DcompPlayer") { return $h }
            $w = [F218Win]::ClientWidth($h)
            $hgt = [F218Win]::ClientHeight($h)
            if ($w -lt 640 -or $hgt -lt 360) { continue }
            if ($w * $hgt -gt $bestArea) {
                $best = $h
                $bestArea = $w * $hgt
            }
            # The winit fallback player is a second, large window; the main
            # window is the one this process created first. Never mistake the
            # home page for the player.
            if ($firstSeen -eq [IntPtr]::Zero) { $firstSeen = $h; continue }
            if ($h -ne $firstSeen) { $fallback = $h }
        }
        if ($fallback -ne [IntPtr]::Zero) { return $fallback }
        # 8 s with nothing but the main window: hand it back so the caller can
        # report instead of spinning (a run without a player is a real failure).
        if (((Get-Date) - $started).TotalSeconds -gt 8 -and $best -ne [IntPtr]::Zero) { return $best }
        Start-Sleep -Milliseconds 60
    }
    return [IntPtr]::Zero
}

try {
    $hwnd = Find-Player $proc 70000
    if ($hwnd -eq [IntPtr]::Zero) { throw "70 秒内没有播放器窗口" }
    Write-Host ("f218 player hwnd={0}" -f $hwnd)

    $outer = New-Object F218Win+RECT
    [void][F218Win]::GetWindowRect($hwnd, [ref]$outer)
    $deltaW = ($outer.Right - $outer.Left) - [F218Win]::ClientWidth($hwnd)
    $deltaH = ($outer.Bottom - $outer.Top) - [F218Win]::ClientHeight($hwnd)
    for ($pass = 0; $pass -lt 10; $pass++) {
        [void][F218Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, ($clientW + $deltaW), ($clientH + $deltaH), 0x0040)
        Start-Sleep -Milliseconds 250
        if ([F218Win]::ClientWidth($hwnd) -eq $clientW -and [F218Win]::ClientHeight($hwnd) -eq $clientH) { break }
    }
    [void][F218Win]::ShowWindow($hwnd, 9)
    [void][F218Win]::SetForegroundWindow($hwnd)
    for ($wait = 0; $wait -lt 40; $wait++) {
        if ([F218Win]::GetForegroundWindow() -eq $hwnd) { break }
        Start-Sleep -Milliseconds 50
        [void][F218Win]::SetForegroundWindow($hwnd)
    }

    Write-Host ("f218 playing {0}s ..." -f $Seconds)
    Start-Sleep -Seconds ($Seconds - 6)
    # Place it again *now*: a restored layout / the min-size tracker can move the
    # window during the load, and a rect that is off the desktop reads black.
    # Retry until the CLIENT sits at 0,0 on the primary monitor.
    $placed = $false
    for ($pass = 0; $pass -lt 12; $pass++) {
        $outer = New-Object F218Win+RECT
        [void][F218Win]::GetWindowRect($hwnd, [ref]$outer)
        $deltaW = ($outer.Right - $outer.Left) - [F218Win]::ClientWidth($hwnd)
        $deltaH = ($outer.Bottom - $outer.Top) - [F218Win]::ClientHeight($hwnd)
        [void][F218Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, ($clientW + $deltaW), ($clientH + $deltaH), 0x0040)
        Start-Sleep -Milliseconds 250
        # A frameless window keeps invisible resize borders, so the client sits a
        # few px inside the window rect: shift the window by that offset so the
        # CLIENT starts at 0,0 and a 1080p client fits a 1080p screen exactly.
        $probe = New-Object F218Win+POINT
        [void][F218Win]::ClientToScreen($hwnd, [ref]$probe)
        [void][F218Win]::SetWindowPos($hwnd, [IntPtr](-1), -$probe.X, -$probe.Y, 0, 0, 0x0001 -bor 0x0010)
        Start-Sleep -Milliseconds 150
        [void][F218Win]::ClientToScreen($hwnd, [ref]$probe)
        if ([F218Win]::ClientWidth($hwnd) -eq $clientW -and [F218Win]::ClientHeight($hwnd) -eq $clientH -and
            $probe.X -eq 0 -and $probe.Y -eq 0) {
            $placed = $true
            break
        }
    }
    if (-not $placed) { Write-Host "f218 note: the client did not land at 0,0" }
    [void][F218Win]::ShowWindow($hwnd, 9)
    # Put the CLIENT (not the window rect) at 0,0: a frameless window still
    # carries the invisible resize borders, so the client origin is not 0,0 and
    # the last few pixels of a 1080p client would fall off a 1080p screen.
    $origin = New-Object F218Win+POINT
    [void][F218Win]::ClientToScreen($hwnd, [ref]$origin)
    [void][F218Win]::SetWindowPos($hwnd, [IntPtr](-1), -$origin.X, -$origin.Y, 0, 0, 0x0001 -bor 0x0010)
    # A foreground app (a game, a browser) owns the foreground lock; attaching to
    # its input queue is what lets a background process raise and focus its own
    # topmost window. Without this the captures below read whatever is on top.
    $fg = [F218Win]::GetForegroundWindow()
    if ($fg -ne [IntPtr]::Zero -and $fg -ne $hwnd) {
        $fgThread = [F218Win]::GetWindowThreadProcessId($fg, [IntPtr]::Zero)
        $mine = [F218Win]::GetCurrentThreadId()
        if ($fgThread -ne 0 -and $fgThread -ne $mine) {
            [void][F218Win]::AttachThreadInput($fgThread, $mine, $true)
            [void][F218Win]::BringWindowToTop($hwnd)
            [void][F218Win]::SetForegroundWindow($hwnd)
            Start-Sleep -Milliseconds 120
            [void][F218Win]::AttachThreadInput($fgThread, $mine, $false)
        }
    }
    for ($wait = 0; $wait -lt 40; $wait++) {
        [void][F218Win]::SetForegroundWindow($hwnd)
        if ([F218Win]::GetForegroundWindow() -eq $hwnd) { break }
        Start-Sleep -Milliseconds 50
    }
    Start-Sleep -Milliseconds 700

    $origin = New-Object F218Win+POINT
    [void][F218Win]::ClientToScreen($hwnd, [ref]$origin)
    $cw = [F218Win]::ClientWidth($hwnd)
    $ch = [F218Win]::ClientHeight($hwnd)
    if ($Pause) {
        [void][F218Win]::PostMessage($hwnd, 0x0102, [IntPtr]0x20, [IntPtr]0)
        Start-Sleep -Milliseconds 1500
        Write-Host "f218 pause sent (WM_CHAR space)"
    }
    Write-Host ("f218 capture client {0}x{1} at {2},{3} source={4}" -f $cw, $ch, $origin.X, $origin.Y, $(if ($Print) { "printwindow" } else { "screen" }))
    $src = if ($Print) { "printwindow" } else { "screen" }

    $rows = @()
    $prev = $null
    # Capture only the measured box, not the whole client: at 1080p a full
    # `CopyFromScreen` costs 100-200 ms in PowerShell, which would turn a 30 ms
    # sample into a 130 ms one (and make a 30 fps ramp look like a 4x playback).
    # The measured box is the picture centre, so it is all the numbers need.
    $boxW = [int]($cw / 2)
    $boxH = [int]($ch / 2)
    if ($boxW -lt 8) { $boxW = $cw }
    if ($boxH -lt 8) { $boxH = $ch }
    $boxX = $origin.X + [int](($cw - $boxW) / 2)
    $boxY = $origin.Y + [int](($ch - $boxH) / 2)
    $clock = [System.Diagnostics.Stopwatch]::new()
    $realMs = @()
    for ($i = 0; $i -lt $Frames; $i++) {
        $clock.Restart()
        $bmp = $null
        if ($Print) { $bmp = Shot-Print $hwnd $cw $ch }
        if ($null -eq $bmp) { $bmp = Shot-Screen $boxX $boxY $boxW $boxH }
        $px = Pixels $bmp
        $luma = Stats $px
        $diff = if ($null -ne $prev) { FrameDiff $prev $px } else { 0.0 }
        $rows += [pscustomobject]@{ N = $i; Luma = $luma; Diff = $diff }
        if ($i -lt 6) { $bmp.Save((Join-Path $OutDir ("f218_{0}_frame{1}.png" -f $tag, $i)), [System.Drawing.Imaging.ImageFormat]::Png) }
        $bmp.Dispose()
        $prev = $px
        $spent = $clock.Elapsed.TotalMilliseconds
        $realMs += $spent
        $wait = $Interval - $spent
        if ($wait -gt 0) { Start-Sleep -Milliseconds ([int]$wait) }
    }

    Write-Host ""
    Write-Host "---- frames (${Interval} ms apart, screen read) ----"
    Write-Host "n   luma   diff"
    foreach ($f in $rows) { Write-Host ("{0,2}  {1,6:N2}  {2,6:N2}" -f $f.N, $f.Luma, $f.Diff) }
    $lum = $rows | ForEach-Object { $_.Luma }
    $avg = ($lum | Measure-Object -Average).Average
    $std = [math]::Sqrt((($lum | ForEach-Object { [math]::Pow($_ - $avg, 2) } | Measure-Object -Sum).Sum) / $lum.Count)
    $diffAvg = (($rows | Select-Object -Skip 1 | ForEach-Object { $_.Diff }) | Measure-Object -Average).Average
    $diffMax = (($rows | Select-Object -Skip 1 | ForEach-Object { $_.Diff }) | Measure-Object -Maximum).Maximum
    $intervalAvg = if ($realMs.Count -gt 1) { ($realMs | Select-Object -Skip 1 | Measure-Object -Average).Average } else { 0 }
    Write-Host ("f218 frames avg={0:N2} std={1:N2} diff_avg={2:N2} diff_max={3:N2} source={4} interval_avg={5:N1}ms" -f $avg, $std, $diffAvg, $diffMax, $src, $intervalAvg)
    $report = Join-Path $OutDir ("f218_{0}_frames.txt" -f $tag)
    $lines = @("stage=$Stage interp=$Interp source=$src sample=$SamplePath interval_ms=$Interval interval_avg_ms=$([math]::Round($intervalAvg,1)) box=${boxW}x${boxH}")
    $lines += "n`tluma`tdiff"
    foreach ($f in $rows) { $lines += ("{0}`t{1:N2}`t{2:N2}" -f $f.N, $f.Luma, $f.Diff) }
    $lines += ("avg={0:N2} std={1:N2} diff_avg={2:N2} diff_max={3:N2}" -f $avg, $std, $diffAvg, $diffMax)
    $lines | Set-Content -Path $report -Encoding UTF8
    Write-Host ("f218 frames table -> {0}" -f $report)
} finally {
    if ($proc -and -not $proc.HasExited) {
        Start-Sleep -Milliseconds 400
        $proc.Kill()
        $proc.WaitForExit(5000) | Out-Null
    }
}

Write-Host ""
Write-Host "---- f218 trace (last 6 slots) ----"
if (Test-Path $Log) {
    Select-String -Path $Log -Pattern "f213 slot" | Select-Object -Last 6 | ForEach-Object { Write-Host $_.Line }
    Select-String -Path $Log -Pattern "f200 layers" | Select-Object -Last 2 | ForEach-Object { Write-Host $_.Line }
} else {
    Write-Host "没有日志 $Log"
}
