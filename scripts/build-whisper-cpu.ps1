# Build whisper.cpp **without** CUDA and install it into tools/whisper-cli-cpu/.
#
# Why a second build: the CUDA build links the CUDA 12 runtime (cudart + cublas
# + cublasLt ≈ 770 MB) and `ggml.dll` imports `ggml-cuda.dll` statically, so that
# build cannot run on a machine without the CUDA runtime. The portable package
# ships this CPU build instead; the CUDA build stays for the development machine
# and for `package.ps1 -WithCuda`.
#
#   powershell -ExecutionPolicy Bypass -File scripts/build-whisper-cpu.ps1
#
# Requirements: CMake >= 3.14, MSVC (VS 2019+) and Ninja (or the VS generator).

param(
    [string]$Source = (Join-Path $PSScriptRoot "..\tools\whisper.cpp"),
    [string]$OutDir = (Join-Path $PSScriptRoot "..\tools\whisper-cli-cpu"),
    [switch]$Clean
)

$ErrorActionPreference = "Stop"
$Source = (Resolve-Path $Source).Path
$OutDir = [System.IO.Path]::GetFullPath($OutDir)

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) { throw "vswhere.exe not found: install Visual Studio with the C++ workload" }
$required = "Microsoft.VisualStudio.Component.VC.Tools.x86.x64"
$vs = & $vswhere -latest -products * -requires $required -property installationPath
if (-not $vs) { throw "No Visual Studio C++ toolchain found" }
$vcvars = Join-Path $vs "VC\Auxiliary\Build\vcvars64.bat"
if (-not (Test-Path $vcvars)) { throw "vcvars64.bat not found under $vs" }

Write-Host "source : $Source"
Write-Host "out    : $OutDir"
Write-Host "vs     : $vs"

$build = Join-Path $Source "build-cpu"
if ($Clean -and (Test-Path $build)) { Remove-Item -Recurse -Force $build }

cmd.exe /c "call `"$vcvars`" >nul && set" | ForEach-Object {
    if ($_ -match '^([^=]+)=(.*)$') { Set-Item -Path "env:$($matches[1])" -Value $matches[2] }
}
if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) { throw "cl.exe missing after vcvars64" }

& cmake -S $Source -B $build -G Ninja `
    "-DCMAKE_BUILD_TYPE=Release" `
    "-DGGML_CUDA=OFF" `
    "-DGGML_VULKAN=OFF" `
    "-DGGML_NATIVE=OFF" `
    "-DWHISPER_BUILD_TESTS=OFF" `
    "-DWHISPER_BUILD_EXAMPLES=ON"
if ($LASTEXITCODE -ne 0) { throw "cmake configure failed with exit code $LASTEXITCODE" }

& cmake --build $build --config Release
if ($LASTEXITCODE -ne 0) { throw "cmake build failed with exit code $LASTEXITCODE" }

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$exe = Join-Path $build "bin\whisper-cli.exe"
if (-not (Test-Path $exe)) {
    $exe = Get-ChildItem -Path $build -Recurse -Filter whisper-cli.exe | Select-Object -First 1 -ExpandProperty FullName
}
if (-not $exe) { throw "whisper-cli.exe was not produced" }
Copy-Item $exe -Destination $OutDir -Force
Get-ChildItem -Path (Split-Path $exe) -Filter *.dll | ForEach-Object {
    Copy-Item $_.FullName -Destination $OutDir -Force
    Write-Host "dll    : $($_.Name)"
}
Write-Host "installed: $(Join-Path $OutDir 'whisper-cli.exe')"
