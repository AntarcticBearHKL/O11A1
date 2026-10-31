# QA helper for the f189 "1080p 到 120 fps?" report. Developer tool, not product.
#
#   # one window grab of the largest 16:9 window the app owns (the player)
#   powershell -File scripts/qa-f189.ps1 -Grab target/qa/f189_4x_ofa.png -Caption "OFA 4x (log: out_hz=120)"
#
#   # the labelled来源/混合/BM/OFA montage from the panels a spike wrote
#   powershell -File scripts/qa-f189.ps1 -Montage target/qa
#
# The grab is a GDI screen copy of the window's rectangle rather than
# PrintWindow: the player is a wgpu flip-model surface, which PrintWindow
# returns black for (see the f155 note in src/main.rs). The window is raised
# first, so the pixels are the real presented frame.
param(
    [string]$Grab,
    [string]$Montage,
    [string]$Annotate,
    [string]$Out,
    [string]$Caption = "",
    [string]$ProcessName = "o11a1",
    [int]$WaitMs = 250
)

Add-Type -AssemblyName System.Drawing

if (-not ("F189Win" -as [type])) {
    Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public struct F189Rect { public int Left, Top, Right, Bottom; }
public struct F189Point { public int X, Y; }

public class F189Win {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);

    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out F189Rect rect);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr hWnd, ref F189Point point);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int cmd);
    [DllImport("user32.dll")] public static extern int GetWindowTextLength(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int max);

    public static List<IntPtr> WindowsOf(uint pid) {
        var found = new List<IntPtr>();
        EnumWindows((h, l) => {
            uint owner;
            GetWindowThreadProcessId(h, out owner);
            if (owner != pid || !IsWindowVisible(h)) { return true; }
            F189Rect rect;
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

    // Client rect in screen coordinates: the presented picture, no borders.
    public static int[] ClientOnScreen(IntPtr h) {
        F189Rect r;
        if (!GetClientRect(h, out r)) { return new int[] { 0, 0, 0, 0 }; }
        var origin = new F189Point();
        ClientToScreen(h, ref origin);
        return new int[] { origin.X, origin.Y, r.Right - r.Left, r.Bottom - r.Top };
    }
}
"@
}

function Get-PlayerWindow {
    $proc = Get-Process -Name $ProcessName -ErrorAction SilentlyContinue |
        Sort-Object -Property WorkingSet -Descending | Select-Object -First 1
    if (-not $proc) { throw "no $ProcessName process" }
    $best = $null
    $bestScore = [double]::MaxValue
    foreach ($h in [F189Win]::WindowsOf([uint32]$proc.Id)) {
        $r = [F189Win]::ClientOnScreen($h)
        $w = $r[2]; $hgt = $r[3]
        if ($w -lt 480 -or $hgt -lt 270) { continue }
        $ratio = [double]$w / [double]$hgt
        # The player is the 16:9 window; the main window is 1280x800 (1.6).
        $score = [Math]::Abs($ratio - 16.0 / 9.0)
        Write-Host ("candidate hwnd=0x{0:x} {1}x{2} ratio={3:N3} class={4} title={5}" -f $h.ToInt64(), $w, $hgt, $ratio, [F189Win]::Class($h), [F189Win]::Title($h))
        if ($score -lt $bestScore) { $bestScore = $score; $best = @{ Hwnd = $h; Rect = $r } }
    }
    if (-not $best) { throw "no player-shaped window" }
    if ($bestScore -gt 0.25) { throw ("no 16:9 window (closest ratio score {0:N3})" -f $bestScore) }
    return $best
}

if ($Annotate) {
    $source = [System.IO.Path]::GetFullPath($Annotate)
    $image = [System.Drawing.Image]::FromFile($source)
    $strip = 34
    $canvas = New-Object System.Drawing.Bitmap $image.Width, ($image.Height + $strip)
    $g = [System.Drawing.Graphics]::FromImage($canvas)
    $g.Clear([System.Drawing.Color]::FromArgb(12, 13, 16))
    $g.DrawImage($image, 0, 0)
    $brush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(235, 235, 240))
    # Shrink the caption until it fits the picture width, so nothing is clipped.
    $size = 12.0
    do {
        $font = New-Object System.Drawing.Font 'Microsoft YaHei', $size
        $width = $g.MeasureString($Caption, $font).Width
        if ($width -le ($image.Width - 16) -or $size -le 6.0) { break }
        $font.Dispose()
        $size -= 0.5
    } while ($true)
    $g.DrawString($Caption, $font, $brush, 8, ($image.Height + 7))
    $g.Dispose()
    $image.Dispose()
    $target = if ($Out) { [System.IO.Path]::GetFullPath($Out) } else { $source }
    $canvas.Save($target, [System.Drawing.Imaging.ImageFormat]::Png)
    $canvas.Dispose()
    Write-Host "f189 annotate path=$target"
}

