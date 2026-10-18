param(
    [string]$DependencyRoot = "D:\OmniDeps",
    [string]$OutName = "omni-win-x64-noh264",
    [switch]$Clean
)

$ErrorActionPreference = "Stop"

$depotToolsDir = Join-Path $DependencyRoot "depot_tools"
$depotToolsGitConfig = Join-Path $DependencyRoot "depot_tools.gitconfig"
$webrtcSrc = Join-Path $DependencyRoot "webrtc\src"
$webrtcOut = Join-Path $webrtcSrc "out\$OutName"

if (!(Test-Path (Join-Path $depotToolsDir "gclient.bat"))) {
    throw "depot_tools are missing. Run scripts\bootstrap-libwebrtc.ps1 first."
}

if (!(Test-Path (Join-Path $webrtcSrc "BUILD.gn"))) {
    throw "WebRTC src checkout is missing. Run scripts\bootstrap-libwebrtc.ps1 -Sync -RunHooks first."
}

$env:PATH = "$depotToolsDir;$(Join-Path $webrtcSrc "buildtools\win");$(Join-Path $webrtcSrc "third_party\ninja");$env:PATH"
$env:DEPOT_TOOLS_WIN_TOOLCHAIN = "0"
$env:GIT_CONFIG_GLOBAL = $depotToolsGitConfig

if ($Clean -and (Test-Path $webrtcOut)) {
    Remove-Item -LiteralPath $webrtcOut -Recurse -Force
}

New-Item -ItemType Directory -Force -Path $webrtcOut | Out-Null

@"
is_debug = false
target_cpu = "x64"
is_clang = true
is_component_build = false
rtc_include_tests = false
rtc_build_examples = false
rtc_build_tools = false
treat_warnings_as_errors = false
proprietary_codecs = false
rtc_use_h264 = false
rtc_use_h265 = true
symbol_level = 0
"@ | Set-Content -Path (Join-Path $webrtcOut "args.gn") -Encoding ascii

Push-Location $webrtcSrc
try {
    & gn gen "out\$OutName"
    if ($LASTEXITCODE -ne 0) {
        throw "gn gen failed with exit code $LASTEXITCODE"
    }

    # Beyond webrtc.lib the native layer links single objects from targets webrtc does not pull in
    # (native/omni-webrtc-media/CMakeLists.txt: field trials, simulated time, fake clock).
    # rtc_use_h265 above is HEVC RTP packetizing/parsing only (no software HEVC codec exists in
    # libwebrtc); it defaults to proprietary_codecs, which is off.
    & ninja -C "out\$OutName" webrtc api/video_codecs:builtin_video_encoder_factory api/video_codecs:builtin_video_decoder_factory api:field_trials api:time_controller rtc_base:rtc_base_tests_utils test/time_controller:time_controller test/time_controller:time_controller_impl
    if ($LASTEXITCODE -ne 0) {
        throw "ninja failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}

Write-Host "libwebrtc build completed: $webrtcOut"
