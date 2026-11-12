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

Push-Location $engine
try {
  & $cargo build --release
  if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
} finally {
  Pop-Location
}

New-Item -ItemType Directory -Force $runtime | Out-Null
Copy-Item (Join-Path $target 'release\o11a1-engine.exe') (Join-Path $runtime 'o11a1-engine.exe') -Force
New-Item -ItemType Directory -Force (Join-Path $runtime 'assets\fonts') | Out-Null
$fonts = Join-Path $root 'public\fonts'
Copy-Item (Join-Path $fonts '*.ttf') (Join-Path $runtime 'assets\fonts') -Force
Write-Output "engine -> $(Join-Path $runtime 'o11a1-engine.exe')"
