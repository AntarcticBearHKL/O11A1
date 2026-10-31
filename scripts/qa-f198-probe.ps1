# f198 probe: three layer rates (video / danmaku / UI) on the DComp player.
# Developer tool, not product code.
#
#   powershell -File scripts/qa-f198-probe.ps1 -W 800 -H 800 -Soak 25
#   powershell -File scripts/qa-f198-probe.ps1 -W 1920 -H 1080 -Soak 25 -Interp "3" -Four "1"
#
# The window size comes from `settings.json`'s `player_layout`, restored before
# the app launches and put back afterwards: resizing the HWND from outside loses
# to the app's own layout restore, and the player window's client rect is what
# decides the decoded frame size (interpolation always follows the window).
#
# Prints only the log lines the report needs: `f196 layers`, `dcomp ui paint`,
# `dcomp danmaku paint`, `dcomp video geom`, `interp`, `f174 soak`.
param(
    [int]$W = 800,
    [int]$H = 800,
    [int]$Soak = 25,
    [int]$BootMs = 12000,
    [string]$Bvid = "BV1BBnb6aEww",
    [string]$Exe = "target/release/o11a1.exe",
    [string]$Tag = "probe",
    [string]$Interp = "",
    [string]$Four = "",
    [string]$Extra = "",
    [switch]$NoDcomp,
    [switch]$NoDanmaku,
    [switch]$KeepChrome,
    [switch]$Watch
)

$ErrorActionPreference = "Stop"
if (-not ("F198Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public class F198Win {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
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
    public static string Class(IntPtr h) { var sb = new StringBuilder(512); GetClassName(h, sb, 512); return sb.ToString(); }
}
"@
}

$settingsPath = Join-Path (Get-Location) "settings.json"
# UTF-8 explicitly: `Get-Content -Raw` decodes as ANSI on PowerShell 5.1 and
# mangles every CJK title in the file.
$settingsBackup = [IO.File]::ReadAllText($settingsPath, [Text.Encoding]::UTF8)
$dir = Join-Path (Get-Location) "target/qa"
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
$Log = Join-Path $dir ("f198_" + $Tag + ".log")
if (Test-Path $Log) { Remove-Item $Log -Force }

# The player window's restored layout, in logical px: [x, y, w, h]. The file is
# patched textually (a full re-serialise would reformat the user's settings and
# `Set-Content -Encoding UTF8` adds a BOM serde would choke on).
$layoutJson = "[0, 0, $W, $H]"
$patched = [regex]::Replace(
    $settingsBackup,
    '"player_layout"\s*:\s*(\[[^\]]*\]|null)',
    ('"player_layout": ' + $layoutJson)
)
if ($patched -eq $settingsBackup) { throw "settings.json has no player_layout field" }
[IO.File]::WriteAllText($settingsPath, $patched, (New-Object Text.UTF8Encoding($false)))

$env:BILI_DEBUG_LOG = $Log
$env:BILI_OPEN_BVID = $Bvid
$env:BILI_SOAK_TEST = "$Soak"
if ($NoDcomp) { Remove-Item Env:BILI_PLAYER_DCOMP -ErrorAction SilentlyContinue } else { $env:BILI_PLAYER_DCOMP = "1" }
if ($NoDanmaku) { Remove-Item Env:BILI_DCOMP_DANMAKU -ErrorAction SilentlyContinue } else { $env:BILI_DCOMP_DANMAKU = "1" }
if ($Interp -ne "") { $env:BILI_INTERP_TEST = $Interp } else { Remove-Item Env:BILI_INTERP_TEST -ErrorAction SilentlyContinue }
if ($Four -ne "") { $env:BILI_INTERP_4X = $Four } else { Remove-Item Env:BILI_INTERP_4X -ErrorAction SilentlyContinue }
if ($Extra -ne "") {
    foreach ($pair in $Extra.Split(";")) {
        if ($pair -eq "") { continue }
        $kv = $pair.Split("=", 2)
        Set-Item -Path ("Env:" + $kv[0]) -Value $kv[1]
    }
}

