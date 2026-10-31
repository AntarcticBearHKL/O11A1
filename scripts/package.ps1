# Builds the portable distribution folder for O11A1.
#
# The folder is meant to run on a machine that has nothing but stock Windows:
# every non-system DLL, every external tool and every model is copied in, and
# the audit at the end fails the build if an import cannot be resolved from the
# folder or from Windows itself.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/package.ps1
#   ... -WithModels          also copy models\ (whisper, ~929 MB)
#   ... -Zip                 also write dist\O11A1-<version>.zip
#   ... -SkipBuild           reuse target\dist\release\o11a1.exe
#
param(
    [string]$OutDir = "dist\O11A1",
    [switch]$WithModels,
    [switch]$SkipBuild,
    [switch]$Zip,
    [switch]$DynamicCrt,
    [switch]$WithCuda
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Step($text) { Write-Host "== $text" -ForegroundColor Cyan }
function Fail($text) { Write-Host "!! $text" -ForegroundColor Red; exit 1 }

$out = if ([IO.Path]::IsPathRooted($OutDir)) { $OutDir } else { Join-Path $root $OutDir }

# ---------------------------------------------------------------- build
$distTarget = Join-Path $root "target\dist"
if (-not $SkipBuild) {
    Step "cargo build --release (static CRT -> $distTarget)"
    $env:CARGO_TARGET_DIR = $distTarget
    if (-not $DynamicCrt) {
        # No VCRUNTIME140.dll / api-ms-win-crt-* imports: the machine is assumed
        # to have no VC++ redistributable installed.
        $env:RUSTFLAGS = "-C target-feature=+crt-static"
    }
    & cargo build --release --bin o11a1
    if ($LASTEXITCODE -ne 0) { Fail "cargo build failed" }
}

$exe = Join-Path $distTarget "release\o11a1.exe"
if (-not (Test-Path $exe)) { Fail "missing $exe" }
if (Get-Process o11a1 -ErrorAction SilentlyContinue) { Fail "o11a1.exe is running; close it first (the file is locked)" }

# ---------------------------------------------------------------- layout
Step "staging $out"
if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force -Path $out | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $out "bin") | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $out "licenses") | Out-Null

Copy-Item $exe (Join-Path $out "O11A1.exe") -Force

# ffmpeg / ffprobe: gyan "essentials" builds are fully static (system DLLs only),
# so they need nothing beside them.
$ffSrc = $env:BILI_FFMPEG_DIR
if (-not $ffSrc) {
    $candidates = @("C:\Programs\ffmpeg-unpack\bin", (Join-Path $root "bin"))
    $ffSrc = $candidates | Where-Object { Test-Path (Join-Path $_ "ffmpeg.exe") } | Select-Object -First 1
}
if (-not $ffSrc) { Fail "ffmpeg.exe not found; set BILI_FFMPEG_DIR" }
foreach ($tool in @("ffmpeg.exe", "ffprobe.exe")) {
    $src = Join-Path $ffSrc $tool
    if (Test-Path $src) { Copy-Item $src (Join-Path $out "bin\$tool") -Force }
    else { Write-Host "   (skip $tool)" -ForegroundColor Yellow }
}
Copy-Item (Join-Path $root "assets\fonts\OFL-License.txt") (Join-Path $out "licenses\OFL-Fonts.txt") -Force -ErrorAction SilentlyContinue

# whisper: exe + ggml/whisper dlls, and the MSVC runtime it needs.
#
# The CUDA build's `ggml.dll` imports `ggml-cuda.dll` statically, and that one
# needs the CUDA 12 runtime (cudart + cublas + cublasLt ≈ 770 MB). A machine
# without the toolkit therefore cannot load it at all, so the portable package
# ships the CPU build (`scripts/build-whisper-cpu.ps1`). `-WithCuda` ships the
# CUDA build plus the runtime instead.
$wSrc = if ($WithCuda) { Join-Path $root "tools\whisper-cli" } else { Join-Path $root "tools\whisper-cli-cpu" }
if (-not (Test-Path $wSrc)) {
    if ($WithCuda) { Fail "missing $wSrc (run scripts/build-whisper-cuda.ps1)" }
    Fail "missing $wSrc (run scripts/build-whisper-cpu.ps1 first)"
}
Step "whisper-cli ($(if ($WithCuda) { 'cuda' } else { 'cpu' }))"
$wDst = Join-Path $out "tools\whisper-cli"
New-Item -ItemType Directory -Force -Path $wDst | Out-Null
Get-ChildItem $wSrc -File | ForEach-Object { Copy-Item $_.FullName $wDst -Force }

