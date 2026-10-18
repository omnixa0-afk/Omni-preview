# Dot-source from a build script: puts the native build tools the Rust core needs on PATH.
#
# The database is SQLCipher over a vendored OpenSSL, which the openssl-src build compiles from source:
# its Configure step runs on Perl, and NASM assembles the AES-NI code paths (without NASM OpenSSL falls
# back to slow portable C). Neither ships with Windows. Where they live is machine-specific, so it is
# read from the untracked Directory.Build.local.props (also used by the WinUI build):
#
#   <Project><PropertyGroup>
#     <OmniBuildToolsPath>C:\path\to\perl\bin;C:\path\to\nasm</OmniBuildToolsPath>
#   </PropertyGroup></Project>
#
# Tools already on PATH (a CI runner, a system install) need no file.

$localProps = Join-Path (Split-Path -Parent $PSScriptRoot) 'Directory.Build.local.props'
if (Test-Path $localProps) {
    $toolsPath = ([xml](Get-Content $localProps -Raw)).Project.PropertyGroup.OmniBuildToolsPath | Where-Object { $_ } | Select-Object -First 1
    if ($toolsPath) { $env:PATH = "$($toolsPath.Trim());$env:PATH" }
}
foreach ($tool in 'perl', 'nasm') {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        Write-Warning "$tool is not on PATH: the SQLCipher/OpenSSL build needs it (see scripts/build-tools-env.ps1)."
    }
}
