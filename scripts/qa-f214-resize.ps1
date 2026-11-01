# QA helper for f214: the video layer follows the window, and the panel opens
# inward.
#
#   powershell -File scripts/qa-f214-resize.ps1                       # all phases
#   powershell -File scripts/qa-f214-resize.ps1 -Phase resize          # 800x600 / 1280x720 / 1920x1080
#   powershell -File scripts/qa-f214-resize.ps1 -Phase fullscreen      # fullscreen + right panel
#   powershell -File scripts/qa-f214-resize.ps1 -Phase frames -Seconds 40
#
# No path switch is set: the composite picture path is the only path since f214
# (the escape valve `BILI_PLAYER_DCOMP=0` is *removed* here on purpose, so this
# script measures what a user gets).
#
# What each phase proves:
#
#   resize   one player window, client set to exactly 800x600, 1280x720 and
#            1920x1080. After each `SetWindowPos` the last `dcomp video geom`
#            line must read `overflow=0.0` and its `video_rect` must equal the
#            16:9 contain-fit of the client within 1px (the picture scales with
#            the window, it is not redrawn at a different quality). Captures go
#            to docs/f214_resize_<W>x<H>.png and one montage to
#            docs/f214_resize_3sizes.png.
#
#   fullscreen  the right panel is opened through `BILI_PANEL_TEST`, the window
#            goes fullscreen through the UI's own callback (`f` → WM_CHAR), and
#            the panel must sit flush with the window's right edge: the log line
#            `panel ... overlay=true strip=360 panel_x=<w-360> right_edge=<w>
#            inside=1`, plus a band that gave up exactly the strip's width.
#            Capture: docs/f214_fullscreen_panel.png.
#
#   frames   the f200 sampler for `-Seconds`, 4x interpolation + the DComp
#            comment layer at 1280x720: `video_s` >= 115 and `dm_s` >= 55 on the
#            same binary that was just resized, i.e. the resize path did not
#            cost the picture or the comments a frame.
param(
    [ValidateSet("all", "resize", "fullscreen", "dpi", "frames", "menu")]
    [string]$Phase = "all",
    [ValidateSet("720", "1080")]
    [string]$Stage = "1080",
    [string]$Exe = "target/release/o11a1.exe",
    [string]$Sizes = "800x600,1280x720,1920x1080",
    [int]$SettleMs = 1400,
    [int]$Seconds = 40,
    [double]$MinVideo = 115.0,
    [double]$MinDanmaku = 55.0,
    [string]$Bvid = "BV1BBnb6aEww",
    [string]$OutDir = "target/qa/f214",
    [string]$ShotDir = "docs",
    # Keep the QA window(s) open for a look (and skip the final kill).
    [switch]$KeepOpen
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

if (-not ("F214Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public class F214Win {
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
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hWnd, uint msg, IntPtr w, IntPtr l);

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
    public static void Key(IntPtr h, char c) {
        PostMessage(h, 0x0100, (IntPtr)(int)c, IntPtr.Zero);   // WM_KEYDOWN
        PostMessage(h, 0x0102, (IntPtr)(int)c, IntPtr.Zero);   // WM_CHAR
        PostMessage(h, 0x0101, (IntPtr)(int)c, IntPtr.Zero);   // WM_KEYUP
    }
    // A DPI change without a second monitor: the message the OS would send,
    // with a null suggested rect (so nothing is resized) and 144 dpi (150 %).
    public static void Dpi(IntPtr h, int dpi) {
        PostMessage(h, 0x02E0, (IntPtr)((dpi << 16) | (dpi & 0xFFFF)), IntPtr.Zero);
    }
}
"@
}

$root = Split-Path -Parent $PSScriptRoot
$OutDir = if ([System.IO.Path]::IsPathRooted($OutDir)) { $OutDir } else { Join-Path $root $OutDir }
$ShotDir = if ([System.IO.Path]::IsPathRooted($ShotDir)) { $ShotDir } else { Join-Path $root $ShotDir }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
New-Item -ItemType Directory -Force -Path $ShotDir | Out-Null

