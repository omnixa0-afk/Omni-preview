param(
    [ValidateSet("Debug", "Release", "RelWithDebInfo")]
    [string]$Configuration = "Debug",
    [string]$Generator = "",
    [switch]$Clean,
    [switch]$NoTest,
    [switch]$UseLibWebRtc,
    [string]$WebRtcSrc = "D:\OmniDeps\webrtc\src",
    [string]$WebRtcOut = ""
)

$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$sourceDir = Join-Path $repoRoot "native\omni-webrtc-media"
$buildName = if ($UseLibWebRtc) { "$Configuration-libwebrtc" } else { $Configuration }
$buildDir = Join-Path $sourceDir "build\$buildName"

if (!(Test-Path $sourceDir)) {
    throw "Native media source directory not found: $sourceDir"
}

if ($Clean -and (Test-Path $buildDir)) {
    Remove-Item -LiteralPath $buildDir -Recurse -Force
}

New-Item -ItemType Directory -Force -Path $buildDir | Out-Null

$configureArgs = @("-S", $sourceDir, "-B", $buildDir)
if (![string]::IsNullOrWhiteSpace($Generator)) {
    $configureArgs += @("-G", $Generator)
}

if ($UseLibWebRtc) {
    if ([string]::IsNullOrWhiteSpace($WebRtcOut)) {
        $WebRtcOut = Join-Path $WebRtcSrc "out\omni-win-x64-noh264"
    }

    if (!(Test-Path (Join-Path $WebRtcSrc "BUILD.gn"))) {
        throw "WebRTC src checkout not found: $WebRtcSrc"
    }

    if (!(Test-Path (Join-Path $WebRtcOut "obj\webrtc.lib"))) {
        throw "WebRTC GN output was not built: $WebRtcOut"
    }

    $webrtcClang = Join-Path $WebRtcSrc "third_party\llvm-build\Release+Asserts\bin\clang-cl.exe"
    $webrtcNinjaDir = Join-Path $WebRtcSrc "third_party\ninja"
    $windowsKitBin = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin" -Directory |
        Where-Object { $_.Name -like "10.*" } |
        Sort-Object Name -Descending |
        Select-Object -First 1
    $windowsKitX64Bin = if ($windowsKitBin) { Join-Path $windowsKitBin.FullName "x64" } else { "" }
    $rcCompiler = if ($windowsKitX64Bin -and (Test-Path (Join-Path $windowsKitX64Bin "rc.exe"))) {
        Join-Path $windowsKitX64Bin "rc.exe"
    } else {
        Join-Path $WebRtcSrc "build\toolchain\win\rc\win\rc.exe"
    }
    $manifestTool = if ($windowsKitX64Bin -and (Test-Path (Join-Path $windowsKitX64Bin "mt.exe"))) {
        Join-Path $windowsKitX64Bin "mt.exe"
    } else {
        ""
    }

    if (!(Test-Path $webrtcClang)) {
        throw "WebRTC clang-cl not found: $webrtcClang"
    }

    if (!(Test-Path (Join-Path $webrtcNinjaDir "ninja.exe"))) {
        throw "WebRTC ninja not found: $webrtcNinjaDir"
    }

    if (!(Test-Path $rcCompiler)) {
        throw "Resource compiler not found: $rcCompiler"
    }

    if ([string]::IsNullOrWhiteSpace($manifestTool) -or !(Test-Path $manifestTool)) {
        throw "Manifest tool mt.exe not found in the Windows SDK."
    }

    $env:PATH = "$webrtcNinjaDir;$(Split-Path $webrtcClang -Parent);$windowsKitX64Bin;$env:PATH"

    if ([string]::IsNullOrWhiteSpace($Generator)) {
        $cmakeClang = $webrtcClang.Replace("\", "/")
        $cmakeRcCompiler = $rcCompiler.Replace("\", "/")
        $cmakeManifestTool = $manifestTool.Replace("\", "/")
        $configureArgs += @(
            "-G",
            "Ninja",
            "-DCMAKE_CXX_COMPILER=$cmakeClang",
            "-DCMAKE_RC_COMPILER=$cmakeRcCompiler",
            "-DCMAKE_MT=$cmakeManifestTool",
            "-DCMAKE_BUILD_TYPE=$Configuration"
        )
    }

    $configureArgs += @(
        "-DOMNI_WEBRTC_MEDIA_ENABLE_LIBWEBRTC=ON",
        "-DOMNI_WEBRTC_MEDIA_WEBRTC_SRC=$WebRtcSrc",
        "-DOMNI_WEBRTC_MEDIA_WEBRTC_OUT=$WebRtcOut"
    )
} elseif ([string]::IsNullOrWhiteSpace($Generator)) {
    $configureArgs += @("-A", "x64")
} else {
    $configureArgs += @("-A", "x64")
}

& cmake @configureArgs
if ($LASTEXITCODE -ne 0) {
    throw "CMake configure failed with exit code $LASTEXITCODE"
}

& cmake --build $buildDir --config $Configuration --parallel
if ($LASTEXITCODE -ne 0) {
    throw "Native media build failed with exit code $LASTEXITCODE"
}

if (!$NoTest) {
    & ctest --test-dir $buildDir -C $Configuration --output-on-failure
    if ($LASTEXITCODE -ne 0) {
        throw "Native media tests failed with exit code $LASTEXITCODE"
    }
}

Write-Host "omni-webrtc-media build completed: $buildDir"
