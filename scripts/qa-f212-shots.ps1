# QA helper for f212: the 搜索 入口 screenshot.
#
# `docs/f212_search_entry.png` — the player opened from a real 搜索 result, with
# the run's own `decode=mf` line drawn next to it (the decoder verdict is a log
# line, so the shot is the window plus the log evidence of the same run).
#
#   powershell -File scripts/qa-f211-shots.ps1
#
# `docs/f211_seek.png`   — the player window right after a seek lands (the
#                          timestamp readout is the target), with the one
#                          `seek trace` line of that seek drawn next to it.
# `docs/f211_avsync.png` — the same window with the performance panel open,
#                          with the sampled `delta_ms` statistics of the run.
#
# Both are screen captures of the real window (the picture is scan-out on the
# DComp path, so a screen read is the only honest one) plus a text block taken
# from the log the same run wrote — no number in the image is typed by hand.
param(
    [string]$Exe = "target/release/o11a1.exe",
    [string]$Bvid = "BV1BBnb6aEww",
    [int]$Rounds = 1,
    [int]$SeekTo = 90,
    [string]$OutDir = "docs",
    [string]$WorkDir = "target/qa/f212"
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

if (-not ("F211Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public class F211Win {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int cmd);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr hWnd, ref POINT point);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr hWnd);

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
$WorkDir = if ([System.IO.Path]::IsPathRooted($WorkDir)) { $WorkDir } else { Join-Path $root $WorkDir }
New-Item -ItemType Directory -Force -Path $OutDir, $WorkDir | Out-Null
$exePath = if ([System.IO.Path]::IsPathRooted($Exe)) { $Exe } else { Join-Path $root $Exe }
if (-not (Test-Path $exePath)) { throw "找不到 $exePath（先 cargo build --release）" }

$stageW = 1280
$stageH = 720

function Find-Player($proc, $timeoutMs) {
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    while ((Get-Date) -lt $deadline) {
        foreach ($h in [F211Win]::Windows([uint32]$proc.Id)) {
            if ([F211Win]::Class($h) -eq "O11A1DcompPlayer") { return $h }
        }
        Start-Sleep -Milliseconds 250
    }
    return [IntPtr]::Zero
}

function Place-TopMost($hwnd) {
    $outer = New-Object F211Win+RECT
    [void][F211Win]::GetWindowRect($hwnd, [ref]$outer)
    $deltaW = ($outer.Right - $outer.Left) - [F211Win]::ClientWidth($hwnd)
    $deltaH = ($outer.Bottom - $outer.Top) - [F211Win]::ClientHeight($hwnd)
    [void][F211Win]::ShowWindow($hwnd, 9)
    [void][F211Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, ($stageW + $deltaW), ($stageH + $deltaH), 0x0010 -bor 0x0040)
    [void][F211Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0010)
    [void][F211Win]::SetForegroundWindow($hwnd)
}

# Capture the client area, then write the log block over it.
function Shot($hwnd, $path, $lines, $title) {
    [void][F211Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0010)
    [void][F211Win]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 700
    if (-not [F211Win]::IsWindow($hwnd) -or [F211Win]::Class($hwnd) -ne "O11A1DcompPlayer") {
        throw "capture aborted: $path would have been the desktop, not the player"
    }
    $origin = New-Object F211Win+POINT
    [void][F211Win]::ClientToScreen($hwnd, [ref]$origin)
    # The control bar hides itself: park the pointer over the picture so the
    # timestamp row is on screen when the capture is taken.
    # Two moves 12 px apart: a single SetCursorPos can land where the pointer
    # already is, and the player's bar opens on a *move*.
    [void][F211Win]::SetCursorPos($origin.X + [int]($stageW / 2), $origin.Y + $stageH - 34)
    Start-Sleep -Milliseconds 120
    [void][F211Win]::SetCursorPos($origin.X + [int]($stageW / 2) + 12, $origin.Y + $stageH - 30)
    # The bar's own appearance recomposites the window, and a screen read in
    # that first frame catches a half-flipped video layer: wait for the
    # composite to settle, then read.
    Start-Sleep -Milliseconds 1200
    $bitmap = New-Object System.Drawing.Bitmap $stageW, $stageH
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.CopyFromScreen($origin.X, $origin.Y, 0, 0, $bitmap.Size)
    $graphics.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::ClearTypeGridFit

    $font = New-Object System.Drawing.Font("Consolas", 13, [System.Drawing.FontStyle]::Regular)
    $bold = New-Object System.Drawing.Font("Consolas", 15, [System.Drawing.FontStyle]::Bold)
    $pad = 14
    $lineH = 21
    $boxH = $pad * 2 + $lineH * ($lines.Count + 1)
    $boxW = $stageW - 40
    $boxX = 20
    # The box sits at the top: the control bar (with the timestamp) stays
    # visible at the bottom, and the bar is part of the evidence.
    $boxY = 18
    $back = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(205, 8, 12, 18))
    $edge = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(120, 120, 200, 255)), 1
    $graphics.FillRectangle($back, $boxX, $boxY, $boxW, $boxH)
    $graphics.DrawRectangle($edge, $boxX, $boxY, $boxW, $boxH)
    $white = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(235, 240, 248))
    $cyan = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(150, 225, 255))
    $graphics.DrawString($title, $bold, $cyan, $boxX + $pad, $boxY + $pad - 2)
    $y = $boxY + $pad + $lineH
    foreach ($line in $lines) {
        $text = if ($line.Length -gt 150) { $line.Substring(0, 150) } else { $line }
        $graphics.DrawString($text, $font, $white, $boxX + $pad, $y)
        $y += $lineH
    }

    $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $graphics.Dispose()
    $bitmap.Dispose()
    Write-Host ("shot {0}" -f $path)
}

