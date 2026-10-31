# QA helper for f207: the danmaku layer's strip cache, A/B against the Slint
# layer it replaces.
#
#   powershell -File scripts/qa-f207.ps1 -Stage 1080 -Strips 1   # strips (opt-in)
#   powershell -File scripts/qa-f207.ps1 -Stage 720  -Strips 0   # f196 Slint layer
#   powershell -File scripts/qa-f207.ps1 -All            # all four cells
#
# Every rate on the summary line is the f200 sampler's per-second series
# (`f200 layers t=… video_s=… dm_s=… ui_s=… dm_compose_us_p50=…`), i.e. presents
# counted by each layer's own thread, and the strip cache's own counters. The
# capture is the real scan-out (CopyFromScreen): the composite path has no Slint
# tree to snapshot.
param(
    [ValidateSet("720", "1080")]
    [string]$Stage = "1080",
    [ValidateSet("0", "1")]
    [string]$Strips = "0",
    [switch]$All,
    [string]$Exe = "target/release/o11a1.exe",
    [string]$Bvid = "BV1BBnb6aEww",
    [string]$OutDir = "target/qa",
    [string]$ShotDir = "docs",
    [int]$BootMs = 14000,
    [int]$Soak = 62
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

if (-not ("F207Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public class F207Win {
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

$root = Get-Location
$dir = Join-Path $root $OutDir
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
if (-not (Test-Path (Join-Path $root $ShotDir))) {
    New-Item -ItemType Directory -Path (Join-Path $root $ShotDir) -Force | Out-Null
}

$Script:Log = ""
function Get-PlayerWindow($proc, $timeoutMs) {
    # The player window is the one the app itself logged: `player hwnd=NNN` is
    # printed by the tick once the session is open, and unlike an EnumWindows
    # guess it is exactly the HWND the composite layers are bound to.
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    while ((Get-Date) -lt $deadline) {
        if (Test-Path $Script:Log) {
            $hit = Select-String -Path $Script:Log -SimpleMatch "player hwnd=" | Select-Object -Last 1
            if ($hit) {
                $m = [regex]::Match($hit.Line, "hwnd=(\d+)")
                if ($m.Success) {
                    $candidate = [IntPtr][int64]$m.Groups[1].Value
                    $cr = New-Object F207Win+RECT
                    if ([F207Win]::GetClientRect($candidate, [ref]$cr)) {
                        if (($cr.Right - $cr.Left) -gt 0) { return $candidate }
                    }
                }
            }
        }
        foreach ($h in [F207Win]::Windows([uint32]$proc.Id)) {
            $c = [F207Win]::Class($h)
            if ($c -eq "ConsoleWindowClass") { continue }
            if ($c -eq "O11A1DcompPlayer") {
                $cr = New-Object F207Win+RECT
                [void][F207Win]::GetClientRect($h, [ref]$cr)
                if (($cr.Right - $cr.Left) -gt 0) { return $h }
            }
        }
        Start-Sleep -Milliseconds 300
    }
    throw "no player window within the deadline"
}

function Set-ClientSize($hwnd, $w, $h) {
    for ($pass = 0; $pass -lt 8; $pass++) {
        for ($i = 0; $i -lt 3; $i++) {
            [void][F207Win]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, $w, $h, 0x0004 -bor 0x0010)
            Start-Sleep -Milliseconds 120
            $c = New-Object F207Win+RECT
            [void][F207Win]::GetClientRect($hwnd, [ref]$c)
            $dw = $w - ($c.Right - $c.Left); $dh = $h - ($c.Bottom - $c.Top)
            if ($dw -eq 0 -and $dh -eq 0) { break }
            $r = New-Object F207Win+RECT
            [void][F207Win]::GetWindowRect($hwnd, [ref]$r)
            [void][F207Win]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, ($r.Right - $r.Left) + $dw, ($r.Bottom - $r.Top) + $dh, 0x0004 -bor 0x0010)
        }
        $c = New-Object F207Win+RECT
        [void][F207Win]::GetClientRect($hwnd, [ref]$c)
        if (($c.Right - $c.Left) -eq $w -and ($c.Bottom - $c.Top) -eq $h) { break }
        Start-Sleep -Milliseconds 500
    }
    Start-Sleep -Milliseconds 1200
}

function Capture($hwnd, $name) {
    [void][F207Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0010)
    [void][F207Win]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 700
    $r = New-Object F207Win+RECT
    [void][F207Win]::GetWindowRect($hwnd, [ref]$r)
    $c = New-Object F207Win+RECT
    [void][F207Win]::GetClientRect($hwnd, [ref]$c)
    $w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
    Write-Host ("capture-target hwnd={0} rect={1},{2} {3}x{4} client={5}x{6}" -f $hwnd, $r.Left, $r.Top, $w, $h, ($c.Right - $c.Left), ($c.Bottom - $c.Top))
    if ($w -le 0 -or $h -le 0) { Write-Host "capture skipped: empty rect"; return }
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($r.Left, $r.Top, 0, 0, (New-Object System.Drawing.Size($w, $h)))
    $g.Dispose()
    $path = Join-Path (Join-Path $root $ShotDir) $name
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host ("capture {0} window={1}x{2} -> {3}" -f $name, $w, $h, $path)
}

# One run: play for Soak seconds at Stage's client size with Strips armed.
function Stop-App($proc) {
    # The launcher handle can be gone while the app it started is not, so the
    # kill is by image path: every earlier run's window must be closed before the
    # next one resizes "the" player window.
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    Get-Process o11a1 -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and ($_.Path -like "*target\release\o11a1.exe") } |
        ForEach-Object { Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue }
}

