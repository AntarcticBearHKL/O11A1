# QA helper for f208: the Media Foundation hardware decoder, end to end.
#
#   powershell -File scripts/qa-f208-mf.ps1
#   powershell -File scripts/qa-f208-mf.ps1 -Bvid BV1BBnb6aEww -Seconds 10
#   powershell -File scripts/qa-f208-mf.ps1 -Url "https://..." -Cookie "SESSDATA=..."
#   powershell -File scripts/qa-f208-mf.ps1 -Download    # also keep sample.mp4
#
# What it does:
#
# 1. asks the *public* playurl endpoint for a DASH video URL for `-Bvid`
#    (`x/web-interface/view` for the cid, then `x/player/playurl`), so no URL
#    that expires is stored anywhere in the repo;
# 2. writes the header map that URL needs (`Referer`, `Origin`, `User-Agent`,
#    and the saved account's cookies from `accounts.json` / `token.json` -
#    1080P tiers need them, but an expired session still caps at 480P, which is
#    what the probe then measures) to `target/mf-probe-f208/headers.json`;
# 3. builds `examples/mf_probe` and runs it for `-Seconds` of wall clock, then
#    prints the probe's numbers: frames/s, D3D11 texture ratio, subresource
#    spread, ReadSample p50/p95, the seek result and the zero-upload line.
#
# The probe exits non-zero when the machine has no hardware decoder for the
# stream. That is a *finding*, not a script bug: the last `mf result error=`
# line says which failure it was (no HEVC decoder, CPU buffer, reset refused).
param(
    [string]$Bvid = "BV1BBnb6aEww",
    [string]$Url = "",
    [int]$Qn = 80,
    [int]$Seconds = 10,
    [string]$Cookie = "",
    [string]$OutDir = "target/mf-probe-f208",
    [switch]$Download
)

$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$root = Split-Path -Parent $PSScriptRoot
$OutDir = if ([System.IO.Path]::IsPathRooted($OutDir)) { $OutDir } else { Join-Path $root $OutDir }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$referer = "https://www.bilibili.com"
$userAgent = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"

# B 站 answers 412 to a bare API request: the first request to the site sets
# `buvid3`/`b_nut`, and every later call in the same session is accepted. This
# is the whole reason the script keeps a WebRequestSession instead of calling
# `Invoke-RestMethod` bare.
$session = New-Object Microsoft.PowerShell.Commands.WebRequestSession
try {
    $null = Invoke-WebRequest -Uri $referer -UserAgent $userAgent -WebSession $session -TimeoutSec 20
} catch {
    Write-Host ("f208 warmup: {0}" -f $_.Exception.Message)
}

function Get-Json($uri, $headers) {
    try {
        return Invoke-RestMethod -Uri $uri -Headers $headers -WebSession $session -TimeoutSec 20
    } catch {
        throw "请求失败 $uri : $($_.Exception.Message)"
    }
}

if (-not $Cookie) {
    # The app's saved account (accounts.json is what the login flow writes;
    # token.json is the older single-account file). 1080P tiers need SESSDATA.
    $cookieSources = @((Join-Path $root "accounts.json"), (Join-Path $root "token.json"))
    foreach ($source in $cookieSources) {
        if (-not (Test-Path $source)) { continue }
        try {
            $saved = Get-Content -Raw $source | ConvertFrom-Json
            $cookies = if ($saved.cookies) { $saved.cookies } else { $saved.accounts[0].cookies }
            if ($cookies) {
                $Cookie = ($cookies | Where-Object { $_.name -and $_.value } |
                    ForEach-Object { "$($_.name)=$($_.value)" }) -join "; "
                if ($Cookie) { break }
            }
        } catch {
            Write-Host ("f208 cookie source skipped {0}: {1}" -f $source, $_.Exception.Message)
        }
    }
}

