# f206: the three acceptance shots for the speech-model section, plus a real
# resume of the 466 MB download.
#
#   powershell -File scripts/qa-f206-models.ps1 -Stage empty
#   powershell -File scripts/qa-f206-models.ps1 -Stage downloading   # shot @12s, cancel @18s
#   powershell -File scripts/qa-f206-models.ps1 -Stage resume        # resume to completion
#   powershell -File scripts/qa-f206-models.ps1 -Stage installed     # the real model dir
#   powershell -File scripts/qa-f206-models.ps1 -Cleanup             # delete the temp trees
#
# Every run uses its own `O11A1_HOME` (a temp directory), so the repo's
# settings.json / fish.db are never touched. `-Stage resume` really downloads
# ggml-small.bin and then loads it once in whisper-cli. `-Cleanup` removes it.
#
# Every line printed below comes from the app's own `BILI_DEBUG_LOG`:
# `models dl start|resume|progress|done|cancel`. Developer tool, not product
# code.
#
# NOTE: ASCII only. Windows PowerShell 5.1 reads a BOM-less .ps1 as ANSI, and a
# non-ASCII comment can then swallow the next line.

param(
    [ValidateSet("empty", "hint", "downloading", "resume", "installed")]
    [string]$Stage = "empty",
    [string]$Exe = "target/release/o11a1.exe",
    [string]$OutDir = "docs",
    [string]$Work = "",
    [string]$RealModels = "models/whisper",
    [int]$CancelMs = 18000,
    [int]$ShotMs = 12000,
    [int]$TimeoutS = 600,
    [switch]$Cleanup
)

$ErrorActionPreference = "Stop"
$root = (Resolve-Path -LiteralPath ".").Path
$dir = Join-Path $root $OutDir
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }

if (-not $Work) { $Work = Join-Path $env:TEMP "o11a1-f206" }
$home1 = Join-Path $Work ("home-" + $Stage)
$models = switch ($Stage) {
    "empty" { Join-Path $Work "models-empty" }
    "hint" { Join-Path $Work "models-empty" }
    "installed" { Join-Path $root $RealModels }
    default { Join-Path $Work "models-dl" }
}

if ($Cleanup) {
    foreach ($path in @($home1, (Join-Path $Work "home-empty"), (Join-Path $Work "home-downloading"),
            (Join-Path $Work "home-resume"), (Join-Path $Work "models-empty"), (Join-Path $Work "models-dl"))) {
        if (Test-Path $path) {
            Remove-Item -LiteralPath $path -Recurse -Force
            Write-Host "cleanup $path"
        }
    }
    return
}

if (-not (Test-Path $home1)) { New-Item -ItemType Directory -Path $home1 -Force | Out-Null }
if (-not (Test-Path $models) -and $Stage -ne "installed") {
    New-Item -ItemType Directory -Path $models -Force | Out-Null
}

# A logged-in account, else the window only draws the QR-login card and there is
# no fish page to capture. Only the temp home gets the copy.
$accounts = Join-Path $root "accounts.json"
if ((Test-Path $accounts) -and -not (Test-Path (Join-Path $home1 "accounts.json"))) {
    Copy-Item -LiteralPath $accounts -Destination (Join-Path $home1 "accounts.json")
}

# A minimal settings.json. The engine is pinned to whisper.cpp so the
# "engine is not whisper.cpp" reminder does not cover what the shot is about,
# and the model directory points at this stage's tree.
$settings = [ordered]@{
    fish_asr_enabled   = $false
    fish_asr_engine    = "whisper.cpp"
    fish_asr_device    = "gpu"
    fish_asr_model     = "small"
    fish_asr_threads   = 4
    fish_asr_model_dir = $models
    fish_model_mirror  = "auto"
    page_size          = 24
}
$json = $settings | ConvertTo-Json -Depth 4
# No BOM: serde_json does not skip one, and a BOM would make the whole file
# fall back to defaults (the app would then write its own settings back).
[System.IO.File]::WriteAllText((Join-Path $home1 "settings.json"), $json, (New-Object System.Text.UTF8Encoding($false)))

$tag = "f206_models_$Stage"
$log = Join-Path $dir ("f206_models_" + $Stage + ".log")
if (Test-Path $log) { Remove-Item -LiteralPath $log -Force }

$env:O11A1_HOME = $home1
$env:BILI_DEBUG_LOG = $log
$env:BILI_CAPTURE_DIR = $dir
$spec = switch ($Stage) {
    "hint" { "$tag,eyes=1,shot_ms=$ShotMs" }
    "downloading" { "$tag,dl=small,cancel_ms=$CancelMs,shot_ms=$ShotMs" }
    "resume" { "$tag,dl=small,shot_ms=$ShotMs" }
    default { "$tag,shot_ms=$ShotMs" }
}
$env:BILI_F206_TEST = $spec

Write-Host "stage=$Stage home=$home1 models=$models"
Write-Host "hook=$spec"

$proc = Start-Process -FilePath (Join-Path $root $Exe) -PassThru
$deadline = (Get-Date).AddSeconds($TimeoutS)
$stopped = $false
while (-not $proc.HasExited -and (Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 500
    if (Test-Path $log) {
        $text = Get-Content -LiteralPath $log -Raw -ErrorAction SilentlyContinue
        if (-not $text) { continue }
        # After the cancel / the completion there is nothing left to watch.
        if ($Stage -eq "downloading" -and $text -match "models dl cancel name=") { $stopped = $true; break }
        if ($Stage -eq "resume" -and $text -match "models dl done name=small") { $stopped = $true; break }
    }
}
if (-not $proc.HasExited) {
    Stop-Process -Id $proc.Id -Force
    Start-Sleep -Milliseconds 500
}
Write-Host "stopped=$stopped"

$text = if (Test-Path $log) { Get-Content -LiteralPath $log } else { @() }
$text | Where-Object { $_ -match "^(models dl|models ui|f206 )" } | ForEach-Object { Write-Host $_ }

$captured = Join-Path $dir "$tag.png"
if (Test-Path $captured) {
    $size = (Get-Item -LiteralPath $captured).Length
    Write-Host "png=$captured bytes=$size"
} else {
    Write-Warning "no capture at $captured"
}

$part = Join-Path $models "whisper.cpp/ggml-small.bin.part"
$final = Join-Path $models "whisper.cpp/ggml-small.bin"
foreach ($path in @($part, $final)) {
    if (Test-Path $path) {
        $len = (Get-Item -LiteralPath $path).Length
        Write-Host "$(Split-Path $path -Leaf) bytes=$len"
    }
}

# The finished file has to load in whisper-cli (not just print --help).
if ($Stage -eq "resume" -and (Test-Path $final)) {
    $cli = Join-Path $root "tools/whisper-cli/whisper-cli.exe"
    $wav = Get-ChildItem -LiteralPath (Join-Path $root "cache/fishaudio") -Filter *.wav -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ((Test-Path $cli) -and $wav) {
        $json = Join-Path $Work "f206_cpp"
        Write-Host "whisper-cli load: $cli -m $final -f $($wav.Name)"
        & $cli -m $final -f $wav.FullName -oj -of $json -l auto 2>&1 |
            Where-Object { $_ -match "whisper_init|model|load|error" } |
            Select-Object -First 24 |
            ForEach-Object { Write-Host "  $_" }
        $exit = $LASTEXITCODE
        Write-Host "whisper-cli exit=$exit json=$(Test-Path ($json + '.json'))"
    }
}