$stageW = if ($Stage -eq "1080") { 1920 } else { 1280 }
$stageH = if ($Stage -eq "1080") { 1080 } else { 720 }
$sample = Join-Path $root ("target/f209/sample{0}p30.mp4" -f $Stage)
if (-not (Test-Path $sample)) { throw "缺样本 $sample（先跑 scripts/qa-f212-accept.ps1 生成）" }

$exePath = if ([System.IO.Path]::IsPathRooted($Exe)) { $Exe } else { Join-Path $root $Exe }
if (-not (Test-Path $exePath)) { throw "找不到 $exePath（先 cargo build --release）" }
$imageName = [System.IO.Path]::GetFileNameWithoutExtension($exePath)

$failed = New-Object System.Collections.Generic.List[string]
$checks = New-Object System.Collections.Generic.List[object]

function Note($name, $ok, $detail) {
    $checks.Add([pscustomobject]@{ check = $name; ok = [bool]$ok; detail = $detail })
    if (-not $ok) { $failed.Add($name) }
    $flag = if ($ok) { "PASS" } else { "FAIL" }
    Write-Host ("  [{0}] {1}  {2}" -f $flag, $name, $detail)
}

function Start-Player([string]$log, [hashtable]$extraEnv) {
    Get-Process $imageName -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 400
    if (Test-Path $log) { Remove-Item $log -Force }

    $env:BILI_DEBUG_LOG = $log
    $env:BILI_OPEN_BVID = $Bvid
    $env:BILI_QUALITY_PICK = "0,6000"
    $env:BILI_QA_MF_FILE = $sample
    # f214: the shipped default, with no path switch at all.
    Remove-Item Env:BILI_PLAYER_DCOMP -ErrorAction SilentlyContinue
    Remove-Item Env:BILI_DECODE -ErrorAction SilentlyContinue
    Remove-Item Env:BILI_FULLSCREEN_TEST -ErrorAction SilentlyContinue
    Remove-Item Env:BILI_PANEL_TEST -ErrorAction SilentlyContinue
    Remove-Item Env:BILI_F200_LAYERS -ErrorAction SilentlyContinue
    Remove-Item Env:BILI_INTERP_TEST -ErrorAction SilentlyContinue
    Remove-Item Env:BILI_INTERP_4X -ErrorAction SilentlyContinue
    $env:BILI_DCOMP_DANMAKU = "1"
    foreach ($key in $extraEnv.Keys) { Set-Item -Path ("Env:" + $key) -Value $extraEnv[$key] }

    $proc = Start-Process -FilePath $exePath -WorkingDirectory $root -PassThru
    $hwnd = [IntPtr]::Zero
    $deadline = (Get-Date).AddMilliseconds(70000)
    while ((Get-Date) -lt $deadline) {
        if ($proc.HasExited) { break }
        foreach ($h in [F214Win]::Windows([uint32]$proc.Id)) {
            if ([F214Win]::Class($h) -eq "O11A1DcompPlayer") { $hwnd = $h; break }
        }
        if ($hwnd -ne [IntPtr]::Zero) { break }
        Start-Sleep -Milliseconds 300
    }
    if ($hwnd -eq [IntPtr]::Zero) {
        throw ("70 秒内没有 DComp 播放器窗口（进程退出={0}）" -f $proc.HasExited)
    }
    Write-Host ("  player hwnd={0} log={1}" -f $hwnd, $log)
    return @{ proc = $proc; hwnd = $hwnd; log = $log }
}