function Run-App($log, $env) {
    Get-Process o11a1 -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 400
    Remove-Item $log -Force -ErrorAction SilentlyContinue
    $env:BILI_DEBUG_LOG = $log
    $env:BILI_OPEN_BVID = $Bvid
    $env:BILI_QUALITY_PICK = "0,6000"
    $env:BILI_PLAYER_DCOMP = "1"
    $env:BILI_AVSYNC_TEST = "shots"
    Remove-Item Env:BILI_DECODE -ErrorAction SilentlyContinue
    return Start-Process -FilePath $exePath -WorkingDirectory $root -PassThru
}

function Stop-App($proc) {
    if ($proc -and -not $proc.HasExited) {
        $proc.Kill()
        $proc.WaitForExit(8000) | Out-Null
    }
}


# ---- the 搜索 入口 shot ----------------------------------------------------
$log = Join-Path $WorkDir "f212_shot_search.log"
Get-Process o11a1 -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 400
Remove-Item $log -Force -ErrorAction SilentlyContinue
$env:BILI_DEBUG_LOG = $log
$env:BILI_ENTRY_CYCLE = "search,$Rounds,9000,0"
Clear-Variable -Name lines -ErrorAction SilentlyContinue
Remove-Item Env:BILI_OPEN_BVID -ErrorAction SilentlyContinue
Remove-Item Env:BILI_DECODE -ErrorAction SilentlyContinue
Remove-Item Env:BILI_PLAYER_DCOMP -ErrorAction SilentlyContinue
Remove-Item Env:BILI_F212_HOLD -ErrorAction SilentlyContinue
$proc = $null
try {
    $proc = Start-Process -FilePath $exePath -WorkingDirectory $root -PassThru
    $hwnd = Find-Player $proc 90000
    if ($hwnd -eq [IntPtr]::Zero) { throw "90 秒内没有播放器窗口" }
    Start-Sleep -Milliseconds 1500
    Place-TopMost $hwnd
    # Wait for the decoder verdict of THIS session, then capture on the frame.
    $deadline = (Get-Date).AddSeconds(60)
    while ((Get-Date) -lt $deadline) {
        if ((Test-Path $log) -and (Select-String -Path $log -Pattern "decode=mf |decode fallback" -Quiet)) { break }
        Start-Sleep -Milliseconds 200
    }
    $fresh = Find-Player $proc 3000
    if ($fresh -ne [IntPtr]::Zero) { $hwnd = $fresh }
    Place-TopMost $hwnd
    Start-Sleep -Milliseconds 800
    $decode = (Select-String -Path $log -Pattern "decode=mf |decode fallback" | Select-Object -Last 1).Line
    $cycle = (Select-String -Path $log -Pattern "f212 cycle start" | Select-Object -Last 1).Line
    $open = (Select-String -Path $log -Pattern "player open mode=" | Select-Object -Last 1).Line
    $fallbacks = @(Select-String -Path $log -Pattern "no_dcomp").Count
    if (-not $decode) { throw "capture aborted: the run logged no decoder verdict" }
    $lines = @(
        $cycle,
        $open,
        $decode,
        "no_dcomp lines in this run: $fallbacks   (BILI_ENTRY_CYCLE=search, default config)"
    )
    Shot $hwnd (Join-Path $OutDir "f212_search_entry.png") $lines "f212 搜索入口 (search -> first result -> player)"
} finally {
    if ($proc -and -not $proc.HasExited) {
        $proc.Kill()
        $proc.WaitForExit(8000) | Out-Null
    }
    Remove-Item Env:BILI_ENTRY_CYCLE -ErrorAction SilentlyContinue
}
