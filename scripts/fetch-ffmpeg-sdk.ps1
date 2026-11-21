$ErrorActionPreference = 'Stop'
# FFmpeg 8.1 shared SDK (headers + import libraries + DLLs) for the in-process
# D3D11VA decoder. The build products are not committed: run this once per
# checkout, or let scripts/build-engine.ps1 call it.
$root = Split-Path $PSScriptRoot -Parent
$archive = Join-Path $root 'native\ffmpeg81\ffmpeg81.zip'
$destination = Join-Path $root 'native\ffmpeg81'
$url = 'https://github.com/FFmpeg-Builds/FFmpeg-Builds/releases/download/latest/ffmpeg-n8.1-latest-win64-gpl-shared-8.1.zip'

$existing = Get-ChildItem $destination -Directory -Filter 'ffmpeg-n8.1-*' -ErrorAction SilentlyContinue |
    Select-Object -First 1
if ($existing -and (Test-Path (Join-Path $existing.FullName 'lib\avcodec.lib'))) {
    Write-Output "ffmpeg sdk -> $($existing.FullName)"
    exit 0
}

New-Item -ItemType Directory -Force $destination | Out-Null
if (-not (Test-Path $archive)) {
    Write-Output "downloading FFmpeg 8.1 shared SDK"
    curl.exe -L --fail --max-time 900 -o $archive $url
    if ($LASTEXITCODE -ne 0) { throw "FFmpeg download failed ($LASTEXITCODE)" }
}
Write-Output "extracting FFmpeg 8.1 shared SDK"
Expand-Archive -Path $archive -DestinationPath $destination -Force
$sdk = Get-ChildItem $destination -Directory -Filter 'ffmpeg-n8.1-*' |
    Select-Object -First 1
if (-not $sdk) { throw 'FFmpeg SDK directory missing after extraction' }
Write-Output "ffmpeg sdk -> $($sdk.FullName)"