# Client-exact size, window parked at 0,0 and topmost: the capture is a screen
# read of a scan-out composition, so nothing may cover it.
function Set-Client([IntPtr]$hwnd, [int]$w, [int]$h) {
    $outer = New-Object F214Win+RECT
    [void][F214Win]::GetWindowRect($hwnd, [ref]$outer)
    $dw = ($outer.Right - $outer.Left) - [F214Win]::ClientWidth($hwnd)
    $dh = ($outer.Bottom - $outer.Top) - [F214Win]::ClientHeight($hwnd)
    for ($pass = 0; $pass -lt 12; $pass++) {
        [void][F214Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, ($w + $dw), ($h + $dh), 0x0010 -bor 0x0040)
        Start-Sleep -Milliseconds 120
        if ([F214Win]::ClientWidth($hwnd) -eq $w -and [F214Win]::ClientHeight($hwnd) -eq $h) { return $true }
    }
    return $false
}

function Capture-Client([IntPtr]$hwnd, [string]$path) {
    $w = [Math]::Max([F214Win]::ClientWidth($hwnd), 64)
    $h = [Math]::Max([F214Win]::ClientHeight($hwnd), 64)
    $bitmap = New-Object System.Drawing.Bitmap $w, $h
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $origin = New-Object F214Win+POINT
    [void][F214Win]::ClientToScreen($hwnd, [ref]$origin)
    $graphics.CopyFromScreen($origin.X, $origin.Y, 0, 0, $bitmap.Size)
    $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $graphics.Dispose()
    $bitmap.Dispose()
    Write-Host ("  capture {0} {1}x{2}" -f $path, $w, $h)
    return @{ w = $w; h = $h }
}

function Measure-Luma([string]$path) {
    $bitmap = [System.Drawing.Bitmap]::FromFile($path)
    $sum = 0.0
    $count = 0
    $stepX = [Math]::Max([int]($bitmap.Width / 40), 1)
    $stepY = [Math]::Max([int]($bitmap.Height / 40), 1)
    for ($y = 0; $y -lt $bitmap.Height; $y += $stepY) {
        for ($x = 0; $x -lt $bitmap.Width; $x += $stepX) {
            $p = $bitmap.GetPixel($x, $y)
            $sum += (0.2126 * $p.R + 0.7152 * $p.G + 0.0722 * $p.B)
            $count += 1
        }
    }
    $bitmap.Dispose()
    if ($count -eq 0) { return 0.0 }
    return $sum / $count
}

function Last-Geom([string]$log) {
    if (-not (Test-Path $log)) { return $null }
    $line = Get-Content $log | Where-Object { $_ -match "dcomp video geom" } | Select-Object -Last 1
    if (-not $line) { return $null }
    $m = [regex]::Match($line,
        'dcomp video geom window=(?<ow>\d+)x(?<oh>\d+) client=(?<cw>\d+)x(?<ch>\d+) .*?scale=(?<scale>[\d.]+) fit=(?<fit>\d+) frame=(?<fw>\d+)x(?<fh>\d+) band=(?<bw>[\d.]+)x(?<bh>[\d.]+)@\((?<bx>[\d.]+),(?<by>[\d.]+)\) video_rect=(?<vw>[\d.]+)x(?<vh>[\d.]+)@\((?<vx>[\d.]+),(?<vy>[\d.]+)\).*?overflow=(?<ov>[\d.]+)')
    if (-not $m.Success) { return $null }
    $values = @{}
    foreach ($name in @("ow", "oh", "cw", "ch", "fit", "fw", "fh", "bw", "bh", "bx", "by", "vw", "vh", "vx", "vy", "ov", "scale")) {
        $values[$name] = [double]$m.Groups[$name].Value
    }
    $values["line"] = $line
    return $values
}

# The 16:9 (frame aspect) contain-fit of a client rect, in physical px.
function Expect-Contain($client, $frame) {
    $sx = $client[0] / $frame[0]
    $sy = $client[1] / $frame[1]
    $s = [Math]::Min($sx, $sy)
    $w = $frame[0] * $s
    $h = $frame[1] * $s
    return @{ w = $w; h = $h; x = ($client[0] - $w) / 2.0; y = ($client[1] - $h) / 2.0 }
}

