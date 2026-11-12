# Native player QA: launch, force the window to the foreground, wait for the
# 插帧/画质增强 QA actions, capture the composited window with CopyFromScreen
# (PrintWindow is meaningless on WS_EX_NOREDIRECTIONBITMAP), then wait for the
# auto-exit.
#
#   powershell -ExecutionPolicy Bypass -File native_player\qa.ps1
param(
    [string]$Exe = "C:\Users\antar\AppData\Local\Temp\opencode\o11a1-player-target\release\native_player.exe",
    [string]$Bvid = "BV1mhxo6BEG4",
    [int]$Seconds = 24,
    [string]$OutPng = "C:\Users\antar\AppData\Local\Temp\opencode\native_player.png",
    [string]$LogPath = "C:\Users\antar\AppData\Local\Temp\opencode\native_player_run.log",
    [string]$Qa = "danmaku,seek,pause,resume,interp,enhance,anime,stats,shot"
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class NativePlayerWin32 {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, System.Text.StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
}
"@

function Find-PlayerWindow([int]$processId) {
    $global:foundWindow = [IntPtr]::Zero
    $callback = {
        param($h, $l)
        $owner = 0
        [void][NativePlayerWin32]::GetWindowThreadProcessId($h, [ref]$owner)
        if ($owner -eq $processId) {
            $class = New-Object System.Text.StringBuilder 128
            [void][NativePlayerWin32]::GetClassName($h, $class, 128)
            if ($class.ToString() -eq "O11A1NativePlayer") {
                $global:foundWindow = $h
                return $false
            }
        }
        return $true
    }
    [void][NativePlayerWin32]::EnumWindows($callback, [IntPtr]::Zero)
    return $global:foundWindow
}

$env:BILI_OPEN_BVID = $Bvid
$env:BILI_PLAYER_QA = $Qa
$env:BILI_PLAYER_SECONDS = "$Seconds"
$env:BILI_PLAYER_SHOT = "C:\Users\antar\AppData\Local\Temp\opencode\native_player_qa.png"

if (Test-Path $LogPath) { Remove-Item $LogPath -Force }
$errLog = [System.IO.Path]::ChangeExtension($LogPath, ".err.log")
if (Test-Path $errLog) { Remove-Item $errLog -Force }

$proc = Start-Process -FilePath $Exe -PassThru -RedirectStandardError $errLog
Write-Host "qa: pid=$($proc.Id) waiting for window"

$hwnd = [IntPtr]::Zero
for ($i = 0; $i -lt 200; $i++) {
    Start-Sleep -Milliseconds 100
    $hwnd = Find-PlayerWindow $proc.Id
    if ($hwnd -ne [IntPtr]::Zero) { break }
}
if ($hwnd -eq [IntPtr]::Zero) {
    Write-Host "qa: window not found"
    if (-not $proc.HasExited) { $proc.Kill() }
    exit 1
}
Write-Host "qa: hwnd=$hwnd"
[void][NativePlayerWin32]::ShowWindow($hwnd, 9)
# Foreground changes are restricted for a background process; a temporary
# topmost placement reliably brings the window above other apps for the shot.
[void][NativePlayerWin32]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0040)
[void][NativePlayerWin32]::BringWindowToTop($hwnd)
[void][NativePlayerWin32]::SetForegroundWindow($hwnd)