if ($WithCuda) {
    $cudaBin = $null
    $kit = Join-Path $env:ProgramFiles "NVIDIA GPU Computing Toolkit\CUDA"
    if (Test-Path $kit) {
        $cudaBin = Get-ChildItem $kit -Directory |
            Sort-Object Name -Descending |
            ForEach-Object { Join-Path $_.FullName "bin" } |
            Where-Object { Test-Path (Join-Path $_ "cudart64_12.dll") } |
            Select-Object -First 1
    }
    if (-not $cudaBin) { Fail "-WithCuda: CUDA 12 runtime not found (install the CUDA toolkit)" }
    foreach ($dll in @("cudart64_12.dll", "cublas64_12.dll", "cublasLt64_12.dll")) {
        $src = Join-Path $cudaBin $dll
        if (Test-Path $src) { Copy-Item $src $wDst -Force }
        else { Write-Host "   (skip $dll)" -ForegroundColor Yellow }
    }
}

if ($WithModels) {
    Step "models"
    Copy-Item (Join-Path $root "models") (Join-Path $out "models") -Recurse -Force
}

# ---------------------------------------------------------------- dependency audit
# Every DLL an executable in the folder imports must be either part of Windows or
# copied into the folder. Anything else is a hard failure: the target machine has
# no developer box to fall back on.
$windowsDlls = @(
    # always present on Windows 10/11
    "kernel32.dll","user32.dll","gdi32.dll","advapi32.dll","shell32.dll","shlwapi.dll",
    "ole32.dll","oleaut32.dll","combase.dll","comctl32.dll","comdlg32.dll","uxtheme.dll",
    "dwmapi.dll","dwrite.dll","d2d1.dll","d3d11.dll","d3d12.dll","dcomp.dll","dxgi.dll",
    "d3dcompiler_47.dll","opengl32.dll","winmm.dll","msvcrt.dll","ntdll.dll","ws2_32.dll",
    "iphlpapi.dll","crypt32.dll","bcrypt.dll","bcryptprimitives.dll","ncrypt.dll",
    "setupapi.dll","version.dll","imm32.dll","uiautomationcore.dll","avicap32.dll",
    "secur32.dll","winhttp.dll","dnsapi.dll","userenv.dll","psapi.dll","powrprof.dll",
    "shcore.dll","dbghelp.dll","propsys.dll","wintrust.dll","mswsock.dll","dwmapi.dll",
    "ntdll.dll","avrt.dll","mfplat.dll","mf.dll","strmiids.dll","ksuser.dll","wldap32.dll",
    "api-ms-win-core-synch-l1-2-0.dll","api-ms-win-core-winrt-error-l1-1-0.dll",
    "api-ms-win-core-path-l1-1-0.dll","api-ms-win-crt-runtime-l1-1-0.dll",
    "api-ms-win-crt-heap-l1-1-0.dll","api-ms-win-crt-string-l1-1-0.dll",
    "api-ms-win-crt-stdio-l1-1-0.dll","api-ms-win-crt-math-l1-1-0.dll",
    "api-ms-win-crt-time-l1-1-0.dll","api-ms-win-crt-utility-l1-1-0.dll",
    "api-ms-win-crt-locale-l1-1-0.dll","api-ms-win-crt-convert-l1-1-0.dll",
    "api-ms-win-crt-environment-l1-1-0.dll","api-ms-win-crt-filesystem-l1-1-0.dll",
    "api-ms-win-eventing-provider-l1-1-0.dll","api-ms-win-appmodel-runtime-l1-1-1.dll",
    # supplied by the GPU driver, never by us: copying these into the folder
    # would pin an old driver API next to a newer driver
    "nvcuda.dll","nvml.dll","nvapi64.dll","nvapi.dll","dxcore.dll","opengl32.dll"
)

Add-Type -TypeDefinition @"
using System;
using System.Text;
using System.Collections.Generic;
public static class PeImports {
    public static List<string> Of(string path) {
        var d = System.IO.File.ReadAllBytes(path);
        int pe = BitConverter.ToInt32(d, 0x3C);
        int nsec = BitConverter.ToUInt16(d, pe + 6);
        int optSize = BitConverter.ToUInt16(d, pe + 0x14);
        int magic = BitConverter.ToUInt16(d, pe + 0x18);
        int dd = (magic == 0x20B) ? pe + 0x18 + 112 : pe + 0x18 + 96;
        int impRva = BitConverter.ToInt32(d, dd + 8);
        int sec = pe + 0x18 + optSize;
        var map = new Dictionary<int, int[]>();
        for (int i = 0; i < nsec; i++) {
            int s = sec + i * 40;
            int vsz = BitConverter.ToInt32(d, s + 8);
            int va = BitConverter.ToInt32(d, s + 12);
            int raw = BitConverter.ToInt32(d, s + 20);
            map[va] = new[] { vsz, raw };
        }
        Func<int, int> r2o = rva => {
            foreach (var kv in map) {
                if (rva >= kv.Key && rva < kv.Key + kv.Value[0]) return kv.Value[1] + (rva - kv.Key);
            }
            return 0;
        };
        var names = new List<string>();
        int o = r2o(impRva);
        while (o > 0) {
            int nameRva = BitConverter.ToInt32(d, o + 12);
            if (nameRva == 0) break;
            int no = r2o(nameRva);
            if (no == 0) break;
            int e = no;
            while (d[e] != 0) e++;
            names.Add(Encoding.ASCII.GetString(d, no, e - no));
            o += 20;
        }
        return names;
    }
}
"@ -ErrorAction Stop