function Assert-Geom([string]$tag, $client, $frame, $geom, [double]$insetRight = 0.0, [double]$scale = 1.0) {
    if ($null -eq $geom) { Note "$tag geom" $false "no 'dcomp video geom' line"; return }
    Note "$tag overflow" ([Math]::Abs($geom.ov) -lt 0.05) ("overflow={0} client={1}x{2}" -f $geom.ov, $geom.cw, $geom.ch)
    $bandW = $geom.cw - ($insetRight * $scale)
    $band = @($bandW, $geom.ch)
    Note "$tag band" (([Math]::Abs($geom.bw - $band[0]) -le 1.0) -and ([Math]::Abs($geom.bh - $band[1]) -le 1.0)) `
        ("band={0}x{1} expect={2}x{3}" -f $geom.bw, $geom.bh, $band[0], $band[1])
    $want = Expect-Contain $band $frame
    $dx = [Math]::Abs($geom.vx - $want.x)
    $dy = [Math]::Abs($geom.vy - $want.y)
    $dw = [Math]::Abs($geom.vw - $want.w)
    $dh = [Math]::Abs($geom.vh - $want.h)
    $worst = [Math]::Max([Math]::Max($dx, $dy), [Math]::Max($dw, $dh))
    Note "$tag aspect" ($worst -le 1.0) `
        ("video_rect={0}x{1}@({2},{3}) expect={4:N1}x{5:N1}@({6:N1},{7:N1}) worst={8:N2}px" -f `
            $geom.vw, $geom.vh, $geom.vx, $geom.vy, $want.w, $want.h, $want.x, $want.y, $worst)
}

function Build-Montage([string[]]$paths, [string[]]$labels, [string]$target) {
    $height = 540
    $pad = 14
    $title = 26
    $images = @()
    foreach ($path in $paths) { $images += [System.Drawing.Image]::FromFile($path) }
    $widths = @()
    foreach ($image in $images) { $widths += [int][Math]::Round($image.Width * ($height / $image.Height)) }
    $totalW = [int](($widths | Measure-Object -Sum).Sum + $pad * ($images.Count + 1))
    $totalH = [int]($height + $title + $pad * 2)
    $canvas = New-Object System.Drawing.Bitmap -ArgumentList $totalW, $totalH
    $g = [System.Drawing.Graphics]::FromImage($canvas)
    $g.Clear([System.Drawing.Color]::FromArgb(16, 16, 20))
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $font = New-Object System.Drawing.Font("Consolas", 12)
    $brush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(230, 230, 235))
    $x = $pad
    for ($i = 0; $i -lt $images.Count; $i++) {
        $g.DrawImage($images[$i], $x, $pad, $widths[$i], $height)
        $g.DrawString($labels[$i], $font, $brush, [single]$x, [single]($pad + $height + 4))
        $x += $widths[$i] + $pad
    }
    $canvas.Save($target, [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose()
    $canvas.Dispose()
    foreach ($image in $images) { $image.Dispose() }
    Write-Host ("  montage {0}" -f $target)
}

function Read-F200([string]$log, [int]$skip) {    $lines = Get-Content $log | Where-Object { $_ -match "f200 layers" }
    $samples = @()
    foreach ($line in $lines) {
        $m = [regex]::Match($line, 'f200 layers t=(\d+)s .*?video_s=([\d.]+) dm_s=([\d.]+)')
        if ($m.Success) {
            $samples += [pscustomobject]@{
                t = [int]$m.Groups[1].Value
                video = [double]$m.Groups[2].Value
                danmaku = [double]$m.Groups[3].Value
            }
        }
    }
    return $samples | Where-Object { $_.t -gt $skip }
}

$wanted = @()
if ($Phase -eq "all" -or $Phase -eq "resize") { $wanted += "resize" }
if ($Phase -eq "all" -or $Phase -eq "fullscreen") { $wanted += "fullscreen" }
if ($Phase -eq "dpi") { $wanted += "dpi" }
if ($Phase -eq "all" -or $Phase -eq "menu") { $wanted += "menu" }
if ($Phase -eq "all" -or $Phase -eq "frames") { $wanted += "frames" }

# ---------------------------------------------------------------------------
# 1) resize sweep
# ---------------------------------------------------------------------------
if ($wanted -contains "resize") {
    Write-Host "`n=== resize sweep ($Sizes)"
    $log = Join-Path $OutDir "f214_resize.log"
    $run = Start-Player $log @{}
    try {
        $hwnd = $run.hwnd
        [void][F214Win]::ShowWindow($hwnd, 9)
        [void][F214Win]::SetForegroundWindow($hwnd)
        Start-Sleep -Milliseconds 2500
        $shots = @()
        $labels = @()
        foreach ($size in ($Sizes -split ",")) {
            $parts = $size.Trim().ToLower() -split "x"
            $w = [int]$parts[0]
            $h = [int]$parts[1]
            if (-not (Set-Client $hwnd $w $h)) {
                Note ("{0}x{1} client" -f $w, $h) $false "SetWindowPos did not reach the requested client size"
                continue
            }
            Start-Sleep -Milliseconds $SettleMs
            $geom = Last-Geom $run.log
            $client = @([F214Win]::ClientWidth($hwnd), [F214Win]::ClientHeight($hwnd))
            $frame = if ($null -ne $geom) { @($geom.fw, $geom.fh) } else { @($stageW, $stageH) }
            Assert-Geom ("{0}x{1}" -f $w, $h) $client $frame $geom 0.0 1.0
            $shot = Join-Path $ShotDir ("f214_resize_{0}x{1}.png" -f $w, $h)
            $cap = Capture-Client $hwnd $shot
            $shots += $shot
            $labels += ("{0}x{1}" -f $w, $h)
            if ($null -ne $geom) { Write-Host ("  geom {0}" -f $geom.line) }
        }
        # Every WM_SIZE must have produced a geom line with overflow=0; the
        # count is the evidence that the resize was handled in the message
        # handler, not one tick later.
        $all = Get-Content $run.log | Where-Object { $_ -match "dcomp video geom" }
        $bad = @($all | Where-Object { $_ -notmatch "overflow=0\.0\s*$" -and [double]([regex]::Match($_, "overflow=([\d.]+)").Groups[1].Value) -gt 0.05 })
        Note "geom lines" ($all.Count -gt 0) ("lines={0} nonzero-overflow={1}" -f $all.Count, $bad.Count)

        # A drag is a burst of `WM_SIZE`, not three tidy steps: 16 resizes at
        # 60 ms, then the picture must still be there (not black, not the old
        # slot) and every geometry line must still have overflow=0.
        $burst = @(@(1000, 620), @(760, 560), @(1400, 800), @(900, 700))
        $before = (Get-Content $run.log | Where-Object { $_ -match "dcomp video geom" }).Count
        for ($i = 0; $i -lt 16; $i++) {
            $target = $burst[$i % $burst.Count]
            [void](Set-Client $hwnd $target[0] $target[1])
            Start-Sleep -Milliseconds 60
        }
        [void](Set-Client $hwnd 1280 720)
        Start-Sleep -Milliseconds $SettleMs
        $after = (Get-Content $run.log | Where-Object { $_ -match "dcomp video geom" }).Count
        $stressShot = Join-Path $OutDir "f214_resize_burst.png"
        [void](Capture-Client $hwnd $stressShot)
        $luma = Measure-Luma $stressShot
        $geom = Last-Geom $run.log
        Note "burst resizes" (($after - $before) -ge 8) ("geom lines during burst={0}" -f ($after - $before))
        Note "burst picture" ($luma -gt 20.0) ("mean luma={0:N1} (not black)" -f $luma)
        Assert-Geom "burst" @([F214Win]::ClientWidth($hwnd), [F214Win]::ClientHeight($hwnd)) @($geom.fw, $geom.fh) $geom 0.0 1.0
        if ($null -ne $geom) { Write-Host ("  geom {0}" -f $geom.line) }

        if ($shots.Count -ge 2) {
            Build-Montage $shots $labels (Join-Path $ShotDir "f214_resize_3sizes.png")
        }
    } finally {
        if (-not $KeepOpen -and -not $run.proc.HasExited) {
            $run.proc.Kill()
            $run.proc.WaitForExit(5000) | Out-Null
        }
    }
}

# ---------------------------------------------------------------------------
# 1b) the default-on shot: no path switch, 画质增强 menu open
# ---------------------------------------------------------------------------
if ($wanted -contains "menu") {
    Write-Host "`n=== default start (no path env) + 画质增强 menu"
    $log = Join-Path $OutDir "f214_default_on.log"
    # No `BILI_PLAYER_DCOMP`, no `BILI_DECODE`: exactly what a user gets. The
    # menu hook only opens the player's own 设置 tab and the 画质增强 list, so
    # the capture shows the tier rows and that there is no 画面直出 switch left.
    $run = Start-Player $log @{ BILI_F204_MENU = "5000,3" }
    try {
        $hwnd = $run.hwnd
        [void][F214Win]::ShowWindow($hwnd, 9)
        [void][F214Win]::SetForegroundWindow($hwnd)
        Start-Sleep -Milliseconds 8000
        $shot = Join-Path $ShotDir "f214_default_on.png"
        [void](Capture-Client $hwnd $shot)
        $path = Get-Content $run.log | Where-Object { $_ -match "dcomp platform installed" } | Select-Object -First 1
        Note "default player_path" ($path -match "player_path=dcomp") $path.Trim()
        $menu = Get-Content $run.log | Where-Object { $_ -match "f204 menu open" } | Select-Object -First 1
        Note "enhance menu open" ($null -ne $menu) ("{0}" -f $menu)
        $restart = Get-Content $run.log | Where-Object { $_ -match "restart required|重启后生效|画面直出：" }
        Note "default no restart wording" (-not $restart) ("hits={0}" -f @($restart).Count)
    } finally {
        if (-not $KeepOpen -and -not $run.proc.HasExited) {
            $run.proc.Kill()
            $run.proc.WaitForExit(5000) | Out-Null
        }
    }
}

# ---------------------------------------------------------------------------
# 2) fullscreen + right panel
# ---------------------------------------------------------------------------
if ($wanted -contains "fullscreen") {
    Write-Host "`n=== fullscreen + right panel"
    $log = Join-Path $OutDir "f214_fullscreen.log"
    # The panel opens 1.5 s after the player does and never closes (the close
    # step is 999 s out), so the capture cannot race a close step.
    $run = Start-Player $log @{ BILI_PANEL_TEST = "0,1,0,999000" }
    try {
        $hwnd = $run.hwnd
        [void][F214Win]::ShowWindow($hwnd, 9)
        Start-Sleep -Milliseconds 2500
        # Windowed with the panel open: the window grew by the strip, the band
        # is the old picture width.
        $scale = 1.0
        $geom = Last-Geom $run.log
        if ($null -ne $geom) { $scale = $geom.scale }
        $strip = [Math]::Round(360.0 * $scale)
        $client = @([F214Win]::ClientWidth($hwnd), [F214Win]::ClientHeight($hwnd))
        Write-Host ("  windowed+panel client={0}x{1} strip={2}" -f $client[0], $client[1], $strip)
        Assert-Geom "windowed-panel" $client @($geom.fw, $geom.fh) $geom 360.0 $scale

        # Fullscreen through the UI's own key binding (`f`).
        [void][F214Win]::SetForegroundWindow($hwnd)
        [F214Win]::Key($hwnd, [char]'f')
        Start-Sleep -Milliseconds 2500
        $client = @([F214Win]::ClientWidth($hwnd), [F214Win]::ClientHeight($hwnd))
        $geom = Last-Geom $run.log
        Assert-Geom "fullscreen-panel" $client @($geom.fw, $geom.fh) $geom 360.0 $scale

        $panelLine = Get-Content $run.log | Where-Object { $_ -match "panel f214 " } | Select-Object -Last 1
        if ($panelLine) {
            $inside = [regex]::Match($panelLine, "inside=(\d)")
            $panelX = [regex]::Match($panelLine, "panel_x=(\d+)")
            $rightEdge = [regex]::Match($panelLine, "right_edge=(\d+)")
            $winW = [regex]::Match($panelLine, "win=(\d+)x")
            $strip = [regex]::Match($panelLine, "strip=(\d+)")
            $full = [regex]::Match($panelLine, "fullscreen=(\d)")
            $ok = $inside.Success -and $inside.Groups[1].Value -eq "1" -and
                $full.Groups[1].Value -eq "1" -and $strip.Groups[1].Value -ne "0"
            $detail = if ($ok) {
                ("fullscreen=1 win_w={0} strip={1} panel_x={2} right_edge={3} (x+width<=window.width)" -f `
                    $winW.Groups[1].Value, $strip.Groups[1].Value, $panelX.Groups[1].Value, $rightEdge.Groups[1].Value)
            } else { $panelLine }
            Note "fullscreen panel inside" $ok $detail
        } else {
            Note "fullscreen panel inside" $false "no 'panel f214' line"
        }

        $shot = Join-Path $ShotDir "f214_fullscreen_panel.png"
        [void](Capture-Client $hwnd $shot)

        # Back to windowed: the band must grow back to the windowed slot.
        [F214Win]::Key($hwnd, [char]'f')
        Start-Sleep -Milliseconds 1800
        $client = @([F214Win]::ClientWidth($hwnd), [F214Win]::ClientHeight($hwnd))
        $geom = Last-Geom $run.log
        Assert-Geom "back-windowed" $client @($geom.fw, $geom.fh) $geom 360.0 $scale
    } finally {
        if (-not $KeepOpen -and -not $run.proc.HasExited) {
            $run.proc.Kill()
            $run.proc.WaitForExit(5000) | Out-Null
        }
    }
}

# ---------------------------------------------------------------------------
# 2b) DPI change (150 %): the insets are logical, the band is physical
# ---------------------------------------------------------------------------
if ($wanted -contains "dpi") {
    Write-Host "`n=== dpi 150% with the right panel open"
    $log = Join-Path $OutDir "f214_dpi.log"
    $run = Start-Player $log @{ BILI_PANEL_TEST = "0,1,0,999000"; BILI_F214_DPI = "144,2500" }
    try {
        $hwnd = $run.hwnd
        [void][F214Win]::ShowWindow($hwnd, 9)
        Start-Sleep -Milliseconds 4500
        # The strip is 360 logical, i.e. 540 physical at 150 %. The DPI change is
        # posted by the app itself (`BILI_F214_DPI`): a cross-process
        # `PostMessage(WM_DPICHANGED)` is refused by UIPI (measured FALSE). The
        # window keeps its size, so the geometry must be rebuilt at the new
        # factor: band = client - strip*1.5, aspect unchanged, nothing outside.
        $geom = Last-Geom $run.log
        $client = @([F214Win]::ClientWidth($hwnd), [F214Win]::ClientHeight($hwnd))
        if ($null -eq $geom) {
            Note "dpi 150%" $false "no geom line"
        } else {
            $scale = $geom.scale
            Note "dpi scale" ([Math]::Abs($scale - 1.5) -lt 0.01) ("scale={0}" -f $scale)
            Assert-Geom "dpi-150" $client @($geom.fw, $geom.fh) $geom 360.0 $scale
        }
    } finally {
        if (-not $KeepOpen -and -not $run.proc.HasExited) {
            $run.proc.Kill()
            $run.proc.WaitForExit(5000) | Out-Null
        }
    }
}

# ---------------------------------------------------------------------------
# 3) frame-rate regression on the same binary
# ---------------------------------------------------------------------------
if ($wanted -contains "frames") {
    Write-Host ("`n=== frames {0}s (4x interpolation, DComp comments)" -f $Seconds)
    $log = Join-Path $OutDir "f214_frames.log"
    $run = Start-Player $log @{
        BILI_INTERP_TEST = "3"
        BILI_INTERP_4X = "1"
        BILI_F200_LAYERS = "$Seconds"
        BILI_DANMAKU_STRIPS = "1"
    }
    try {
        $hwnd = $run.hwnd
        [void][F214Win]::ShowWindow($hwnd, 9)
        [void][F214Win]::SetForegroundWindow($hwnd)
        [void](Set-Client $hwnd 1280 720)
        Start-Sleep -Seconds ($Seconds + 2)
        # t <= 4 is the decoder/pacer warm-up (the f213 baseline dips to 0/40/80
        # at t=1/2/3 too); the steady state is what the thresholds describe.
        $samples = Read-F200 $run.log 4
        if (-not $samples -or $samples.Count -eq 0) {
            Note "frames" $false "no 'f200 layers' samples"
        } else {
            $video = $samples | Measure-Object -Property video -Average -Minimum -Maximum
            $dm = $samples | Measure-Object -Property danmaku -Average -Minimum -Maximum
            $videoAvg = [Math]::Round($video.Average, 1)
            $dmAvg = [Math]::Round($dm.Average, 1)
            Write-Host ("  video_s avg={0} min={1} max={2}  dm_s avg={3} min={4} max={5}  samples={6}" -f `
                $videoAvg, $video.Minimum, $video.Maximum, $dmAvg, $dm.Minimum, $dm.Maximum, $samples.Count)
            Note "video_s" ($videoAvg -ge $MinVideo -and $video.Minimum -ge $MinVideo) ("avg={0} min={1} (>= {2} after warm-up)" -f $videoAvg, $video.Minimum, $MinVideo)
            Note "dm_s" ($dmAvg -ge $MinDanmaku) ("avg={0} min={1} (>= {2})" -f $dmAvg, $dm.Minimum, $MinDanmaku)
            # The decode stays at the source size whatever the window is: the
            # window scales the picture, it never re-decodes it.
            $decode = Get-Content $run.log | Where-Object { $_ -match "decode=" } | Select-Object -First 1
            if ($decode) { Note "decode" $true ("{0}" -f $decode.Trim()) }
        }
        $restart = Get-Content $run.log | Where-Object { $_ -match "restart required|重启后生效|画面直出：" }
        Note "no restart wording" (-not $restart) ("hits={0}" -f @($restart).Count)
        $path = Get-Content $run.log | Where-Object { $_ -match "dcomp platform installed" } | Select-Object -First 1
        if ($path) {
            Write-Host ("  {0}" -f $path.Trim())
            Note "player_path" ($path -match "player_path=dcomp") $path.Trim()
        }
    } finally {
        if (-not $KeepOpen -and -not $run.proc.HasExited) {
            $run.proc.Kill()
            $run.proc.WaitForExit(5000) | Out-Null
        }
    }
}

Write-Host ""
Write-Host "==== f214 checks ===="
$checks | Format-Table -AutoSize | Out-String | Write-Host
if ($failed.Count -gt 0) {
    Write-Host ("FAILED: {0}" -f ($failed -join ", "))
    exit 1
}
Write-Host "all f214 checks passed"
exit 0
