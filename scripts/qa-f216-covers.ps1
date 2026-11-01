# f216: do the covers survive a real session?
#
# Developer tool, not product code.
#
#   powershell -File scripts/qa-f216-covers.ps1                    # 10 minutes
#   powershell -File scripts/qa-f216-covers.ps1 -Minutes 5
#   powershell -File scripts/qa-f216-covers.ps1 -KeepOpen          # leave it up
#
# One launch, one log. Inside the app:
#   * `BILI_COVER_SOAK=<minutes>,<page_ms>` walks 首页 / 分区 / 番剧 / 直播 /
#     搜索 / 历史 / 收藏, holding the home feed at the 0/5/10 minute marks so
#     the three acceptance shots are the SAME page.
#   * `BILI_COVER_WATCH=1000` writes one line per second with the cover count of
#     every list, the in-flight decode set (count + oldest age), the cache
#     hit/miss/fail/drop counters, the per-second HTTP status buckets and the
#     cover request timing.
# The script drives the real wheel over the list (three screens per page) and
# takes docs/f216_covers_0min.png / _5min.png / _10min.png through PrintWindow.
#
# What to read in the log:
#   covers t=<s>s page=<p> feed=<c>/<t> ... decodes=<n> ...
#       one per second: the list cover counts and the counters' deltas
#   covers lost t=.. list=.. from=.. to=..
#       a cover that was shown and then vanished (the reported failure)
#   covers decode leak n=.. oldest_ms=.. path=.. url=..
#       a decode held longer than any request may live
#   covers http status=403 url=..
#       the CDN answering a cover request with a non-2xx status
param(
    [string]$Exe = "target/release/o11a1.exe",
    [int]$Minutes = 10,
    [int]$PageMs = 15000,
    [int]$BootMs = 18000,
    [string]$OutDir = "docs",
    [string]$LogPath = "target/qa/f216_covers.log",
    # Open + close a real player once per page cycle (the "播放器关掉后所有封面
    # 都没了" regression).
    [switch]$Player,
    [switch]$KeepOpen
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

if (-not ("F216Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public class F216Win {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, int dx, int dy, uint data, UIntPtr extra);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hWnd, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
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

$root = (Resolve-Path -LiteralPath ".").Path
$shotDir = Join-Path $root $OutDir
if (-not (Test-Path $shotDir)) { New-Item -ItemType Directory -Path $shotDir -Force | Out-Null }
$log = Join-Path $root $LogPath
$logDir = Split-Path -Parent $log
if (-not (Test-Path $logDir)) { New-Item -ItemType Directory -Path $logDir -Force | Out-Null }
if (Test-Path $log) { Remove-Item $log -Force }

function Find-MainWindow([int]$ProcessId, [int]$TimeoutMs) {
    $deadline = (Get-Date).AddMilliseconds($TimeoutMs)
    while ((Get-Date) -lt $deadline) {
        foreach ($h in [F216Win]::Windows([uint32]$ProcessId)) {
            if ([F216Win]::Class($h) -eq "O11A1DcompPlayer") { continue }
            $r = New-Object F216Win+RECT
            [void][F216Win]::GetWindowRect($h, [ref]$r)
            $w = $r.Right - $r.Left
            $hgt = $r.Bottom - $r.Top
            if ($w -ge 700 -and $hgt -ge 450) { return $h }
        }
        Start-Sleep -Milliseconds 300
    }
    return [IntPtr]::Zero
}

function Save-Shot([IntPtr]$Hwnd, [string]$Name) {
    $r = New-Object F216Win+RECT
    [void][F216Win]::GetWindowRect($Hwnd, [ref]$r)
    $w = $r.Right - $r.Left
    $h = $r.Bottom - $r.Top
    if ($w -le 0 -or $h -le 0) { return $null }
    $bmp = New-Object System.Drawing.Bitmap -ArgumentList ([int]$w), ([int]$h)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    $ok = [F216Win]::PrintWindow($Hwnd, $hdc, 2)
    if (-not $ok) { $ok = [F216Win]::PrintWindow($Hwnd, $hdc, 0) }
    $g.ReleaseHdc($hdc)
    $g.Dispose()
    $path = Join-Path $shotDir $Name
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    # Non-backdrop coverage, so "the shot is all gradient" is visible in the log.
    $lit = 0
    $step = [Math]::Max(8, [int]($w / 90))
    for ($y = 0; $y -lt $h; $y += $step) {
        for ($x = 0; $x -lt $w; $x += $step) {
            $c = $bmp.GetPixel($x, $y)
            if (($c.R + $c.G + $c.B) -gt 150) { $lit++ }
        }
    }
    $bmp.Dispose()
    "covers shot ok={0} path={1} size={2}x{3} lit={4}" -f $ok, $path, $w, $h, $lit
}

$env:BILI_DEBUG_LOG = $log
# `guard` closes a player window the run did not ask for (another QA run's stray
# click, or someone playing a video mid-run): covers pause while a player is
# open by design, so an unasked-for player would make the run measure nothing.
$watch = "1000"
if (-not $Player) { $watch = "1000,guard" }
$env:BILI_COVER_WATCH = $watch
$soak = "$Minutes,$PageMs"
if ($Player) { $soak = "$soak,player" }
$env:BILI_COVER_SOAK = $soak
$env:BILI_CAPTURE_DIR = $shotDir
# A build outside `target\` (say `target-f216\`) is not recognised as a dev
# build, so the root would be the exe's folder and the saved account would be
# missed. Point it at the repository explicitly.
if (-not $env:O11A1_HOME) { $env:O11A1_HOME = $root }
Write-Host "f216 covers: log=$log shot_dir=$shotDir minutes=$Minutes page_ms=$PageMs"
$proc = Start-Process -FilePath (Join-Path $root $Exe) -PassThru
$started = Get-Date
try {
    $hwnd = Find-MainWindow $proc.Id ($BootMs + 30000)
    if ($hwnd -eq [IntPtr]::Zero) { throw "main window not found" }
    Write-Host ("f216 covers: main window 0x{0:x}" -f [int64]$hwnd)
    [void][F216Win]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 1200

    # Screenshot marks are the feed holds: 0-45s, 300-345s, 600-645s.
    $shots = @(
        @{ At = 25.0; Name = "f216_covers_0min.png" },
        @{ At = 325.0; Name = "f216_covers_5min.png" },
        @{ At = 625.0; Name = "f216_covers_10min.png" }
    )
    $pending = @($shots)
    $deadline = $started.AddMinutes($Minutes).AddSeconds(70)
    $wheel = 0
    while ((Get-Date) -lt $deadline -and -not $proc.HasExited) {
        $r = New-Object F216Win+RECT
        [void][F216Win]::GetWindowRect($hwnd, [ref]$r)
        # Only drive the wheel while THIS window is free to be the foreground one.
        # Another o11a1 (a parallel QA run on the same desktop) keeps focus
        # otherwise, and then both runs would be injecting into the same window.
        $fg = [F216Win]::GetForegroundWindow()
        $fgPid = 0
        if ($fg -ne [IntPtr]::Zero) { [void][F216Win]::GetWindowThreadProcessId($fg, [ref]$fgPid) }
        $busy = $false
        if ($fgPid -ne 0 -and $fgPid -ne $proc.Id) {
            $owner = Get-Process -Id $fgPid -ErrorAction SilentlyContinue
            if ($owner -and $owner.ProcessName -eq "o11a1") { $busy = $true }
        }
        if (-not $busy) {
            $cx = [int](($r.Left + $r.Right) / 2)
            $cy = [int](($r.Top + $r.Bottom) / 2)
            [void][F216Win]::SetForegroundWindow($hwnd)
            [void][F216Win]::SetCursorPos($cx, $cy)
            # -120 per notch, three notches per tick, one tick every 500 ms: each
            # page gets well past "three screens" of real scrolling.
            for ($i = 0; $i -lt 3; $i++) {
                [F216Win]::mouse_event(0x0800, 0, 0, [uint32]65416, [UIntPtr]::Zero)
            }
            $wheel++
        }
        $elapsed = ((Get-Date) - $started).TotalSeconds
        foreach ($shot in @($pending)) {
            if ($elapsed -ge $shot.At) {
                $line = Save-Shot $hwnd $shot.Name
                Write-Host ("f216 covers: t={0:n0}s {1}" -f $elapsed, $line)
                $pending = @($pending | Where-Object { $_.Name -ne $shot.Name })
            }
        }
        Start-Sleep -Milliseconds 500
    }
    Write-Host ("f216 covers: run over after {0:n0}s, wheel ticks={1}" -f ((Get-Date) - $started).TotalSeconds, $wheel)
}
finally {
    if (-not $KeepOpen) {
        if (-not $proc.HasExited) {
            try { $proc.CloseMainWindow() | Out-Null } catch {}
            Start-Sleep -Milliseconds 800
            if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
        }
    }
}

if (Test-Path $log) {
    $lines = Get-Content $log
    $watch = @($lines | Where-Object { $_ -match "covers t=" })
    $lost = @($lines | Where-Object { $_ -match "covers lost|decode leak|covers http status=" })
    Write-Host ""
    Write-Host ("f216 covers: log lines={0} watch ticks={1} alarms={2}" -f $lines.Count, $watch.Count, $lost.Count)
    if ($watch.Count -gt 0) {
        foreach ($line in @($watch[0], $watch[[Math]::Min(60, $watch.Count - 1)], $watch[$watch.Count - 1])) {
            Write-Host "  $line"
        }
    }
    foreach ($line in $lost | Select-Object -First 20) { Write-Host "  ALARM $line" }
}