if ($Grab) {
    $player = Get-PlayerWindow
    $rect = $player.Rect
    [void][F189Win]::ShowWindow($player.Hwnd, 9)  # SW_RESTORE
    [void][F189Win]::BringWindowToTop($player.Hwnd)
    [void][F189Win]::SetForegroundWindow($player.Hwnd)
    Start-Sleep -Milliseconds $WaitMs
    if ([F189Win]::GetForegroundWindow() -ne $player.Hwnd) {
        Write-Host "warning: player did not take the foreground"
    }
    $rect = [F189Win]::ClientOnScreen($player.Hwnd)
    $bitmap = New-Object System.Drawing.Bitmap $rect[2], $rect[3]
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.CopyFromScreen($rect[0], $rect[1], 0, 0, $bitmap.Size)
    $graphics.Dispose()
    if ($Caption) {
        # A caption strip appended below the picture: it names the run and the
        # numbers are the log's own, quoted verbatim.
        $strip = 34
        $canvas = New-Object System.Drawing.Bitmap $bitmap.Width, ($bitmap.Height + $strip)
        $g = [System.Drawing.Graphics]::FromImage($canvas)
        $g.Clear([System.Drawing.Color]::FromArgb(12, 13, 16))
        $g.DrawImage($bitmap, 0, 0)
        $font = New-Object System.Drawing.Font 'Microsoft YaHei', 12
        $brush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(235, 235, 240))
        $g.DrawString($Caption, $font, $brush, 8, ($bitmap.Height + 7))
        $g.Dispose()
        $bitmap.Dispose()
        $bitmap = $canvas
    }
    $full = [System.IO.Path]::GetFullPath($Grab)
    $dir = [System.IO.Path]::GetDirectoryName($full)
    if ($dir) { [void][System.IO.Directory]::CreateDirectory($dir) }
    $bitmap.Save($full, [System.Drawing.Imaging.ImageFormat]::Png)
    $bitmap.Dispose()
    Write-Host ("f189 grab path={0} size={1}x{2}" -f $full, $rect[2], $rect[3])
}

if ($Montage) {
    $dir = [System.IO.Path]::GetFullPath($Montage)
    # 2x2 of 960x540 tiles, one caption per panel, 8 px gutters.
    $tileW = 960; $tileH = 540; $capH = 30; $gap = 8
    $outW = $tileW * 2 + $gap
    $outH = ($tileH + $capH) * 2 + $gap * 3
    $canvas = New-Object System.Drawing.Bitmap $outW, $outH
    $g = [System.Drawing.Graphics]::FromImage($canvas)
    $g.Clear([System.Drawing.Color]::FromArgb(12, 13, 16))
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $font = New-Object System.Drawing.Font 'Microsoft YaHei', 12
    $brush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(235, 235, 240))
    $panels = @(
        @{ File = 'f189_src.png'; Caption = 'SOURCE  t=0  (real 1080p30 frame)' },
        @{ File = 'f189_blend_mid.png'; Caption = 'BLEND  t=0.5  (baseline: ghosting)' },
        @{ File = 'f189_bm_mid.png'; Caption = 'BLOCK MATCH  t=0.5  (1080p: measurement path only)' },
        @{ File = 'f189_ofa_mid.png'; Caption = 'OFA  t=0.5  (hardware flow + CPU warp)' }
    )
    for ($i = 0; $i -lt $panels.Count; $i++) {
        $col = $i % 2
        $row = [Math]::Floor($i / 2)
        $x = $gap + $col * ($tileW + $gap)
        $y = $gap + $row * ($tileH + $capH + $gap)
        $path = Join-Path $dir $panels[$i].File
        if (-not (Test-Path $path)) { Write-Host "missing $path"; continue }
        $image = [System.Drawing.Image]::FromFile($path)
        $g.DrawImage($image, $x, $y, $tileW, $tileH)
        $image.Dispose()
        $g.DrawString($panels[$i].Caption, $font, $brush, $x, ($y + $tileH + 6))
    }
    $g.Dispose()
    $target = Join-Path $dir 'f189_frames_compare.png'
    $canvas.Save($target, [System.Drawing.Imaging.ImageFormat]::Png)
    $canvas.Dispose()
    Write-Host ("f189 montage path={0} size={1}x{2}" -f $target, $outW, $outH)
}