$proc = Start-Process -FilePath $Exe -PassThru
try {
    $hwnd = [IntPtr]::Zero
    $deadline = (Get-Date).AddMilliseconds($BootMs + 30000)
    while ((Get-Date) -lt $deadline) {
        foreach ($h in [F198Win]::Windows([uint32]$proc.Id)) {
            if ([F198Win]::Class($h) -eq "O11A1DcompPlayer") { $hwnd = $h; break }
        }
        if ($hwnd -ne [IntPtr]::Zero) { break }
        Start-Sleep -Milliseconds 300
    }
    if ($hwnd -eq [IntPtr]::Zero) { throw "no DComp player window" }
    $c = New-Object F198Win+RECT
    [void][F198Win]::GetClientRect($hwnd, [ref]$c)
    Write-Host ("window hwnd={0} client={1}x{2}" -f $hwnd, ($c.Right - $c.Left), ($c.Bottom - $c.Top))
    Start-Sleep -Milliseconds $BootMs
    [void][F198Win]::SetForegroundWindow($hwnd)
    $rr = New-Object F198Win+RECT
    [void][F198Win]::GetWindowRect($hwnd, [ref]$rr)
    [void][F198Win]::SetCursorPos([int](($rr.Left + $rr.Right) / 2), [int](($rr.Top + $rr.Bottom) / 2))
    $nudge = 0
    # The chrome hides after ~2.5 s of a still pointer (CHROME_IDLE_MS). Keep
    # nudging from the moment the window is focused, so the pill is on screen
    # for the whole soak instead of only after the boot wait.
    if ($KeepChrome) {
        for ($i = 0; $i -lt 6; $i++) {
            Start-Sleep -Milliseconds 500
            $nudge = ($nudge + 1) % 2
            [void][F198Win]::SetCursorPos([int](($rr.Left + $rr.Right) / 2) + $nudge, [int](($rr.Top + $rr.Bottom) / 2))
        }
    } else {
        Start-Sleep -Milliseconds 600
    }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $nudge = 0
    while ($sw.Elapsed.TotalSeconds -lt ($Soak + 4)) {
        Start-Sleep -Milliseconds 500
        if ($KeepChrome) {
            # The chrome hides after ~2.5 s of a still pointer (CHROME_IDLE_MS).
            # A 1 px nudge resets that timer, exactly like a user moving the
            # mouse, so the pill stays on screen for the whole measurement.
            $nudge = ($nudge + 1) % 2
            [void][F198Win]::SetCursorPos([int](($rr.Left + $rr.Right) / 2) + $nudge, [int](($rr.Top + $rr.Bottom) / 2))
        }
        if ($Watch -and (Test-Path $Log)) {
            $last = Get-Content $Log -Tail 1
            Write-Host ("  t={0:n1}s {1}" -f $sw.Elapsed.TotalSeconds, $last)
        }
    }
} finally {
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    Start-Sleep -Milliseconds 800
    [IO.File]::WriteAllText($settingsPath, $settingsBackup, (New-Object Text.UTF8Encoding($false)))
}

if (Test-Path $Log) {
    Write-Host "---- f196 layers ----"
    Select-String -Path $Log -Pattern "f196 layers" | ForEach-Object { $_.Line }
    Write-Host "---- ui paint ----"
    Select-String -Path $Log -Pattern "dcomp ui paint" | Select-Object -Last 8 | ForEach-Object { $_.Line }
    Write-Host "---- danmaku paint ----"
    Select-String -Path $Log -Pattern "dcomp danmaku paint" | Select-Object -Last 6 | ForEach-Object { $_.Line }
    Write-Host "---- geometry ----"
    Select-String -Path $Log -Pattern "dcomp video geom" | Select-Object -Last 6 | ForEach-Object { $_.Line }
    Write-Host "---- video target ----"
    Select-String -Path $Log -Pattern "video target" | Select-Object -Last 4 | ForEach-Object { $_.Line }
    Write-Host "---- interp ----"
    Select-String -Path $Log -Pattern "interp |ofa |blockmatch" | Select-Object -Last 12 | ForEach-Object { $_.Line }
    Write-Host "---- soak ----"
    Select-String -Path $Log -Pattern "f174 soak" | Select-Object -Last 8 | ForEach-Object { $_.Line }
}
