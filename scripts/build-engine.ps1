$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$engine = Join-Path $root 'engine'
$runtime = Join-Path $root 'runtime'
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'engine-target' }
$cargo = Join-Path $env:USERPROFILE '.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\cargo.exe'

if (-not (Test-Path $cargo)) {
  $cargo = 'cargo'
}
if (-not $env:CARGO_HOME -and (Test-Path 'D:\Dev\.cargo')) { $env:CARGO_HOME = 'D:\Dev\.cargo' }
$env:CARGO_TARGET_DIR = $target

# The native player links libavcodec/libavformat at build time and loads their
# DLLs at runtime. Keep the SDK and the runtime DLL set in one place.
if (-not $env:FFMPEG_DIR) {
  $sdk = Get-ChildItem (Join-Path $root 'native\ffmpeg81') -Directory -Filter 'ffmpeg-n8.1-*' -ErrorAction SilentlyContinue |
    Select-Object -First 1
  if (-not $sdk -or -not (Test-Path (Join-Path $sdk.FullName 'lib\avcodec.lib'))) {
    & (Join-Path $PSScriptRoot 'fetch-ffmpeg-sdk.ps1')
    $sdk = Get-ChildItem (Join-Path $root 'native\ffmpeg81') -Directory -Filter 'ffmpeg-n8.1-*' |
      Select-Object -First 1
  }
  if (-not $sdk) { throw 'FFmpeg 8.1 shared SDK is missing' }
  $env:FFMPEG_DIR = $sdk.FullName
}
$ffmpegSdkBin = Join-Path $env:FFMPEG_DIR 'bin'

Push-Location $engine
try {
  & $cargo build --release
  if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
} finally {
  Pop-Location
}

New-Item -ItemType Directory -Force $runtime | Out-Null
Copy-Item (Join-Path $target 'release\o11a1-engine.exe') (Join-Path $runtime 'o11a1-engine.exe') -Force

# ffmpeg.exe stays as the fallback / audio / probe tool. Its shared DLLs sit
# beside it; Electron prepends this directory to PATH so the loader finds them
# when o11a1-engine starts.
$ffmpegBin = Join-Path $runtime 'ffmpeg\bin'
New-Item -ItemType Directory -Force $ffmpegBin | Out-Null
Copy-Item (Join-Path $ffmpegSdkBin '*.dll') $ffmpegBin -Force
Copy-Item (Join-Path $ffmpegSdkBin 'ffmpeg.exe') $ffmpegBin -Force
Copy-Item (Join-Path $ffmpegSdkBin 'ffprobe.exe') $ffmpegBin -Force

foreach ($dir in @(
    (Join-Path $target 'release'),
    (Join-Path $root 'native\target\release')
  )) {
  if (Test-Path $dir) {
    Copy-Item (Join-Path $ffmpegSdkBin '*.dll') $dir -Force
    $deps = Join-Path $dir 'deps'
    if (Test-Path $deps) {
      Copy-Item (Join-Path $ffmpegSdkBin '*.dll') $deps -Force
    }
  }
}
New-Item -ItemType Directory -Force (Join-Path $runtime 'assets\fonts') | Out-Null
$fonts = Join-Path $root 'public\fonts'
$fontTarget = Join-Path $runtime 'assets\fonts'
# Clear stale faces so an older bundled family can never ship alongside the
# current one.
if (Test-Path $fontTarget) { Remove-Item (Join-Path $fontTarget '*') -Force }
Copy-Item (Join-Path $fonts '*.ttf') $fontTarget -Force
Copy-Item (Join-Path $fonts '*.txt') $fontTarget -Force
Write-Output "engine -> $(Join-Path $runtime 'o11a1-engine.exe')"
