<#
.SYNOPSIS
    Pack the part of a built libwebrtc tree that the native media module needs, or print the kit key.

.DESCRIPTION
    The release links the native media module against libwebrtc. Building libwebrtc takes an hour and
    the module needs only a small part of the tree: headers, the GN output's libraries and objects,
    generated headers, and the clang-cl/ninja the module is compiled with. This script copies that
    part (same relative paths, so the result is used exactly like the full checkout) and archives it
    with a kit.json manifest.

    The KEY names one exact libwebrtc build: a hash of the scripts that decide it - the pinned
    revision (bootstrap-libwebrtc.ps1), the GN arguments and targets (build-libwebrtc.ps1) and what
    goes into the kit (this file). The release workflow computes the key from its own checkout and
    uses a kit only if the kit's manifest carries the same key and its provenance attestation says
    it was built by this repository's libwebrtc workflow.

.PARAMETER DependencyRoot
    The folder bootstrap-libwebrtc.ps1 filled (webrtc\src under it).

.PARAMETER Output
    The .tar.gz to write.

.PARAMETER KeyOnly
    Print the kit key and exit.
#>
param(
    [string]$DependencyRoot = "D:\OmniDeps",
    [string]$OutName = "omni-win-x64-noh264",
    [string]$Output = "",
    [switch]$KeyOnly
)

$ErrorActionPreference = "Stop"

function Get-KitKey {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $all = New-Object System.IO.MemoryStream
    foreach ($name in 'bootstrap-libwebrtc.ps1', 'build-libwebrtc.ps1', 'pack-libwebrtc-kit.ps1') {
        # Line endings depend on the checkout, not on the build.
        $text = [System.IO.File]::ReadAllText((Join-Path $PSScriptRoot $name)).Replace("`r`n", "`n")
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($text)
        $all.Write($bytes, 0, $bytes.Length)
    }
    $hash = $sha.ComputeHash($all.ToArray())
    return (-join ($hash | ForEach-Object { $_.ToString('x2') })).Substring(0, 16)
}

$key = Get-KitKey
if ($KeyOnly) {
    Write-Output $key
    return
}
if (-not $Output) { throw "-Output is required" }

$src = Join-Path $DependencyRoot "webrtc\src"
$out = Join-Path $src "out\$OutName"
if (-not (Test-Path (Join-Path $out "obj\webrtc.lib"))) { throw "libwebrtc is not built: $out" }

$bootstrap = [System.IO.File]::ReadAllText((Join-Path $PSScriptRoot 'bootstrap-libwebrtc.ps1'))
if ($bootstrap -notmatch '\$Revision = "([0-9a-f]{40})"') { throw "Pinned revision not found in bootstrap-libwebrtc.ps1" }
$revision = $Matches[1]

$staging = Join-Path ([System.IO.Path]::GetTempPath()) "libwebrtc-kit-$key"
if (Test-Path $staging) { Remove-Item $staging -Recurse -Force }
$kitSrc = Join-Path $staging "webrtc\src"

function Copy-Tree([string]$from, [string]$to, [string[]]$patterns, [string[]]$excludeDirs = @()) {
    $arguments = @($from, $to) + $patterns + @('/S', '/NFL', '/NDL', '/NJH', '/NJS', '/NP', '/R:1', '/W:1')
    if ($excludeDirs.Count -gt 0) { $arguments += @('/XD') + $excludeDirs }
    & robocopy @arguments | Out-Null
    # robocopy: 0-7 is success (files copied or nothing to copy), 8 and up is a failure.
    if ($LASTEXITCODE -ge 8) { throw "robocopy $from failed ($LASTEXITCODE)" }
}

# Headers anywhere in the tree: the module's include roots are src itself and several third_party dirs.
Copy-Tree $src $kitSrc @('*.h', '*.hpp', '*.inc', '*.def') @((Join-Path $src 'out'), (Join-Path $src '.git'))
# Whole folders: libc++ headers have no extension; api/ also holds a source the module compiles
# (api/video/rtp_video_frame_assembler.cc); the toolchain the module is compiled and linked with.
foreach ($dir in 'api', 'buildtools\third_party\libc++', 'third_party\libc++\src\include',
                 'third_party\llvm-build', 'third_party\ninja') {
    Copy-Tree (Join-Path $src $dir) (Join-Path $kitSrc $dir) @('*.*', '*')
}
Copy-Item (Join-Path $src 'BUILD.gn') $kitSrc
# GN output: the libraries and single objects the module links, and the generated headers.
Copy-Tree (Join-Path $out 'obj') (Join-Path $kitSrc "out\$OutName\obj") @('*.lib', '*.obj')
Copy-Tree (Join-Path $out 'gen') (Join-Path $kitSrc "out\$OutName\gen") @('*.*', '*')

[ordered]@{
    key = $key
    revision = $revision
    outName = $OutName
    sourceCommit = $env:GITHUB_SHA
} | ConvertTo-Json | Set-Content -Path (Join-Path $staging 'kit.json') -Encoding ascii

$outputDir = Split-Path ([System.IO.Path]::GetFullPath($Output)) -Parent
New-Item -ItemType Directory -Force $outputDir | Out-Null
& tar -czf $Output -C $staging .
if ($LASTEXITCODE -ne 0) { throw "tar failed ($LASTEXITCODE)" }
Remove-Item $staging -Recurse -Force
$size = (Get-Item $Output).Length / 1MB
Write-Host ("libwebrtc kit {0} (revision {1}): {2} ({3:N0} MB)" -f $key, $revision, $Output, $size)
