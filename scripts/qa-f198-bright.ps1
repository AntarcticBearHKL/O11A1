Add-Type -AssemblyName System.Drawing

function Count-Bright {
    param([string]$Path, [int]$X, [int]$Y, [int]$W, [int]$H)
    $b = [System.Drawing.Bitmap]::FromFile($Path)
    $bright = 0
    $yEnd = [Math]::Min($Y + $H, $b.Height)
    $xEnd = [Math]::Min($X + $W, $b.Width)
    for ($y = $Y; $y -lt $yEnd; $y += 2) {
        for ($x = $X; $x -lt $xEnd; $x += 2) {
            $c = $b.GetPixel($x, $y)
            if (($c.R + $c.G + $c.B) -gt 200) { $bright++ }
        }
    }
    $b.Dispose()
    return $bright
}

$dir = "D:\Dev\bilibili-slint\target\qa"
$cases = @(
    @{ File = "f198_ui55.png"; Regions = @(
        @{ N = "above-video"; X = 0; Y = 0; W = 800; H = 170 },
        @{ N = "below-video"; X = 0; Y = 630; W = 800; H = 170 },
        @{ N = "video"; X = 0; Y = 180; W = 800; H = 440 } ) },
    @{ File = "f198_geom_panel.png"; Regions = @(
        @{ N = "above-video"; X = 0; Y = 0; W = 480; H = 260 },
        @{ N = "below-video"; X = 0; Y = 540; W = 480; H = 260 } ) }
)
foreach ($case in $cases) {
    foreach ($r in $case.Regions) {
        $n = Count-Bright -Path (Join-Path $dir $case.File) -X $r.X -Y $r.Y -W $r.W -H $r.H
        "{0} {1}: bright={2}" -f $case.File, $r.N, $n
    }
}