function Run-Cell($stage, $strips) {
    $log = Join-Path $dir ("f207_" + $stage + "_s" + $strips + ".log")
    if (Test-Path $log) { Remove-Item $log -Force }
    $Script:Log = $log
    $env:BILI_DEBUG_LOG = $log
    $env:BILI_OPEN_BVID = $Bvid
    $env:BILI_SOAK_TEST = "$Soak"
    $env:BILI_F200_LAYERS = "$Soak"
    $env:BILI_PLAYER_DCOMP = "1"
    $env:BILI_DCOMP_DANMAKU = "1"
    $env:BILI_DANMAKU_STRIPS = $strips
    # The highest tier the stream offers, so the 1080p cell really runs a 1080p
    # source: the frame size is logged as `dcomp video layer frame=…`.
    if (-not $env:BILI_QUALITY_PICK) { $env:BILI_QUALITY_PICK = "0,6000" }
    Remove-Item Env:BILI_DCOMP_UI_FORCE -ErrorAction SilentlyContinue
    Remove-Item Env:BILI_PANEL_TEST -ErrorAction SilentlyContinue

    $proc = Start-Process -FilePath (Join-Path $root $Exe) -PassThru
    try {
        # The session is open once the app logs its HWND; resizing then means the
        # following Soak seconds all run at the cell's size.
        $hwnd = Get-PlayerWindow $proc ($BootMs + 30000)
        Start-Sleep -Milliseconds 6000
        [void][F207Win]::SetForegroundWindow($hwnd)
        $rr = New-Object F207Win+RECT
        [void][F207Win]::GetWindowRect($hwnd, [ref]$rr)
        [void][F207Win]::SetCursorPos([int](($rr.Left + $rr.Right) / 2), [int](($rr.Top + $rr.Bottom) / 2))
        if ($stage -eq "1080") { Set-ClientSize $hwnd 1920 1080 } else { Set-ClientSize $hwnd 1280 720 }
        # The app applies its own persisted window size around the first frames;
        # a resize issued before that is silently reverted. Apply it again after
        # the layout has settled and verify the client rect really is ours.
        Start-Sleep -Milliseconds 6000
        if ($stage -eq "1080") { Set-ClientSize $hwnd 1920 1080 } else { Set-ClientSize $hwnd 1280 720 }
        $want = if ($stage -eq "1080") { 1920 } else { 1280 }
        $wantH = if ($stage -eq "1080") { 1080 } else { 720 }
        $c = New-Object F207Win+RECT
        [void][F207Win]::GetClientRect($hwnd, [ref]$c)
        Write-Host ("resized client={0}x{1} want={2}x{3}" -f ($c.Right - $c.Left), ($c.Bottom - $c.Top), $want, $wantH)
        Start-Sleep -Milliseconds 9000
        if ($strips -eq "1") {
            Capture $hwnd ("f207_" + $stage + "p_danmaku.png")
        } else {
            Capture $hwnd ("f207_" + $stage + "p_danmaku_sl.png")
        }
        Start-Sleep -Milliseconds 20000
        if ($strips -eq "1") {
            Capture $hwnd ("f207_" + $stage + "p_danmaku_b.png")
        }
    } finally {
        Start-Sleep -Milliseconds 1500
        Stop-App $proc
        Start-Sleep -Milliseconds 800
    }
    Write-Host "log: $log"
}

