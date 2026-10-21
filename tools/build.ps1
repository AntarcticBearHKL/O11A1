# Build gate for parallel agent work.
#
# Problem it solves: several agents run `cargo build --release` at the same time.
# Cargo serialises them on target\ lock, so N agents cost one build plus N-1
# agents idling, and the expensive parts (Slint codegen, codegen-units merge,
# link) are paid once per agent instead of once per phase.
#
# Protocol this script enforces:
#   - EDIT PHASE  : agents edit files, no cargo at all.
#   - CHECK PHASE : ONE caller runs  build.ps1 -Mode check
#   - BUILD PHASE : ONE caller runs  build.ps1 -Mode build
#   - TEST PHASE  : ONE caller runs  build.ps1 -Mode test
#   - VERIFY PHASE: every agent runs the SAME exe with its own BILI_* hooks,
#                   no compiling.
#
# The gate is a lock file opened with FileShare.None, so callers queue instead
# of fighting over cargo's own lock, and a waiter prints how long it waited so
# the coordination cost is visible instead of silent.
#
# Usage:
#   powershell -File tools/build.ps1 -Mode check
#   powershell -File tools/build.ps1 -Mode build
#   powershell -File tools/build.ps1 -Mode test
#   powershell -File tools/build.ps1 -Mode all      # check, then build, then test
#   powershell -File tools/build.ps1 -Mode build -TimeoutSec 1800
#
# Exit code is cargo's exit code (0 on success), or 124 when the gate could not
# be acquired in time.

[CmdletBinding()]
param(
    [ValidateSet('check', 'build', 'test', 'all')]
    [string]$Mode = 'build',

    # Seconds to wait for another caller to release the gate.
    [int]$TimeoutSec = 1800,

    # Optional label shown in the log lines, e.g. the agent's wave name.
    [string]$Label = 'build',

    # Parallel codegen jobs. The repo's .cargo/config.toml pins jobs = 4 so that
    # several projects can build side by side; when this is the only build on the
    # machine, pass a higher value (6-8 on a 6C/12T box) for the same reason the
    # config comment says so.
    [int]$Jobs = 0
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
$gatePath = Join-Path $root 'target\.build-gate'
$logPath = Join-Path $root 'target\.build-gate.log'

if (-not (Test-Path $cargo)) {
    Write-Error "cargo not found at $cargo"
    exit 127
}

# Development pause switch. While `target\.build-paused` exists the gate refuses
# every build instead of queueing, so a paused tree cannot be compiled by a
# stray agent or a forgotten command. Delete the file to resume.
$pausePath = Join-Path $root 'target\.build-paused'
if (Test-Path $pausePath) {
    Write-Host 'build gate: PAUSED (delete target\.build-paused to resume)'
    exit 130
}
if (-not (Test-Path (Join-Path $root 'target'))) {
    New-Item -ItemType Directory -Path (Join-Path $root 'target') | Out-Null
}

function Write-Gate([string]$text) {
    $line = "[{0:HH:mm:ss}] [{1}] {2}" -f (Get-Date), $Label, $text
    Write-Host $line
    Add-Content -Path $logPath -Value $line -Encoding utf8
}

# Acquire the gate: FileShare.None fails while someone else holds it.
$waited = [Diagnostics.Stopwatch]::StartNew()
$handle = $null
while ($null -eq $handle) {
    try {
        $handle = [System.IO.File]::Open(
            $gatePath,
            [System.IO.FileMode]::OpenOrCreate,
            [System.IO.FileAccess]::ReadWrite,
            [System.IO.FileShare]::None)
    }
    catch [System.IO.IOException] {
        if ($waited.Elapsed.TotalSeconds -ge $TimeoutSec) {
            Write-Gate ("gate timeout after {0:N1}s: another caller holds it" -f $waited.Elapsed.TotalSeconds)
            exit 124
        }
        Start-Sleep -Milliseconds 750
    }
}

try {
    if ($waited.Elapsed.TotalSeconds -ge 3) {
        Write-Gate ("gate acquired after {0:N1}s wait" -f $waited.Elapsed.TotalSeconds)
    } else {
        Write-Gate 'gate acquired'
    }

    $modes = if ($Mode -eq 'all') { @('check', 'build', 'test') } else { @($Mode) }
    $lastExit = 0
    foreach ($step in $modes) {
        $started = [Diagnostics.Stopwatch]::StartNew()
        Write-Gate "cargo $step (release) jobs=$(if ($Jobs -gt 0) { $Jobs } else { 'auto' }) ..."
        if ($Jobs -gt 0) {
            & $cargo $step --release -j $Jobs 2>&1 | ForEach-Object { Write-Host $_ }
        } else {
            & $cargo $step --release 2>&1 | ForEach-Object { Write-Host $_ }
        }
        $lastExit = $LASTEXITCODE
        Write-Gate ("cargo {0} exit={1} elapsed={2:N1}s" -f $step, $lastExit, $started.Elapsed.TotalSeconds)
        if ($lastExit -ne 0) { break }
    }
    exit $lastExit
}
finally {
    if ($null -ne $handle) {
        $handle.Dispose()
    }
}
