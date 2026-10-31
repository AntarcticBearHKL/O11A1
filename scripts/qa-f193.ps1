# QA helper for f193 "播放列表：清空按钮 + 悬停打开 / 离开隐藏".
# Developer tool, not product code.
#
#   powershell -File scripts/qa-f193.ps1 -Stage probe
#   powershell -File scripts/qa-f193.ps1 -Stage hover
#   powershell -File scripts/qa-f193.ps1 -Stage clear
#
# The hover paths need the REAL cursor: the window hit-tests `GetCursorPos`
# against the disc / panel rectangles every 60ms, so this script moves the OS
# pointer (and puts it back afterwards) and grabs the client rect with GDI.
param(
    [ValidateSet("probe", "probe2", "hover", "clear", "regress")]
    [string]$Stage = "hover",
    [string]$Exe = "target\release\o11a1.exe",
    [string]$OutDir = "target\qa",
    [string]$Log,
    [int]$BootMs = 9000,
    [switch]$KeepRunning
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

if (-not ("F193Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public struct F193Rect { public int Left, Top, Right, Bottom; }
public struct F193Point { public int X, Y; }

public class F193Win {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);

    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out F193Rect rect);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr hWnd, ref F193Point point);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out F193Point point);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int cmd);
    [DllImport("user32.dll")] public static extern void SwitchToThisWindow(IntPtr hWnd, bool altTab);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern int GetWindowTextLength(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int max);

    public const uint LEFT_DOWN = 0x0002;
    public const uint LEFT_UP = 0x0004;
    public const uint WHEEL = 0x0800;

    // Raise a window from a background process: SetForegroundWindow is refused
    // unless this process owns the foreground, so the raise goes through
    // SwitchToThisWindow plus a one-frame TOPMOST/NOTOPMOST bounce. The grab
    // copies SCREEN pixels, so the window really has to be on top.
    public static bool Raise(IntPtr h) {
        ShowWindow(h, 9);           // SW_RESTORE
        SwitchToThisWindow(h, true);
        SetWindowPos(h, new IntPtr(-1), 0, 0, 0, 0, 0x0001 | 0x0002 | 0x0010);
        System.Threading.Thread.Sleep(120);
        SetWindowPos(h, new IntPtr(-2), 0, 0, 0, 0, 0x0001 | 0x0002 | 0x0010);
        SetForegroundWindow(h);
        BringWindowToTop(h);
        System.Threading.Thread.Sleep(120);
        return GetForegroundWindow() == h;
    }

    public static void Click() {
        mouse_event(LEFT_DOWN, 0, 0, 0, UIntPtr.Zero);
        System.Threading.Thread.Sleep(40);
        mouse_event(LEFT_UP, 0, 0, 0, UIntPtr.Zero);
    }

    public static void Scroll(int notches) {
        mouse_event(WHEEL, 0, 0, (uint)(-120 * notches), UIntPtr.Zero);
    }

    public static List<IntPtr> WindowsOf(uint pid) {
        var found = new List<IntPtr>();
        EnumWindows((h, l) => {
            uint owner;
            GetWindowThreadProcessId(h, out owner);
            if (owner != pid || !IsWindowVisible(h)) { return true; }
            F193Rect rect;
            if (!GetClientRect(h, out rect)) { return true; }
            if (rect.Right - rect.Left < 480 || rect.Bottom - rect.Top < 270) { return true; }
            found.Add(h);
            return true;
        }, IntPtr.Zero);
        return found;
    }

    public static string Title(IntPtr h) {
        int n = GetWindowTextLength(h);
        var text = new StringBuilder(Math.Max(n + 1, 2));
        GetWindowText(h, text, text.Capacity);
        return text.ToString();
    }

    public static string Class(IntPtr h) {
        var text = new StringBuilder(256);
        GetClassName(h, text, text.Capacity);
        return text.ToString();
    }

    public static int[] ClientOnScreen(IntPtr h) {
        F193Rect r;
        if (!GetClientRect(h, out r)) { return new int[] { 0, 0, 0, 0 }; }
        var origin = new F193Point();
        ClientToScreen(h, ref origin);
        return new int[] { origin.X, origin.Y, r.Right - r.Left, r.Bottom - r.Top };
    }
}
"@
}