# Wait until the log says Anime4K is active, or 16 s, whichever is first.
$deadline = (Get-Date).AddSeconds(18)
$wantStats = $Qa -match "stats"
$wantDanmaku = $Qa -match "danmaku"
while ((Get-Date) -lt $deadline) {
    [void][NativePlayerWin32]::ShowWindow($hwnd, 9)
    [void][NativePlayerWin32]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0040)
    [void][NativePlayerWin32]::SetForegroundWindow($hwnd)
    if (Test-Path $errLog) {
        $text = Get-Content $errLog -Raw -ErrorAction SilentlyContinue
        $ready = $true
        if ($Qa -match "interp") { $ready = $ready -and ($text -match "interp mode=1") }
        if ($Qa -match "anime|enhance2") { $ready = $ready -and ($text -match "enhance mode=2") }
        elseif ($Qa -match "enhance|cas") { $ready = $ready -and ($text -match "enhance mode=1") }
        if ($wantStats) { $ready = $ready -and ($text -match "stats=1") }
        if ($wantDanmaku) { $ready = $ready -and ($text -match "danmaku loaded n=") }
        if ($Qa -match "mini") { $ready = $ready -and ($text -match "mini=1") }
        if ($Qa -match "fullscreen|fs") { $ready = $ready -and ($text -match "fullscreen=1") }
        if ($Qa -match "dmpopup|danmakupopup") { $ready = $ready -and ($text -match "danmaku popup=1") }
        if ($Qa -match "dm_menu|dmmenu") { $ready = $ready -and ($text -match "qa danmaku menu") }
        if ($Qa -match "dm_hold|dmhold") { $ready = $ready -and ($text -match "danmaku long-press fired") }
        if ($Qa -match "dm_like|dmlike") { $ready = $ready -and ($text -match "danmaku action kind=点赞") }
        if ($Qa -match "dm_report_one|dmreportone|dm_report1") { $ready = $ready -and ($text -match "danmaku action kind=举报") }
        elseif ($Qa -match "dm_report|dmreport") { $ready = $ready -and ($text -match "qa danmaku report submenu") }
        if ($Qa -match "dm_block|dmblock") { $ready = $ready -and ($text -match "danmaku block mid_hash=") }
        if ($Qa -match "dm_copy|dmcopy") { $ready = $ready -and ($text -match "danmaku copy content") }
        if ($Qa -match "dm_undo|dmundo") { $ready = $ready -and ($text -match "danmaku block undo mid_hash=") }
        if ($Qa -match "dm_history_date|dmhistorydate") { $ready = $ready -and ($text -match "danmaku history applied") }
        elseif ($Qa -match "dm_history|dmhistory") { $ready = $ready -and ($text -match "danmaku history dates loaded") }
        if ($Qa -match "shot|screenshot") { $ready = $ready -and ($text -match "qa shot path=") }
        if ($Qa -match "comments|comment") { $ready = $ready -and ($text -match "detail avatars loaded n=") }
        if ($Qa -match "quality|audio_menu|audiomenu") { $ready = $ready -and ($text -match "settings menu=") }
        if ($Qa -match "quality_low|quality_next|audio_next|audionext") { $ready = $ready -and ($text -match "source track (switch|applied)") }
        if ($Qa -match "quality_locked|qualitylocked") { $ready = $ready -and ($text -match "quality locked") }
        if ($Qa -match "hover") { $ready = $ready -and ($text -match "storyboard (cell|unavailable)") }
        if ($Qa -match "sleep_short|sleepshort") { $ready = $ready -and ($text -match "sleep timer expired") }
        if ($Qa -match "fault") { $ready = $ready -and ($text -match "player error") }
        if ($Qa -match "stall") { $ready = $ready -and ($text -match "qa stall overlay") }
        if ($ready) { break }
    }
    Start-Sleep -Milliseconds 200
}
Start-Sleep -Milliseconds 500
[void][NativePlayerWin32]::ShowWindow($hwnd, 9)
[void][NativePlayerWin32]::SetForegroundWindow($hwnd)
Start-Sleep -Milliseconds 400

$rect = New-Object NativePlayerWin32+RECT
if (-not [NativePlayerWin32]::GetWindowRect($hwnd, [ref]$rect)) {
    Write-Host "qa: GetWindowRect failed"
    exit 1
}
$width = $rect.Right - $rect.Left
$height = $rect.Bottom - $rect.Top
$bitmap = New-Object System.Drawing.Bitmap($width, $height)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen($rect.Left, $rect.Top, 0, 0, $bitmap.Size)
$bitmap.Save($OutPng, [System.Drawing.Imaging.ImageFormat]::Png)
$graphics.Dispose()
$bitmap.Dispose()
Write-Host "qa: shot=$OutPng size=${width}x${height}"

[void]$proc.WaitForExit(40000)
if (-not $proc.HasExited) { $proc.Kill() }
if (Test-Path $errLog) {
    Copy-Item $errLog $LogPath -Force
    Get-Content $LogPath -Tail 12
}
Write-Host "qa: done"
