# QA helper for f212: the shipped default path at 1080p/720p.
#
#   powershell -File scripts/qa-f210.ps1                        # 1080p, strips on, 60 s
#   powershell -File scripts/qa-f210.ps1 -Strips 0              # the Slint layer (the f209 A/B)
#   powershell -File scripts/qa-f210.ps1 -Stage 720 -Seconds 60
#   powershell -File scripts/qa-f210.ps1 -Repeat 3              # the strips soak
#
# Differences from `qa-f209.ps1`:
#
# * `BILI_F210_TRACE=1` - the 1 Hz device / upload / video-memory line
#   (`f210 trace ...`) next to the per-second `f200 layers ...` line;
# * `-Strips 1|0` sets `BILI_DANMAKU_STRIPS` explicitly, so both arms of the
#   comment layer are measured by the same script (f210 made strips default on;
#   `=0` is the fallback the old 1080p numbers came from);
# * `-Repeat N` runs the whole thing N times and reports, per run, whether the
#   process was still alive at the end - that is the soak criterion
#   (`nvwgf2umx.dll` faults killed the f207 runs 10-40 s in).
#
# The sample is the same local 1080p30/720p30 file `qa-f209.ps1` generates:
# real H.264, no audio, so the pacer paces on the wall clock like a silent
# stream. 弹幕 still comes from the account's own session (that layer is what
# this run measures).
param(
    [ValidateSet("720", "1080")]
    [string]$Stage = "1080",
    [string]$Exe = "target/release/o11a1.exe",
    [int]$Seconds = 60,
    # "" leaves `BILI_DANMAKU_STRIPS` unset: that is the shipped default since
    # f210 and the run that proves it.
    [ValidateSet("0", "1", "")]
    [string]$Strips = "1",
    [int]$Repeat = 1,
    # Take the screenshot this many seconds into playback (0 = at the end).
    # A capture inside the run is what "playing, dense comments, panel open"
    # needs: the end of a 60 s run can land on a stall.
    [int]$ShotAt = 0,
    # How many frames a second apart to capture: `dm_s` on the panel is a
    # one-second rate, so a single instant can read a dip.
    [int]$Shots = 1,
    [switch]$NoShot,
    # Open the 性能 panel for the whole run (`BILI_F201_TEST`): the acceptance
    # screenshot has to show `弹幕` = dm_s, and an animated chrome is what the
    # `ui_s` row is judged on.
    [switch]$Panel,
    [string]$Bvid = "BV1BBnb6aEww",
    [string]$OutDir = "target/qa/f212"
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

if (-not ("F210Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public class F210Win {
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

# Same sample as f209: generating it twice would be a different bitstream and a
# different decoder load, and the f209 numbers are the baseline this run is
# compared with.
$sample = Join-Path $root ("target/f209/sample{0}p30.mp4" -f $Stage)
if (-not (Test-Path $sample)) {
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $sample) | Out-Null
    $size = "{0}x{1}" -f $clientW, $clientH
    Write-Host ("f210 sample generating {0} ({1})" -f $sample, $size)
    & ffmpeg -hide_banner -loglevel error -y -f lavfi -i "testsrc2=size=${size}:rate=30" -t 60 `
        -c:v libx264 -pix_fmt yuv420p -profile:v high -preset veryfast -crf 20 $sample
}

$exePath = if ([System.IO.Path]::IsPathRooted($Exe)) { $Exe } else { Join-Path $root $Exe }
if (-not (Test-Path $exePath)) { throw "找不到 $exePath（先 cargo build --release）" }
$imageName = [System.IO.Path]::GetFileNameWithoutExtension($exePath)

function Find-Player($proc, $timeoutMs) {
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    while ((Get-Date) -lt $deadline) {
        if ($proc.HasExited) { return [IntPtr]::Zero }
        foreach ($h in [F210Win]::Windows([uint32]$proc.Id)) {
            if ([F210Win]::Class($h) -eq "O11A1DcompPlayer") { return $h }
        }
        Start-Sleep -Milliseconds 300
    }
    return [IntPtr]::Zero
}

$results = @()
for ($run = 1; $run -le $Repeat; $run++) {
    $tag = "f212_{0}p_default" -f $Stage
    if ($Repeat -gt 1) { $tag = "{0}_r{1}" -f $tag, $run }
    $Log = Join-Path $OutDir ("{0}.log" -f $tag)
    if (Test-Path $Log) { Remove-Item $Log -Force }

    # Only this build's own image: another QA run (a different agent's f211
    # seek soak) may be using the shared `o11a1.exe`, and killing it would both
    # invalidate their run and poison this one's numbers.
    Get-Process $imageName -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 500

    $env:BILI_DEBUG_LOG = $Log
    $env:BILI_OPEN_BVID = $Bvid
    $env:BILI_QUALITY_PICK = "0,6000"
    # No path switch: 画面直出 + 零拷贝 are the shipped defaults (f212).
    Remove-Item Env:BILI_PLAYER_DCOMP -ErrorAction SilentlyContinue
    Remove-Item Env:BILI_DECODE -ErrorAction SilentlyContinue
    $env:BILI_DCOMP_DANMAKU = "1"
    $env:BILI_INTERP_TEST = "3"
    $env:BILI_INTERP_4X = "1"
    $env:BILI_F200_LAYERS = "$Seconds"
    $env:BILI_F210_TRACE = "1"
    $env:BILI_QA_MF_FILE = $sample
    Remove-Item Env:BILI_DANMAKU_STRIPS -ErrorAction SilentlyContinue
    foreach ($name in @("BILI_PANEL_TEST", "BILI_FULLSCREEN_TEST", "BILI_DCOMP_UI_FORCE",
            "BILI_WARP_TRACE", "BILI_SOAK_TEST", "BILI_PACE_TRACE", "BILI_DANMAKU_EMPTY",
            "BILI_F201_TEST")) {
        Remove-Item ("Env:" + $name) -ErrorAction SilentlyContinue
    }
    if ($Panel) { $env:BILI_F201_TEST = "$($Seconds + 20)" }

    Write-Host ("`n=== run {0}/{1} {2} exe={3} log={4}" -f $run, $Repeat, $tag, $exePath, $Log)
    $proc = Start-Process -FilePath $exePath -WorkingDirectory $root -PassThru
    $alive = $false
    $status = "unknown"
    try {
        $hwnd = Find-Player $proc 70000
        if ($hwnd -eq [IntPtr]::Zero) { throw "70 秒内没有播放器窗口（进程退出码 {0}）" -f $(if ($proc.HasExited) { $proc.ExitCode } else { "-" }) }
        Write-Host ("f210 player hwnd={0}" -f $hwnd)

        # Client exactly the stage size, window TOPMOST (the screenshot is a
        # screen read of a scan-out composition).
        $outer = New-Object F210Win+RECT
        [void][F210Win]::GetWindowRect($hwnd, [ref]$outer)
        $deltaW = ($outer.Right - $outer.Left) - [F210Win]::ClientWidth($hwnd)
        $deltaH = ($outer.Bottom - $outer.Top) - [F210Win]::ClientHeight($hwnd)
        for ($pass = 0; $pass -lt 10; $pass++) {
            [void][F210Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, ($clientW + $deltaW), ($clientH + $deltaH), 0x0010 -bor 0x0040)
            Start-Sleep -Milliseconds 300
            if ([F210Win]::ClientWidth($hwnd) -eq $clientW -and [F210Win]::ClientHeight($hwnd) -eq $clientH) { break }
        }
        [void][F210Win]::ShowWindow($hwnd, 9)
        [void][F210Win]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0010)
        [void][F210Win]::SetForegroundWindow($hwnd)
        Start-Sleep -Milliseconds 900
        $client = "{0}x{1}" -f [F210Win]::ClientWidth($hwnd), [F210Win]::ClientHeight($hwnd)
        Write-Host ("f210 client {0}" -f $client)

        $shotDelay = if ($ShotAt -gt 0 -and $ShotAt -lt $Seconds) { $ShotAt } else { $Seconds }
        Start-Sleep -Seconds $shotDelay
        $alive = -not $proc.HasExited
        if ($alive -and -not $NoShot) {
            # Fixed handle: a second player window (a stray click from another
            # run on this machine) must not be what the capture lands on.
            $fresh = Find-Player $proc 2000
            if ($fresh -ne [IntPtr]::Zero -and $fresh -ne $hwnd) {
                Write-Host ("f210 warn: player window changed {0} -> {1}" -f $hwnd, $fresh)
            }
            $shotName = if ($Stage -eq "1080") { "f212_default_120" } else { "f212_default_720" }
            $shot = if ($Repeat -gt 1) {
                Join-Path $root ("docs/{0}_r{1}.png" -f $shotName, $run)
            } else {
                Join-Path $root ("docs/{0}.png" -f $shotName)
            }
            # The whole client, not just the stage: opening the 性能 panel widens
            # the window (the app keeps the picture at the stage width and the
            # panel sits beside it), and the panel is what the capture is for.
            $capW = [Math]::Max([F210Win]::ClientWidth($hwnd), 320)
            $capH = [Math]::Max([F210Win]::ClientHeight($hwnd), 240)
            for ($shotIndex = 1; $shotIndex -le [Math]::Max($Shots, 1); $shotIndex++) {
                if ($shotIndex -gt 1) {
                    Start-Sleep -Milliseconds 1100
                    $capW = [Math]::Max([F210Win]::ClientWidth($hwnd), 320)
                    $capH = [Math]::Max([F210Win]::ClientHeight($hwnd), 240)
                }
                $bitmap = New-Object System.Drawing.Bitmap $capW, $capH
                $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
                $origin = New-Object F210Win+POINT
                [void][F210Win]::ClientToScreen($hwnd, [ref]$origin)
                $graphics.CopyFromScreen($origin.X, $origin.Y, 0, 0, $bitmap.Size)
                $target = if ($Shots -gt 1) {
                    $shot -replace '\.png$', ("_{0}.png" -f $shotIndex)
                } else {
                    $shot
                }
                $bitmap.Save($target, [System.Drawing.Imaging.ImageFormat]::Png)
                $graphics.Dispose()
                $bitmap.Dispose()
                Write-Host ("f210 screenshot {0} at {1}s client={2}x{3}" -f $target, ($shotDelay + $shotIndex - 1), $capW, $capH)
            }
        }
        if ($shotDelay -lt $Seconds) { Start-Sleep -Seconds ($Seconds - $shotDelay) }
        if (-not $alive) {
            $status = "crashed exit=0x{0:x8}" -f $proc.ExitCode
        } else {
            $status = "survived"
        }

    } catch {
        $alive = -not $proc.HasExited
        $status = if ($alive) { "no-window" } else { "crashed exit=0x{0:x8}" -f $proc.ExitCode }
        Write-Host ("f210 run {0} failed: {1}" -f $run, $_.Exception.Message)
    } finally {
        if ($proc -and -not $proc.HasExited) {
            $proc.Kill()
            $proc.WaitForExit(5000) | Out-Null
        }
        $results += [pscustomobject]@{ run = $run; tag = $tag; status = $status; log = $Log }
    }
}

Write-Host ""
Write-Host "==== runs ===="
$results | Format-Table -AutoSize | Out-String | Write-Host

foreach ($result in $results) {
    if (-not (Test-Path $result.log)) { continue }
    Write-Host ""
    Write-Host ("---- {0} ({1}) ----" -f $result.tag, $result.status)
    $lines = Select-String -Path $result.log -Pattern "f200 layers" | Select-Object -Last 62
    foreach ($line in $lines) { Write-Host $line.Line }
    $trace = Select-String -Path $result.log -Pattern "f210 trace" | Select-Object -First 2
    foreach ($line in $trace) { Write-Host $line.Line }
    $trace = Select-String -Path $result.log -Pattern "f210 trace" | Select-Object -Last 2
    foreach ($line in $trace) { Write-Host $line.Line }
    $slow = Select-String -Path $result.log -Pattern "f210 danmaku slow|danmaku strips|f210 danmaku layer resize"
    foreach ($line in $slow) { Write-Host $line.Line }
}