# ---- the app's own layout constants (ui/app.slint) ----------------------
# The main window is pinned to 1280 x 800 logical at startup, the home bar is
# 56px tall and 28px above the bottom edge, the disc is centred in it, and the
# playlist panel is 450 wide, 26px above the bar, with a 46px header.
$WinW = 1280.0
$WinH = 800.0
$BarH = 56.0
$BarBottomGap = 28.0

function Get-MainWindow {
    param([string]$Name)
    $proc = Get-Process -Name $Name -ErrorAction SilentlyContinue |
        Sort-Object -Property WorkingSet -Descending | Select-Object -First 1
    if (-not $proc) { throw "no $Name process" }
    $best = $null
    foreach ($h in [F193Win]::WindowsOf([uint32]$proc.Id)) {
        $r = [F193Win]::ClientOnScreen($h)
        Write-Host ("candidate hwnd=0x{0:x} {1}x{2} class={3} title={4}" -f `
            $h.ToInt64(), $r[2], $r[3], [F193Win]::Class($h), [F193Win]::Title($h))
        if ($r[2] -lt 900 -or $r[3] -lt 600) { continue }
        # The main window is the 1280x800 (1.6) one; the player is 16:9.
        $ratio = [double]$r[2] / [double]$r[3]
        if ([Math]::Abs($ratio - ($WinW / $WinH)) -gt 0.05) { continue }
        $best = @{ Hwnd = $h; Rect = $r }
        break
    }
    if (-not $best) { throw "no 1280x800 main window" }
    return $best
}

function Start-App {
    param([string]$Path, [string]$LogPath)
    $full = [System.IO.Path]::GetFullPath($Path)
    if (-not (Test-Path $full)) { throw "no exe $full" }
    $env:BILI_DEBUG_LOG = [System.IO.Path]::GetFullPath($LogPath)
    if (Test-Path $env:BILI_DEBUG_LOG) { Remove-Item $env:BILI_DEBUG_LOG -Force }
    $proc = Start-Process -FilePath $full -PassThru
    Write-Host "start pid=$($proc.Id) log=$env:BILI_DEBUG_LOG"
    return $proc
}

function Save-Shot {
    param([IntPtr]$Hwnd, [string]$Name)
    # The grab copies SCREEN pixels, so the window is raised first (and the
    # hover state has settled again 220ms later, before the copy).
    $raised = [F193Win]::Raise($Hwnd)
    Start-Sleep -Milliseconds 220
    if (-not $raised) { Write-Host "warning: window not in the foreground for $Name" }
    $rect = [F193Win]::ClientOnScreen($Hwnd)
    $bitmap = New-Object System.Drawing.Bitmap $rect[2], $rect[3]
    $g = [System.Drawing.Graphics]::FromImage($bitmap)
    $g.CopyFromScreen($rect[0], $rect[1], 0, 0, $bitmap.Size)
    $g.Dispose()
    $dir = [System.IO.Path]::GetFullPath($OutDir)
    [void][System.IO.Directory]::CreateDirectory($dir)
    $path = Join-Path $dir $Name
    $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bitmap.Dispose()
    Write-Host ("shot {0} size={1}x{2} hwnd=0x{3:x}" -f $path, $rect[2], $rect[3], $Hwnd.ToInt64())
}

# Logical -> screen px: the client rect tells the DPI scale.
function Get-Scale {
    param($Rect)
    $sx = [double]$Rect[2] / $WinW
    $sy = [double]$Rect[3] / $WinH
    if ([Math]::Abs($sx - $sy) -gt 0.02) {
        throw ("client {0}x{1} is not 1280x800 scaled (sx={2:N3} sy={3:N3})" -f $Rect[2], $Rect[3], $sx, $sy)
    }
    return $sx
}

function To-Screen {
    param($Rect, [double]$Scale, [double]$X, [double]$Y)
    return @(($Rect[0] + $X * $Scale), ($Rect[1] + $Y * $Scale))
}

function Move-To {
    param($Rect, [double]$Scale, [double]$X, [double]$Y)
    $p = To-Screen $Rect $Scale $X $Y
    $wantX = [int][Math]::Round($p[0])
    $wantY = [int][Math]::Round($p[1])
    # The pointer is the real OS cursor, so someone using the mouse during a run
    # would move it away again: verify, and say so instead of measuring a lie.
    for ($try = 0; $try -lt 3; $try++) {
        [void][F193Win]::SetCursorPos($wantX, $wantY)
        Start-Sleep -Milliseconds 60
        $now = [F193Point]::new()
        [void][F193Win]::GetCursorPos([ref]$now)
        if ([Math]::Abs($now.X - $wantX) -le 2 -and [Math]::Abs($now.Y - $wantY) -le 2) { break }
        Write-Host ("cursor moved away try={0} want=({1},{2}) got=({3},{4})" -f $try, $wantX, $wantY, $now.X, $now.Y)
    }
    Write-Host ("cursor logical=({0:N0},{1:N0}) screen=({2},{3})" -f $X, $Y, $wantX, $wantY)
}

function Log-Last {
    param([string]$Tag, [int]$Lines = 3)
    if (-not $env:BILI_DEBUG_LOG -or -not (Test-Path $env:BILI_DEBUG_LOG)) { return }
    $tail = Get-Content $env:BILI_DEBUG_LOG -Tail $Lines
    Write-Host ("step {0}: {1}" -f $Tag, ($tail -join " | "))
}

# Poll the log until a pattern shows up in the lines written after `$script:seen`.
# The hover timers are the app's own 60ms clock, so a fixed sleep races them on a
# loaded machine; every step below waits for its receipt instead.
$script:seen = 0
function Wait-Log {
    param([string]$Pattern, [int]$TimeoutMs = 6000)
    if (-not (Test-Path $env:BILI_DEBUG_LOG)) { Write-Host "no log to wait on"; return $false }
    $deadline = [DateTime]::Now.AddMilliseconds($TimeoutMs)
    while ([DateTime]::Now -lt $deadline) {
        $all = @(Get-Content $env:BILI_DEBUG_LOG)
        if ($all.Count -gt $script:seen) {
            $new = $all[$script:seen..($all.Count - 1)]
            $script:seen = $all.Count
            foreach ($line in $new) {
                if ($line -match $Pattern) {
                    $script:lastMatch = $line
                    Write-Host ("wait ok /{0}/ -> {1}" -f $Pattern, $line)
                    return $true
                }
            }
        }
        Start-Sleep -Milliseconds 50
    }
    Write-Host ("wait TIMEOUT /{0}/ after {1}ms" -f $Pattern, $TimeoutMs)
    return $false
}

# The panel's current state according to the log (`playlist open source=… open=N`).
function Get-PlaylistOpen {
    if (-not (Test-Path $env:BILI_DEBUG_LOG)) { return $false }
    $last = @(Select-String -Path $env:BILI_DEBUG_LOG -Pattern "playlist open source=.* open=(\d)" |
        Select-Object -Last 1)
    if ($last.Count -eq 0) { return $false }
    return ($last[0].Matches[0].Groups[1].Value -eq "1")
}

# The lines written since the last Wait-Log, so a step can assert nothing else
# happened (e.g. "the panel did not close").
function Read-NewLog {
    if (-not (Test-Path $env:BILI_DEBUG_LOG)) { return @() }
    $all = @(Get-Content $env:BILI_DEBUG_LOG)
    if ($all.Count -le $script:seen) { return @() }
    $new = @($all[$script:seen..($all.Count - 1)])
    $script:seen = $all.Count
    return $new
}

function Save-TailOnly { return }

function Show-Tail {
    param([int]$Lines = 40)
    if (-not $env:BILI_DEBUG_LOG) { return }
    Write-Host "---- log tail $env:BILI_DEBUG_LOG"
    if (Test-Path $env:BILI_DEBUG_LOG) {
        Get-Content $env:BILI_DEBUG_LOG -Tail $Lines | Write-Host
    }
    Write-Host "---- hover / clear lines"
    if (Test-Path $env:BILI_DEBUG_LOG) {
        Select-String -Path $env:BILI_DEBUG_LOG -Pattern "playlist hover|playlist open source|playlist clear|playlist publish" |
            ForEach-Object { $_.Line } | Write-Host
    }
}

# ---- ordered geometry helpers -----------------------------------------
function Get-PanelBox {
    param([int]$Rows)
    $w = [Math]::Min(450.0, $WinW - 16.0)
    $barY = $WinH - $BarH - $BarBottomGap
    $maxH = [Math]::Min(520.0, [Math]::Min($WinH - 24.0, $barY - 34.0))
    $h = [Math]::Min($maxH, [Math]::Max(230.0, 95.0 + $Rows * 68.4))
    $x = ($WinW - $w) / 2.0
    $y = [Math]::Max(8.0, $barY - $h - 26.0)
    return @{ X = $x; Y = $y; W = $w; H = $h }
}

function Get-ClearButtonCenter {
    param($Panel)
    # Header: clear (62) | 8 | transport (172), 14px from the panel edge.
    $x = $Panel.X + $Panel.W - 14.0 - 172.0 - 8.0 - 62.0 + 31.0
    $y = $Panel.Y + 9.0 + 14.0
    return @{ X = $x; Y = $y }
}

# ---- go ----------------------------------------------------------------
$rows = 5
$saved = $null
if ($Stage -ne "probe" -and $Stage -ne "probe2") {
    # The queue comes from settings.json: keep the owner's copy and put it back.
    $settings = Join-Path (Get-Location) "settings.json"
    if (Test-Path $settings) {
        $saved = "$settings.f193bak"
        Copy-Item $settings $saved -Force
        # One `"bvid"` per queue entry; `ConvertFrom-Json` is not reliable on
        # this file under Windows PowerShell 5.1, and the count is all we need.
        $rows = ([regex]::Matches((Get-Content $settings -Raw), '"bvid"')).Count
        Write-Host "settings playlist rows=$rows (backup $saved)"
    }
}

$logPath = if ($Log) { $Log } else { Join-Path $OutDir ("f193_{0}.log" -f $Stage) }
$proc = Start-App -Path $Exe -LogPath $logPath
$p = [F193Point]::new()
[void][F193Win]::GetCursorPos([ref]$p)
$cursor0 = @($p.X, $p.Y)

try {
    Start-Sleep -Milliseconds $BootMs
    $main = Get-MainWindow -Name ([System.IO.Path]::GetFileNameWithoutExtension($Exe))
    $rect = [F193Win]::ClientOnScreen($main.Hwnd)
    $top = [F193Win]::Raise($main.Hwnd)
    Write-Host "raised=$top hwnd=0x$($main.Hwnd.ToInt64().ToString('x')) rect=$($rect -join ',')"
    Start-Sleep -Milliseconds 600
    $scale = Get-Scale $rect
    $barY = $WinH - $BarH - $BarBottomGap
    $discX = $WinW / 2.0
    $discY = $barY + $BarH / 2.0
    $panel = Get-PanelBox -Rows $rows
    $clear = Get-ClearButtonCenter -Panel $panel
    Write-Host ("scale={0:N3} disc=({1:N0},{2:N0}) panel=({3:N0},{4:N0},{5:N0}x{6:N0}) clear=({7:N0},{8:N0})" -f `
        $scale, $discX, $discY, $panel.X, $panel.Y, $panel.W, $panel.H, $clear.X, $clear.Y)

    if ($Stage -eq "probe") {
        Save-Shot $main.Hwnd "f193_probe.png"
    }

    if ($Stage -eq "probe2") {
        # Two disc visits with an away in between: the second hover-open has to
        # fire exactly like the first.
        [void](Read-NewLog)
        Move-To $rect $scale $discX $discY
        [void](Wait-Log "playlist hover open")
        [void](Wait-Log "playlist hover disc=0")
        Move-To $rect $scale 640 120
        [void](Wait-Log "playlist hover close")
        Move-To $rect $scale $discX $discY
        $p0 = [F193Point]::new()
        [void][F193Win]::GetCursorPos([ref]$p0)
        Write-Host ("cursor actual={0},{1}" -f $p0.X, $p0.Y)
        [void](Wait-Log "playlist hover open")
        Save-Shot $main.Hwnd "f193_probe2.png"
    }

    if ($Stage -eq "hover" -or $Stage -eq "clear" -or $Stage -eq "regress") {
        [void](Read-NewLog)
        # 1) pointer onto the disc -> 150ms dwell -> panel opens by itself.
        Move-To $rect $scale $discX $discY
        [void](Wait-Log "playlist hover open")
        Save-Shot $main.Hwnd "f193_hover_open.png"
        # 2) into the panel: it must STAY open (no close line while it sits here).
        Move-To $rect $scale 640 470
        Start-Sleep -Milliseconds 700
        $stayed = -not ((Read-NewLog) -match "hover close")
        Write-Host "panel stayed open while hovering it: $stayed"
        # 3) away from both -> 300ms grace -> closed.
        Move-To $rect $scale 640 120
        [void](Wait-Log "playlist hover close")
        Write-Host ("close receipt: {0}" -f $script:lastMatch)
        Save-Shot $main.Hwnd "f193_hover_away.png"
    }

    if ($Stage -eq "clear") {
        [void](Read-NewLog)
        # 4) back onto the disc: the panel reopens; then onto the clear button.
        Move-To $rect $scale $discX $discY
        [void](Wait-Log "playlist hover open")
        Move-To $rect $scale $clear.X $clear.Y
        Start-Sleep -Milliseconds 400
        Save-Shot $main.Hwnd "f193_clear_button.png"
        # 5) first press arms 确认清空 (4s), it must NOT clear.
        [void](Read-NewLog)
        [F193Win]::Click()
        Start-Sleep -Milliseconds 400
        Save-Shot $main.Hwnd "f193_clear_armed.png"
        $cleared = @(Read-NewLog | Where-Object { $_ -match "playlist clear" })
        Write-Host ("first press cleared={0} (must be 0)" -f $cleared.Count)
        # 6) the arm expires by itself after 4s: the label is 清空 again and the
        # queue is untouched.
        Start-Sleep -Milliseconds 4200
        Save-Shot $main.Hwnd "f193_clear_disarmed.png"
        $cleared = @(Read-NewLog | Where-Object { $_ -match "playlist clear" })
        Write-Host ("4s disarm cleared={0} (must be 0)" -f $cleared.Count)
        # 7) arm + confirm: the queue is gone (empty state, disc cleared).
        # Someone using the real mouse during the run can walk the pointer off
        # the button between the two presses, so the pair is retried (an already
        # armed button clears on the FIRST press of the retry).
        $clearedOk = $false
        for ($try = 0; $try -lt 3 -and -not $clearedOk; $try++) {
            [void](Read-NewLog)
            Move-To $rect $scale $clear.X $clear.Y
            Start-Sleep -Milliseconds 180
            [F193Win]::Click()
            Start-Sleep -Milliseconds 280
            [F193Win]::Click()
            $clearedOk = Wait-Log "playlist clear removed=" -TimeoutMs 2500
            if (-not $clearedOk -and -not (Get-PlaylistOpen)) {
                Move-To $rect $scale $discX $discY
                [void](Wait-Log "playlist hover open")
            }
        }
        Write-Host "clear confirmed: $clearedOk"
        # The empty panel is only 230px tall (top at 460), so the pointer has to
        # move INTO it right away or the grace timer closes it under the shot.
        $empty = Get-PanelBox -Rows 0
        Move-To $rect $scale 640 ($empty.Y + 140)
        Start-Sleep -Milliseconds 400
        Save-Shot $main.Hwnd "f193_cleared.png"
        # 8) regression: the pointer is already inside the empty panel - a wheel
        # over it must not close anything.
        [void](Read-NewLog)
        Move-To $rect $scale 640 ($empty.Y + 140)
        [F193Win]::Scroll(2)
        Start-Sleep -Milliseconds 400
        Save-Shot $main.Hwnd "f193_wheel.png"
        $closed = @(Read-NewLog | Where-Object { $_ -match "hover close" })
        Write-Host ("wheel closed the panel={0} (must be 0)" -f $closed.Count)
        # 9) click regression: the disc click still toggles both ways, and a
        # click-CLOSE is not undone by the dwell (pointer stays on the disc).
        [void](Read-NewLog)
        Move-To $rect $scale $discX $discY
        $open = Get-PlaylistOpen
        [F193Win]::Click()
        [void](Wait-Log ("playlist open source=click open=" + [int](-not $open)))
        if ($open) {
            # That click closed it: the dwell must NOT reopen under the still
            # pointer, and the open shot is the next click.
            Start-Sleep -Milliseconds 700
            $reopen = @(Read-NewLog | Where-Object { $_ -match "playlist hover open" })
            Write-Host ("click-close reopened by dwell={0} (must be 0)" -f $reopen.Count)
            Save-Shot $main.Hwnd "f193_click_closed.png"
            [F193Win]::Click()
            [void](Wait-Log "playlist open source=click open=1")
            Save-Shot $main.Hwnd "f193_click_open.png"
        } else {
            Save-Shot $main.Hwnd "f193_click_open.png"
            [void](Read-NewLog)
            [F193Win]::Click()
            [void](Wait-Log "playlist open source=click open=0")
            Start-Sleep -Milliseconds 700
            $reopen = @(Read-NewLog | Where-Object { $_ -match "playlist hover open" })
            Write-Host ("click-close reopened by dwell={0} (must be 0)" -f $reopen.Count)
            Save-Shot $main.Hwnd "f193_click_closed.png"
        }
    }

    if ($Stage -eq "regress") {
        # Row click / ✕ delete must behave exactly as before: the window-level
        # hover test adds no input surface, so this is the proof.
        [void](Read-NewLog)
        Move-To $rect $scale $discX $discY
        [void](Wait-Log "playlist hover open")
        # Row 1's ✕: list starts 51px below the panel top, rows are 68.4 tall,
        # the ✕ is a 22px disc at (width-30, 21.2) inside the 4px list padding.
        $rowX = $panel.X + $panel.W - 4.0 - 19.0
        $rowY = $panel.Y + 51.0 + 32.2
        Move-To $rect $scale $rowX $rowY
        Start-Sleep -Milliseconds 350
        Save-Shot $main.Hwnd "f193_row_x.png"
        [void](Read-NewLog)
        [F193Win]::Click()
        [void](Wait-Log "playlist remove|playlist publish items=4")
        Start-Sleep -Milliseconds 350
        Save-Shot $main.Hwnd "f193_row_removed.png"
        # Row 1's body (100px in from the left edge) plays it. The panel is one
        # row shorter now, so its box is re-derived instead of reused.
        $panel4 = Get-PanelBox -Rows 4
        $bodyX = $panel4.X + 120.0
        $bodyY = $panel4.Y + 51.0 + 34.0
        Move-To $rect $scale $bodyX $bodyY
        [void](Read-NewLog)
        [F193Win]::Click()
        [void](Wait-Log "player open mode=|card open index=")
        Start-Sleep -Milliseconds 500
        Save-TailOnly
    }

    Show-Tail 60
}
finally {
    [void][F193Win]::SetCursorPos($cursor0[0], $cursor0[1])
    if (-not $KeepRunning) {
        Get-Process -Id $proc.Id -ErrorAction SilentlyContinue | Stop-Process -Force
        Start-Sleep -Milliseconds 800
    }
    if ($saved -and (Test-Path $saved)) {
        $settings = Join-Path (Get-Location) "settings.json"
        Copy-Item $saved $settings -Force
        Remove-Item $saved -Force
        Write-Host "settings.json restored"
    }
}
