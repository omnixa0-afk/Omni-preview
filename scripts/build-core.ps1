param(
    [switch]$Release,
    [string]$TargetTriple = 'x86_64-pc-windows-msvc'
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'build-tools-env.ps1')

$repoRoot = Split-Path -Parent $PSScriptRoot
$manifestPath = Join-Path $repoRoot 'core-rust\Cargo.toml'
$profileDir = if ($Release) { 'release' } else { 'debug' }

$cargoArgs = @(
    'build'
    '--manifest-path'
    $manifestPath
    '-p'
    'omni-ffi'
    '--target'
    $TargetTriple
)

if ($Release) {
    $cargoArgs += '--release'
}

Write-Host "Building omni-ffi ($profileDir, $TargetTriple)..."
cargo @cargoArgs

$dllPath = Join-Path $repoRoot "core-rust\target\$TargetTriple\$profileDir\omni_ffi.dll"
if (-not (Test-Path $dllPath)) {
    throw "Rust DLL was not produced at expected path: $dllPath"
}

Write-Host "Rust DLL path: $dllPath"
