<#
.SYNOPSIS
    Build the portable release exe.

.DESCRIPTION
    1. Publishes the single-file standalone build (win-x64-standalone profile).
    2. Copies the exe to dist\Omni.exe and writes dist\SHA256SUMS.txt.

.PARAMETER TargetDir
    Cargo's target directory for the release core. Outside the source tree by default, so the release
    build never shares artifacts with development builds.
#>
param(
    [switch]$SkipBuild,
    [string]$TargetDir = ''
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'build-tools-env.ps1')
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path.TrimEnd('\')
if (-not $TargetDir) { $TargetDir = Join-Path ([System.IO.Path]::GetPathRoot($repo)) 'omni-release-target' }
$TargetDir = [System.IO.Path]::GetFullPath($TargetDir).TrimEnd('\')
if ($TargetDir.StartsWith($repo, [StringComparison]::OrdinalIgnoreCase)) {
    throw "TargetDir must be outside the source tree: $TargetDir"
}
$app = Join-Path $repo 'windows-app\src\Omni.Windows.App'
$publishDir = Join-Path $app 'bin\x64\publish-standalone'
$dist = Join-Path $repo 'dist'

# The project file bundles the media DLL only if it exists, so without it the publish succeeds and
# the exe silently has no calls at all (the media engine fails to load). Check before the long build.
if (-not (Get-ChildItem (Join-Path $repo 'native\omni-webrtc-media\build') -Recurse -Filter 'omni_webrtc_media.dll' -ErrorAction SilentlyContinue)) {
    throw 'No native media DLL was built: an exe without it has no audio or video. Run scripts\build-omni-webrtc-media.ps1 -Configuration Release -UseLibWebRtc first (see DEVELOPING.md).'
}

if (-not $SkipBuild) {
    dotnet publish (Join-Path $app 'Omni.Windows.App.csproj') -p:PublishProfile=win-x64-standalone "-p:OmniCoreTargetDir=$TargetDir"
    if ($LASTEXITCODE -ne 0) { throw "dotnet publish failed ($LASTEXITCODE)" }
}

$exe = Join-Path $publishDir 'Omni.Windows.App.exe'
if (-not (Test-Path $exe)) { throw "Published exe not found: $exe" }

New-Item -ItemType Directory -Force $dist | Out-Null
Copy-Item $exe (Join-Path $dist 'Omni.exe') -Force
$hash = (Get-FileHash (Join-Path $dist 'Omni.exe') -Algorithm SHA256).Hash.ToLowerInvariant()
[System.IO.File]::WriteAllText((Join-Path $dist 'SHA256SUMS.txt'), "$hash  Omni.exe`n")
Write-Host ''
Write-Host "dist\Omni.exe  sha256 $hash" -ForegroundColor Green
