$TargetTriple = 'x86_64-pc-windows-msvc'
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'build-tools-env.ps1')

$repoRoot = Split-Path -Parent $PSScriptRoot
$manifestPath = Join-Path $repoRoot 'core-rust\Cargo.toml'

Write-Host 'Running Rust workspace tests...'
# Let cargo write straight to the console. Do NOT pipe its output (| tee / tail / Out-String / ...):
# a PowerShell pipeline exits with the LAST element's code, so piping drops cargo's failure code and
# makes the gate read green even though a test failed. That measurement artifact -- not this script --
# is what made an earlier run of the gate look green (the script has always propagated $LASTEXITCODE).
# --no-fail-fast runs every crate's tests even after one crate fails, so a failure in an early crate
# can't hide failures in the crates that would have run after it: the gate reports them all in one pass.
cargo test --manifest-path $manifestPath --workspace --no-fail-fast --target $TargetTriple -- --test-threads=1
$testsExitCode = $LASTEXITCODE


if ($testsExitCode -ne 0) {
    Write-Host ''
    Write-Host "CORE TESTS FAILED (cargo exit code $testsExitCode)" -ForegroundColor Red
    exit $testsExitCode
}

# Dev-only checks kept outside the published tree, when this checkout has them (repository hygiene).
$devChecks = Join-Path $repoRoot 'public\check-hygiene.ps1'
if (Test-Path $devChecks) {
    Write-Host 'Running repository hygiene check...'
    & powershell -NoProfile -ExecutionPolicy Bypass -File $devChecks
    if ($LASTEXITCODE -ne 0) {
        Write-Host ''
        Write-Host "HYGIENE CHECK FAILED (exit code $LASTEXITCODE)" -ForegroundColor Red
        exit $LASTEXITCODE
    }
}

# Dependencies (docs/decisions/dependency-gate.md): reported here, they fail only a release.
& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'check-dependencies.ps1')

Write-Host 'CORE TESTS PASSED' -ForegroundColor Green
exit 0
