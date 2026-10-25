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
# Measured profile (6C/12T, `-j 8`, lld-link, warm dependency graph), release
# build of this crate after `touch src/main.rs` -- i.e. the edit/verify cycle:
#
#   phase breakdown of one default release build      wall      share
#   ---------------------------------------------------------------
#   Slint build script (build.rs -> app.rs)            0 s      0 %   (does not re-run on .rs edits)
#   one rustc invocation, LLVM codegen                ~4m15s    79 %
#   front end (parse/expand/typeck of 23 MB app.rs)   ~0m53s    17 %   (this is what `cargo check` costs)
#   lld-link                                           1 s      0.4 %
#
#   `touch src/main.rs && cargo build --release`:
#   before [profile.release.package.o11a1] incremental = true  4m18s - 5m35s
#   after   (steady state, cache warm)                         0m30s - 0m50s
#   `cargo check --release`, same touch, after                 ~0m16s
#   `touch ui/app.slint` + build (real edit, app.rs changes)   ~4m - 5m
#
# Consequences for this script:
#   - The first build after Cargo.toml/build.rs changed, or after the target
#     dir was cleaned, pays a full rebuild to write the incremental cache
#     (~3.5 GB under target/release/incremental). Do not read that number as a
#     regression.
#   - A real ui/app.slint edit invalidates the whole generated module, so
#     incremental cannot help there: UI iterations still cost minutes. Iterate
#     on Rust with the numbers above and batch UI changes.
#   - Numbers move by +-30 % when other projects build at the same time (SMT
#     sharing inflates both wall and CPU time), which is exactly why the gate
#     below exists; quote timings only from gated builds.
#   - `-j` does not change the cost of rebuilding this crate: it is one rustc
#     unit and its codegen threads come from the core count, not from -j. -j
#     matters for a cold dependency build, so it is still threaded through.
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
        # cargo prints progress ("Compiling ...", "Finished ...") on stderr. In
        # PowerShell 5.1 a native command's stderr becomes an ErrorRecord, and
        # with $ErrorActionPreference = 'Stop' the very first line would abort
        # the script (exit 1, no "exit=" line in the gate log). Merge the two
        # streams inside cmd.exe so PowerShell only ever sees stdout text; the
        # exit code still comes from cargo via cmd.exe.
        $argLine = "$step --release" + $(if ($Jobs -gt 0) { " -j $Jobs" } else { '' })
        & cmd /d /c "`"$cargo`" $argLine 2>&1" | ForEach-Object { Write-Host $_ }
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