Step "dependency audit"
$bundled = @{}
Get-ChildItem $out -Recurse -Include *.exe, *.dll | ForEach-Object { $bundled[$_.Name.ToLower()] = $_.FullName }

$missing = @()
foreach ($file in (Get-ChildItem $out -Recurse -Include *.exe, *.dll)) {
    foreach ($import in [PeImports]::Of($file.FullName)) {
        $lower = $import.ToLower()
        if ($bundled.ContainsKey($lower)) { continue }
        if ($windowsDlls -contains $lower) { continue }
        # Not Windows, not bundled: take it app-local from the build machine, and
        # only fail when even that is impossible.
        $system = Join-Path $env:WINDIR "System32\$import"
        if (Test-Path $system) {
            $dest = Join-Path (Split-Path $file.FullName) $import
            Copy-Item $system $dest -Force
            $bundled[$lower] = $dest
            Write-Host "   bundled $import (from System32) for $($file.Name)" -ForegroundColor Yellow
        } else {
            $missing += "$($file.Name) -> $import"
        }
    }
}
if ($missing.Count -gt 0) {
    $missing | ForEach-Object { Write-Host "   MISSING $_" -ForegroundColor Red }
    Fail "unresolved imports"
}

# ---------------------------------------------------------------- readme
$version = "unknown"
if (Test-Path (Join-Path $root "Cargo.toml")) {
    $line = Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version\s*=\s*"(.*)"' | Select-Object -First 1
    if ($line) { $version = $line.Matches[0].Groups[1].Value }
}
$readme = @"
O11A1 $version — 便携版

这是解压即用的完整目录，不依赖 .NET、VC++ 运行库或任何外部程序。

  O11A1.exe            主程序
  bin\ffmpeg.exe       解码 / 转码（静态构建，无附加依赖）
  bin\ffprobe.exe      媒体探测
  tools\whisper-cli\   「养鱼」页的本地语音识别（CPU 版）
  models\              识别模型（如随包提供，约 900 MB）
  licenses\            字体与第三方许可

首次运行会在本目录旁创建配置与缓存：
  settings.json  accounts.json  fish.db  continue.json  cache\

常用环境变量（一般不需要）：
  BILI_FFMPEG_DIR     指定 ffmpeg 所在目录（默认 bin\）
  SLINT_BACKEND       渲染后端，默认 femtovg-wgpu
  BILI_PLAYER_DCOMP   1 = 启用 DirectComposition 播放器路径（可选）

系统要求：Windows 10 1809 或更新（x64）。GPU 建议支持 D3D11。
"@
Set-Content -Path (Join-Path $out "README.txt") -Value $readme -Encoding UTF8

# ---------------------------------------------------------------- report
Step "contents"
Get-ChildItem $out -Recurse -File |
    Group-Object { $_.Directory.FullName.Replace($out, "") } |
    ForEach-Object {
        $size = ($_.Group | Measure-Object Length -Sum).Sum / 1MB
        Write-Host ("   {0,-28} {1,6:N1} MB  {2} files" -f ($_.Name.TrimStart('\') + '\'), $size, $_.Count)
    }
$total = (Get-ChildItem $out -Recurse -File | Measure-Object Length -Sum).Sum / 1MB
Write-Host ("   TOTAL {0:N1} MB" -f $total) -ForegroundColor Green

if ($Zip) {
    $zipPath = Join-Path $root ("dist\O11A1-$version-win64.zip")
    if (Test-Path $zipPath) { Remove-Item $zipPath -Force }
    Step "zip $zipPath"
    Compress-Archive -Path (Join-Path $out "*") -DestinationPath $zipPath -CompressionLevel Optimal
    Write-Host ("   {0:N1} MB" -f ((Get-Item $zipPath).Length / 1MB)) -ForegroundColor Green
}

Write-Host "OK -> $out" -ForegroundColor Green
