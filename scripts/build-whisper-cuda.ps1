# Build whisper.cpp with the CUDA backend and install the runnable bits into
# tools/whisper-cli/ (whisper-cli.exe + the ggml/whisper DLLs it loads).
#
# The source tree lives in tools/whisper.cpp and is git-ignored: only this
# script is committed, so a fresh checkout can rebuild the same binary.
#
#   powershell -ExecutionPolicy Bypass -File scripts/build-whisper-cuda.ps1
#
# Requirements: CMake >= 3.14, a CUDA toolkit with nvcc, MSVC (VS 2019+) and
# Ninja (or let CMake pick the Visual Studio generator).

param(
    [string]$Source = (Join-Path $PSScriptRoot "..\tools\whisper.cpp"),
    [string]$OutDir = (Join-Path $PSScriptRoot "..\tools\whisper-cli"),
    # RTX 4060 Ti is sm_89; overriding keeps the CUDA compile short.
    [string]$Arch = "89",
    # Delete the CMake cache first (a cache built with another toolchain breaks).
    [switch]$Clean
)

$ErrorActionPreference = "Stop"
$Source = (Resolve-Path $Source).Path
$OutDir = [System.IO.Path]::GetFullPath($OutDir)

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) { throw "vswhere.exe not found: install Visual Studio with the C++ workload" }
# CUDA 12.9 refuses _MSC_VER >= 1950 (VS 2026 / v145), so prefer a VS 2022
# (v143, _MSC_VER 194x) toolchain when one is installed and fall back to the
# newest only if it is not.
$required = "Microsoft.VisualStudio.Component.VC.Tools.x86.x64"
$vs = & $vswhere -latest -products * -version "[17.0,18.0)" -requires $required -property installationPath
if (-not $vs) {
    $vs = & $vswhere -latest -products * -requires $required -property installationPath
    if ($vs) { Write-Warning "Only a post-2022 MSVC was found: nvcc will reject it unless CUDA is newer than 12.9" }
}
if (-not $vs) { throw "No Visual Studio C++ toolchain found" }
$vcvars = Join-Path $vs "VC\Auxiliary\Build\vcvars64.bat"
if (-not (Test-Path $vcvars)) { throw "vcvars64.bat not found under $vs" }

$nvcc = Get-Command nvcc -ErrorAction SilentlyContinue
if (-not $nvcc) { throw "nvcc not on PATH: install the CUDA toolkit" }

Write-Host "source : $Source"
Write-Host "out    : $OutDir"
Write-Host "vs     : $vs"
Write-Host "nvcc   : $($nvcc.Source)"
Write-Host "arch   : sm_$Arch"

$build = Join-Path $Source "build"
if ($Clean -and (Test-Path $build)) { Remove-Item -Recurse -Force $build }

# Import the developer environment into this process once, so cmake/ninja/cl see
# cl.exe and the Windows SDK exactly as vcvars64 sets them.
cmd.exe /c "call `"$vcvars`" >nul && set" | ForEach-Object {
    if ($_ -match '^([^=]+)=(.*)$') { Set-Item -Path "env:$($matches[1])" -Value $matches[2] }
}
if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) { throw "cl.exe missing after vcvars64" }

& cmake -S $Source -B $build -G Ninja `
    "-DCMAKE_BUILD_TYPE=Release" `
    "-DGGML_CUDA=1" `
    "-DCMAKE_CUDA_ARCHITECTURES=$Arch" `
    "-DWHISPER_BUILD_TESTS=OFF" `
    "-DWHISPER_BUILD_EXAMPLES=ON"
if ($LASTEXITCODE -ne 0) { throw "cmake configure failed with exit code $LASTEXITCODE" }

& cmake --build $build --config Release
if ($LASTEXITCODE -ne 0) { throw "cmake build failed with exit code $LASTEXITCODE" }

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$exe = Join-Path $build "bin\whisper-cli.exe"
if (-not (Test-Path $exe)) { $exe = Get-ChildItem -Path $build -Recurse -Filter whisper-cli.exe | Select-Object -First 1 -ExpandProperty FullName }
if (-not $exe) { throw "whisper-cli.exe was not produced" }
Copy-Item $exe -Destination $OutDir -Force
# Windows resolves DLLs next to the executable, so they travel together.
Get-ChildItem -Path (Split-Path $exe) -Filter *.dll | ForEach-Object { Copy-Item $_.FullName -Destination $OutDir -Force }
Get-ChildItem -Path (Split-Path $exe) -Filter *.dll | ForEach-Object { Write-Host "dll    : $($_.Name)" }
Write-Host "installed: $(Join-Path $OutDir 'whisper-cli.exe')"