function Summarize($stage, $strips) {
    $log = Join-Path $dir ("f207_" + $stage + "_s" + $strips + ".log")
    if (-not (Test-Path $log)) { return }
    $lines = Select-String -Path $log -SimpleMatch "f200 layers" | ForEach-Object { $_.Line }
    if (-not $lines) { Write-Host "  (no f200 layers lines)"; return }
    $rows = @()
    foreach ($line in $lines) {
        if ($line -notmatch "dm_s=([\d.]+)") { continue }
        $dm = [double]$Matches[1]
        $video = if ($line -match "video_s=([\d.]+)") { [double]$Matches[1] } else { 0 }
        $ui = if ($line -match "ui_s=([\d.]+)") { [double]$Matches[1] } else { 0 }
        $p50 = if ($line -match "dm_compose_us_p50=(\d+)") { [int]$Matches[1] } else { 0 }
        $p95 = if ($line -match "dm_compose_us_p95=(\d+)") { [int]$Matches[1] } else { 0 }
        $paint = if ($line -match "dm_paint_ms=([\d.]+)") { [double]$Matches[1] } else { 0 }
        $quads = if ($line -match "dm_quads=(\d+)") { [int]$Matches[1] } else { 0 }
        $stripsN = if ($line -match "dm_strips=(\d+)") { [int]$Matches[1] } else { 0 }
        $atlas = if ($line -match "dm_atlas_mb=([\d.]+)") { [double]$Matches[1] } else { 0 }
        $masks = if ($line -match "dm_mask=(\d+)/(\d+)") { "$($Matches[1])/$($Matches[2])" } else { "-" }
        $on = if ($line -match "dm_strips_on=(\d+)") { [int]$Matches[1] } else { 0 }
        $rows += [pscustomobject]@{ dm = $dm; video = $video; ui = $ui; p50 = $p50; p95 = $p95; paint = $paint; quads = $quads; strips = $stripsN; atlas = $atlas; masks = $masks; on = $on }
    }
    $moving = $rows | Where-Object { $_.dm -gt 0 -and $_.video -gt 1 }
    if (-not $moving) { Write-Host "  (no moving samples)"; return }
    function Avg($values) {
        $list = @($values)
        if ($list.Count -eq 0) { return 0 }
        return [math]::Round((($list | Measure-Object -Average).Average), 1)
    }
    $videoAvg = Avg ($moving | ForEach-Object { $_.video })
    $dmAvg = Avg ($moving | ForEach-Object { $_.dm })
    $uiAvg = Avg ($moving | ForEach-Object { $_.ui })
    $p50avg = Avg ($moving | ForEach-Object { $_.p50 })
    $p95avg = Avg ($moving | ForEach-Object { $_.p95 })
    $paintAvg = Avg ($moving | ForEach-Object { $_.paint })
    $quadsAvg = Avg ($moving | ForEach-Object { $_.quads })
    $stripsMax = ($moving | ForEach-Object { $_.strips } | Measure-Object -Maximum).Maximum
    $atlasMax = ($moving | ForEach-Object { $_.atlas } | Measure-Object -Maximum).Maximum
    $last = $moving[-1]
    Write-Host ("f207 {0}p strips={1} samples={2} video_s={3} dm_s={4} ui_s={5} compose_us p50={6} p95={7} paint_ms={8} quads={9} masks={10} atlas_mb={11} strips_on={12}" -f `
            $stage, $strips, $moving.Count, $videoAvg, $dmAvg, $uiAvg, $p50avg, $p95avg, $paintAvg, $quadsAvg, $last.masks, $atlasMax, $last.on)
    Write-Host ("   max cached masks={0} max atlas_mb={1}" -f $stripsMax, $atlasMax)
}

$cells = @()
if ($All) {
    foreach ($s in @("1080", "720")) { foreach ($k in @("1", "0")) { $cells += ,@($s, $k) } }
} else {
    $cells += ,@($Stage, $Strips)
}

foreach ($cell in $cells) {
    Write-Host ("=== f207 run stage={0}p strips={1}" -f $cell[0], $cell[1])
    Run-Cell $cell[0] $cell[1]
}

Write-Host ""
Write-Host "=== f207 summary"
foreach ($cell in $cells) { Summarize $cell[0] $cell[1] }

Write-Host ""
Write-Host "=== strips cache/paint lines (strips=1 runs)"
foreach ($cell in $cells) {
    if ($cell[1] -ne "1") { continue }
    $log = Join-Path $dir ("f207_" + $cell[0] + "_s1.log")
    if (-not (Test-Path $log)) { continue }
    Write-Host ("--- {0}p" -f $cell[0])
    Select-String -Path $log -SimpleMatch "danmaku strips" | Select-Object -First 6 | ForEach-Object { $_.Line }
    Select-String -Path $log -SimpleMatch "danmaku paint" | Select-Object -Last 3 | ForEach-Object { $_.Line }
    Select-String -Path $log -SimpleMatch "dcomp danmaku layer created" | ForEach-Object { $_.Line }
    Select-String -Path $log -SimpleMatch "danmaku strips unavailable" | ForEach-Object { $_.Line }
    Select-String -Path $log -SimpleMatch "danmaku strips failed" | ForEach-Object { $_.Line }
}
