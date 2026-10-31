# QA helper for f213: which frame is transparent / which frame flickers.
#
#   powershell -File scripts/qa-f213-trace.ps1                       # 1080p, 插帧开 (mode 3, 4x)
#   powershell -File scripts/qa-f213-trace.ps1 -Interp 0             # 插帧关 (vp-copy only)
#   powershell -File scripts/qa-f213-trace.ps1 -Stage 720 -Interp 0
#
# What it does, all of it on the *shipped* binary with `BILI_F213_TRACE=1`:
#
# 1. before the app starts, it photographs the desktop under where the player
#    window will be. After the window appears it photographs the same screen
#    rect at 0 / 200 / 500 ms. If the load-phase pixels still equal the desktop
#    snapshot, the window is showing the desktop through itself - the exact
#    "加载时背景完全透明" report, measured instead of described;
# 2. once playback has settled it grabs 40 frames 30 ms apart and reports, per
#    frame, the mean luma of the centre of the picture plus the mean absolute
#    difference from the previous frame (A/B alternation shows up as a diff of
#    the same size every other frame);
# 3. it prints every `f213 slot` line, which names the drawing path of each
#    presented slot (warp / vp-copy / staging / nothing), the pair/phase, the
#    alpha range of the backbuffer, the present HRESULT and the swapchain's
#    BufferCount / AlphaMode.
param(
    [ValidateSet("720", "1080")]
    [string]$Stage = "1080",
    [ValidateSet("0", "3")]
    [string]$Interp = "3",
    [string]$Exe = "target/release/o11a1.exe",
    [int]$Seconds = 20,
    [int]$Frames = 40,
    [int]$Interval = 30,
    # Override the local sample (`BILI_QA_MF_FILE`). The ramp sample below is
    # what turns the frame table into a *frame-order* probe: every source frame
    # is one flat luma level, so the sequence of 40 numbers says whether the
    # picture advances or oscillates.
    [string]$SamplePath = "",
    [switch]$NoDanmaku,
    [string]$Bvid = "BV1BBnb6aEww",
    [string]$OutDir = "target/qa/f213"
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms

if (-not ("F213Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public class F213Win {
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
    [DllImport("user32.dll", SetLastError=true)] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);

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
$stageSample = Join-Path $root ("target/f209/sample{0}p30.mp4" -f $Stage)
$tag = "{0}_{1}p_interp{2}" -f $Stage, $Stage, $Interp
$Log = Join-Path $OutDir ("f213_{0}.log" -f $tag)
if (Test-Path $Log) { Remove-Item $Log -Force }

if (-not (Test-Path $stageSample)) {
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $stageSample) | Out-Null
    $size = "{0}x{1}" -f $clientW, $clientH
    Write-Host ("f213 sample generating {0} ({1})" -f $stageSample, $size)
    & ffmpeg -hide_banner -loglevel error -y -f lavfi -i "testsrc2=size=${size}:rate=30" -t 60 `
        -c:v libx264 -pix_fmt yuv420p -profile:v high -preset veryfast -crf 20 $stageSample
}

Get-Process o11a1 -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

function Shot-Screen([int]$x, [int]$y, [int]$w, [int]$h) {
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($x, $y, 0, 0, $bmp.Size)
    $g.Dispose()
    return $bmp
}

function Shot-PrintWindow([IntPtr]$hwnd, [int]$w, [int]$h) {
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    $ok = [F213Win]::PrintWindow($hwnd, $hdc, 2)
    $g.ReleaseHdc($hdc)
    $g.Dispose()
    if (-not $ok) { $bmp.Dispose(); return $null }
    return $bmp
}

# Centre of the picture, 50% of the client: chrome, the pill and the danmaku
# rows all live at the edges, the picture is what is left.
#
# Every measurement locks the bitmap once and reads a byte[] (GetPixel per
# pixel would take seconds per frame at 1080p).
function Pixels([System.Drawing.Bitmap]$bmp) {
    $rect = New-Object System.Drawing.Rectangle 0, 0, $bmp.Width, $bmp.Height
    $data = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $bytes = New-Object byte[] ($data.Stride * $bmp.Height)
    [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $bytes, 0, $bytes.Length)
    $stride = $data.Stride
    $bmp.UnlockBits($data)
    return @{ Bytes = $bytes; Stride = $stride; Width = $bmp.Width; Height = $bmp.Height }
}

function Stats($px) {
    $x = [int]($px.Width * 0.25)
    $y = [int]($px.Height * 0.25)
    $sw = [int]($px.Width * 0.5)
    $sh = [int]($px.Height * 0.5)
    $sum = 0.0; $n = 0
    for ($py = $y; $py -lt ($y + $sh); $py += 4) {
        $row = $py * $px.Stride
        for ($col = $x; $col -lt ($x + $sw); $col += 4) {
            $i = $row + $col * 4          # BGRA
            $sum += 0.114 * $px.Bytes[$i] + 0.587 * $px.Bytes[$i + 1] + 0.299 * $px.Bytes[$i + 2]
            $n++
        }
    }
    if ($n -eq 0) { return 0.0 }
    return $sum / $n
}

# Same sample grid, but the absolute difference between two frames: the number
# that says "the picture jumped" instead of "the picture is dark".
function FrameDiff($a, $b) {
    $x = [int]($a.Width * 0.25)
    $y = [int]($a.Height * 0.25)
    $sw = [int]($a.Width * 0.5)
    $sh = [int]($a.Height * 0.5)
    $sum = 0.0; $n = 0
    for ($py = $y; $py -lt ($y + $sh); $py += 4) {
        $row = $py * $a.Stride
        for ($col = $x; $col -lt ($x + $sw); $col += 4) {
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

# Fraction of pixels that are identical between two same-sized images: 1.0
# means "this is still the desktop underneath".
function Same($a, $b) {
    $n = 0; $same = 0
    for ($y = 0; $y -lt $a.Height; $y += 8) {
        $row = $y * $a.Stride
        for ($x = 0; $x -lt $a.Width; $x += 8) {
            $i = $row + $x * 4
            $n++
            if ($a.Bytes[$i] -eq $b.Bytes[$i] -and $a.Bytes[$i + 1] -eq $b.Bytes[$i + 1] -and $a.Bytes[$i + 2] -eq $b.Bytes[$i + 2]) { $same++ }
        }
    }
    if ($n -eq 0) { return 0.0 }
    return $same / $n
}

$screen = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$desktop = Shot-Screen 0 0 $screen.Width $screen.Height

$env:BILI_DEBUG_LOG = $Log
$env:BILI_OPEN_BVID = $Bvid
$env:BILI_QUALITY_PICK = "0,6000"
$env:BILI_PLAYER_DCOMP = "1"
if ($NoDanmaku) {
    $env:BILI_DCOMP_DANMAKU = "0"
    $env:BILI_DANMAKU_STRIPS = "0"
} else {
    $env:BILI_DCOMP_DANMAKU = "1"
    Remove-Item Env:BILI_DANMAKU_STRIPS -ErrorAction SilentlyContinue
}
$env:BILI_F213_TRACE = "1"
$env:BILI_F200_LAYERS = "$Seconds"
$env:BILI_QA_MF_FILE = $(if ($SamplePath -ne "") { $SamplePath } else { $stageSample })
$env:BILI_INTERP_TEST = $Interp
if ($Interp -eq "3") { $env:BILI_INTERP_4X = "1" } else { Remove-Item Env:BILI_INTERP_4X -ErrorAction SilentlyContinue }
foreach ($name in @("BILI_DECODE", "BILI_PANEL_TEST", "BILI_FULLSCREEN_TEST", "BILI_DCOMP_UI_FORCE", "BILI_WARP_TRACE", "BILI_PACE_TRACE", "BILI_SOAK_TEST")) {
    Remove-Item ("Env:" + $name) -ErrorAction SilentlyContinue
}

$exePath = if ([System.IO.Path]::IsPathRooted($Exe)) { $Exe } else { Join-Path $root $Exe }
if (-not (Test-Path $exePath)) { throw "找不到 $exePath（先 cargo build --release）" }
Write-Host ("f213 exe {0} stage={1} interp={2} log={3}" -f $exePath, $Stage, $Interp, $Log)

$proc = Start-Process -FilePath $exePath -WorkingDirectory $root -PassThru

function Find-Player($proc, $timeoutMs) {
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    while ((Get-Date) -lt $deadline) {
        foreach ($h in [F213Win]::Windows([uint32]$proc.Id)) {
            if ([F213Win]::Class($h) -eq "O11A1DcompPlayer") { return $h }
        }
        Start-Sleep -Milliseconds 60
    }
    return [IntPtr]::Zero
}

$load = @()
try {
    $hwnd = Find-Player $proc 70000
    if ($hwnd -eq [IntPtr]::Zero) { throw "70 秒内没有播放器窗口" }
    $appeared = Get-Date
    Write-Host ("f213 player hwnd={0}" -f $hwnd)

    # Load phase: 0 / 200 / 500 ms after the window exists, photograph the
    # screen where its client area is and compare it with the desktop snapshot
    # taken before the app started. The window is brought to the top first
    # (without resizing it, which would cost the moment being measured): a
    # screen read of a buried window would measure whatever is over it.
    [void][F213Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0010)
    [void][F213Win]::SetForegroundWindow($hwnd)
    # "0 ms" means "the window is the one on screen", not "the HWND exists":
    # a screen read before the raise lands on whatever was in front, which
    # measures the browser and says nothing about the player.
    for ($wait = 0; $wait -lt 40; $wait++) {
        if ([F213Win]::GetForegroundWindow() -eq $hwnd) { break }
        Start-Sleep -Milliseconds 50
        [void][F213Win]::SetForegroundWindow($hwnd)
    }
    $appeared = Get-Date
    foreach ($delay in @(0, 200, 500)) {
        $wait = $delay - ((Get-Date) - $appeared).TotalMilliseconds
        if ($wait -gt 0) { Start-Sleep -Milliseconds ([int]$wait) }
        $origin = New-Object F213Win+POINT
        [void][F213Win]::ClientToScreen($hwnd, [ref]$origin)
        $cw = [F213Win]::ClientWidth($hwnd)
        $ch = [F213Win]::ClientHeight($hwnd)
        if ($cw -lt 16 -or $ch -lt 16) { continue }
        $shot = Shot-Screen $origin.X $origin.Y $cw $ch
        $crop = New-Object System.Drawing.Bitmap $cw, $ch
        $cg = [System.Drawing.Graphics]::FromImage($crop)
        $cg.DrawImage($desktop, (New-Object System.Drawing.Rectangle 0, 0, $cw, $ch), (New-Object System.Drawing.Rectangle $origin.X, $origin.Y, $cw, $ch), [System.Drawing.GraphicsUnit]::Pixel)
        $cg.Dispose()
        $same = Same (Pixels $shot) (Pixels $crop)
        $luma = Stats (Pixels $shot)
        $path = Join-Path $OutDir ("f213_{0}_load{1}ms.png" -f $tag, $delay)
        $shot.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
        $load += [pscustomobject]@{ Ms = $delay; Desktop = $same; Luma = $luma; Path = $path }
        Write-Host ("f213 load {0}ms client={1}x{2} desktop_match={3:P0} luma={4:N1} -> {5}" -f $delay, $cw, $ch, $same, $luma, $path)
        $shot.Dispose(); $crop.Dispose()
    }

    # Settle: size the client exactly like the stage, put the window at 0,0 and
    # on top - the capture is a screen read, so anything covering it would be
    # what the frames show.
    $outer = New-Object F213Win+RECT
    [void][F213Win]::GetWindowRect($hwnd, [ref]$outer)
    $deltaW = ($outer.Right - $outer.Left) - [F213Win]::ClientWidth($hwnd)
    $deltaH = ($outer.Bottom - $outer.Top) - [F213Win]::ClientHeight($hwnd)
    for ($pass = 0; $pass -lt 10; $pass++) {
        [void][F213Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, ($clientW + $deltaW), ($clientH + $deltaH), 0x0010 -bor 0x0040)
        Start-Sleep -Milliseconds 250
        if ([F213Win]::ClientWidth($hwnd) -eq $clientW -and [F213Win]::ClientHeight($hwnd) -eq $clientH) { break }
    }
    [void][F213Win]::ShowWindow($hwnd, 9)
    [void][F213Win]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 1000

    Write-Host ("f213 playing {0}s ..." -f $Seconds)
    Start-Sleep -Seconds ($Seconds - 6)
    [void][F213Win]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 700

    $fresh = Find-Player $proc 3000
    if ($fresh -ne [IntPtr]::Zero) { $hwnd = $fresh }
    $origin = New-Object F213Win+POINT
    [void][F213Win]::ClientToScreen($hwnd, [ref]$origin)
    $cw = [F213Win]::ClientWidth($hwnd)
    $ch = [F213Win]::ClientHeight($hwnd)
    Write-Host ("f213 capture client {0}x{1} at {2},{3} printwindow=2" -f $cw, $ch, $origin.X, $origin.Y)

    $framesList = @()
    $shots = @()
    $printed = $true
    $prev = $null
    for ($i = 0; $i -lt $Frames; $i++) {
        $bmp = Shot-PrintWindow $hwnd $cw $ch
        if ($null -eq $bmp) { $bmp = Shot-Screen $origin.X $origin.Y $cw $ch; $printed = $false }
        $px = Pixels $bmp
        $luma = Stats $px
        $diff = if ($null -ne $prev) { FrameDiff $prev $px } else { 0.0 }
        $framesList += [pscustomobject]@{ N = $i; Luma = $luma; Diff = $diff }
        if ($i -eq 0 -or $i -eq 1 -or $i -eq 2) {
            $path = Join-Path $OutDir ("f213_{0}_frame{1}.png" -f $tag, $i)
            $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
            $shots += $path
        }
        $bmp.Dispose()
        $prev = $px
        Start-Sleep -Milliseconds $Interval
    }


    Write-Host ""
    Write-Host "---- 40 frames (30 ms apart) ----"
    Write-Host "n   luma   diff"
    foreach ($f in $framesList) { Write-Host ("{0,2}  {1,6:N2}  {2,6:N2}" -f $f.N, $f.Luma, $f.Diff) }
    $lum = $framesList | ForEach-Object { $_.Luma }
    $avg = ($lum | Measure-Object -Average).Average
    $std = [math]::Sqrt((($lum | ForEach-Object { [math]::Pow($_ - $avg, 2) } | Measure-Object -Sum).Sum) / $lum.Count)
    $diffAvg = (($framesList | Select-Object -Skip 1 | ForEach-Object { $_.Diff }) | Measure-Object -Average).Average
    $diffMax = (($framesList | Select-Object -Skip 1 | ForEach-Object { $_.Diff }) | Measure-Object -Maximum).Maximum
    Write-Host ("f213 frames avg={0:N2} std={1:N2} diff_avg={2:N2} diff_max={3:N2} source={4}" -f $avg, $std, $diffAvg, $diffMax, $(if ($printed) { "printwindow" } else { "screen-bitblt" }))
    if (-not $printed) { Write-Host "f213 note: PrintWindow(2) returned nothing usable; frames are screen reads" }
    $report = Join-Path $OutDir ("f213_{0}_frames.txt" -f $tag)
    $lines = @("stage=$Stage interp=$Interp source=$(if ($printed) { 'printwindow' } else { 'screen-bitblt' })")
    $lines += "n`tluma`tdiff"
    foreach ($f in $framesList) { $lines += ("{0}`t{1:N2}`t{2:N2}" -f $f.N, $f.Luma, $f.Diff) }
    $lines += ("avg={0:N2} std={1:N2} diff_avg={2:N2} diff_max={3:N2}" -f $avg, $std, $diffAvg, $diffMax)
    $lines | Set-Content -Path $report -Encoding UTF8
    Write-Host ("f213 frames table -> {0}" -f $report)
} finally {
    if ($proc -and -not $proc.HasExited) {
        Start-Sleep -Milliseconds 400
        $proc.Kill()
        $proc.WaitForExit(5000) | Out-Null
    }
    if ($null -ne $desktop) { $desktop.Dispose() }
}

Write-Host ""
Write-Host "---- f213 trace (4 lines/s) ----"
if (Test-Path $Log) {
    Select-String -Path $Log -Pattern "f213 slot" | Select-Object -Last 80 | ForEach-Object { Write-Host $_.Line }
    Write-Host ""
    Write-Host "---- rates ----"
    Select-String -Path $Log -Pattern "f200 layers|dcomp video layer|interp plan|decode=mf|enhance ready" | Select-Object -Last 12 | ForEach-Object { Write-Host $_.Line }
} else {
    Write-Host "没有日志 $Log"
}
