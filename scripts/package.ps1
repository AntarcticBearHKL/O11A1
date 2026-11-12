$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
& (Join-Path $PSScriptRoot 'build-engine.ps1')
if ($LASTEXITCODE -ne 0) { throw 'engine build failed' }

$runtime = Join-Path $root 'runtime'
$ffmpegSource = $env:BILI_FFMPEG_DIR
if (-not $ffmpegSource) {
  $command = Get-Command ffmpeg.exe -ErrorAction SilentlyContinue
  if ($command) { $ffmpegSource = Split-Path $command.Source }
}
if (-not $ffmpegSource) { $ffmpegSource = 'C:\Programs\ffmpeg-unpack\bin' }
$ffmpegTarget = Join-Path $runtime 'ffmpeg\bin'
New-Item -ItemType Directory -Force $ffmpegTarget | Out-Null
foreach ($exe in @('ffmpeg.exe', 'ffprobe.exe')) {
  $source = Join-Path $ffmpegSource $exe
  if (Test-Path $source) {
    Copy-Item $source (Join-Path $ffmpegTarget $exe) -Force
  } else {
    Write-Warning "missing $source (the app will use BILI_FFMPEG_DIR or the system fallback)"
  }
}

Push-Location $root
try {
  npm run build:renderer
  if ($LASTEXITCODE -ne 0) { throw 'renderer build failed' }
  npx electron-builder --win --x64
  if ($LASTEXITCODE -ne 0) { throw 'electron-builder failed' }
} finally {
  Pop-Location
}
Write-Output "installer -> $(Join-Path $root 'release')"