if (-not $Url) {
    $headers = @{ Referer = $referer; "User-Agent" = $userAgent }
    if ($Cookie) { $headers["Cookie"] = $Cookie }

    $view = Get-Json "https://api.bilibili.com/x/web-interface/view?bvid=$Bvid" $headers
    if ($view.code -ne 0) { throw "view 失败 bvid=$Bvid code=$($view.code) $($view.message)" }
    $cid = [int64]$view.data.cid
    if ($cid -le 0 -and $view.data.pages) { $cid = [int64]$view.data.pages[0].cid }
    if ($cid -le 0) { throw "bvid=$Bvid 没有可用 cid" }

    $playurl = "https://api.bilibili.com/x/player/playurl?bvid=$Bvid&cid=$cid&qn=$Qn&fnval=4048&fnver=0&fourk=1"
    $play = Get-Json $playurl $headers
    if ($play.code -ne 0) { throw "playurl 失败 bvid=$Bvid cid=$cid code=$($play.code) $($play.message)" }
    $videos = $play.data.dash.video
    if (-not $videos) {
        throw "playurl 没有 DASH 视频轨（该视频可能是 durl/付费内容）；试试 -Cookie 或更低的 -Qn"
    }
    # Highest bandwidth first: the probe measures decode, and the 1080p stream
    # is the one whose 8.3 MB/frame upload this work is meant to remove.
    $track = $videos | Sort-Object bandwidth -Descending | Select-Object -First 1
    $Url = $track.baseUrl
    if (-not $Url) { $Url = $track.base_url }
    if (-not $Url) { throw "playurl 视频轨没有 baseUrl" }

    $hostName = ([uri]$Url).Host
    Write-Host ("f208 url bvid={0} cid={1} id={2} codec={3} {4}x{5} bandwidth={6} host={7}" -f `
            $Bvid, $cid, $track.id, $track.codecid, $track.width, $track.height, $track.bandwidth, $hostName)
} else {
    Write-Host ("f208 url external host={0} len={1}" -f ([uri]$Url).Host, $Url.Length)
}

$headersPath = Join-Path $OutDir "headers.json"
# The signed URL expires (about two hours); keeping it next to the log lets a
# failed probe be re-run with `-Url (Get-Content ...)` without another playurl
# call. Not hardcoded anywhere.
$urlPath = Join-Path $OutDir "url.txt"
[System.IO.File]::WriteAllText($urlPath, $Url, (New-Object System.Text.UTF8Encoding($false)))
$headerMap = [ordered]@{
    "Referer"    = $referer
    "Origin"     = $referer
    "User-Agent" = $userAgent
}
if ($Cookie) { $headerMap["Cookie"] = $Cookie }
# UTF-8 *without* BOM: serde_json rejects a leading BOM.
$headerJson = $headerMap | ConvertTo-Json
[System.IO.File]::WriteAllText($headersPath, $headerJson, (New-Object System.Text.UTF8Encoding($false)))

# `-Download` caches the stream next to the log: the same signed URL then works
# for repeat probe runs (and for a local `MFCreateSourceReaderFromURL` check)
# without another playurl call.
if ($Download) {
    $samplePath = Join-Path $OutDir "sample.mp4"
    Write-Host ("f208 download {0} -> {1}" -f ([uri]$Url).Host, $samplePath)
    Invoke-WebRequest -Uri $Url -Headers $headerMap -OutFile $samplePath -TimeoutSec 300
    Write-Host ("f208 downloaded bytes={0}" -f (Get-Item $samplePath).Length)
}
Write-Host ("f208 headers={0} cookies={1}" -f $headersPath, [bool]$Cookie)

$logPath = Join-Path $OutDir "mf-probe.log"
$buildLog = Join-Path $OutDir "cargo-build.log"
$probeExe = Join-Path $root "target/release/examples/mf_probe.exe"
Push-Location $root
try {
    # Build first and run the binary directly: `cargo run` writes progress to
    # stderr, which would land in the probe log (and PowerShell turns a native
    # command's stderr into an ErrorRecord, which `Stop` would abort on).
    $buildOutput = & cmd.exe /c "cargo build --release --example mf_probe 2>&1"
    $buildOutput | Out-File -Encoding utf8 $buildLog
    if ($LASTEXITCODE -ne 0) {
        $buildOutput | Select-Object -Last 20
        throw "cargo build --release --example mf_probe 失败"
    }
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    & $probeExe $Url $headersPath $Seconds 2>&1 | Tee-Object -FilePath $logPath
    $probeExit = $LASTEXITCODE
    $ErrorActionPreference = $previous
} finally {
    Pop-Location
}

$log = Get-Content -Raw $logPath
function Pick($pattern) {
    $match = [regex]::Match($log, $pattern)
    if ($match.Success) { return $match.Groups[1].Value }
    return $null
}

$frames = Pick "mf decode frames=(\d+)"
$fps = Pick "mf decode .*? fps=([\d.]+)"
$textures = Pick "d3d11_textures=(\d+)"
$cpu = Pick "cpu_buffers=(\d+)"
$p50 = Pick "p50_us=([\d.]+)"
$p95 = Pick "p95_us=([\d.]+)"
$eos = Pick "eos=(\d)"
$maps = Pick "map_calls=(\d+)"

Write-Host ""
Write-Host "f208 summary"
Write-Host ("  frames={0} fps={1} d3d11_textures={2} cpu_buffers={3} eos={4}" -f $frames, $fps, $textures, $cpu, $eos)
Write-Host ("  readsample p50_us={0} p95_us={1}" -f $p50, $p95)
Write-Host ("  map_calls={0} bytes_to_upper_layer=0" -f $maps)
Write-Host ("  log={0}" -f $logPath)

$failed = $false
if ($probeExit -ne 0) {
    $reason = Pick "mf result error=(.+)"
    Write-Host ("FAIL probe exit={0} reason={1}" -f $probeExit, $reason) -ForegroundColor Red
    $failed = $true
} elseif (-not $frames -or [int64]$frames -le 0) {
    Write-Host "FAIL no frames decoded" -ForegroundColor Red
    $failed = $true
} elseif ($textures -ne $frames) {
    Write-Host ("FAIL {0}/{1} pictures were D3D11 textures" -f $textures, $frames) -ForegroundColor Red
    $failed = $true
} elseif ($cpu -ne "0") {
    Write-Host ("FAIL {0} CPU buffers (hardware decode not in use)" -f $cpu) -ForegroundColor Red
    $failed = $true
} elseif ($maps -ne "0") {
    Write-Host ("FAIL map_calls={0} (a CPU map happened)" -f $maps) -ForegroundColor Red
    $failed = $true
} else {
    Write-Host "PASS every picture was a D3D11 texture, zero CPU uploads" -ForegroundColor Green
}

if ($failed) { exit 1 }
exit 0
