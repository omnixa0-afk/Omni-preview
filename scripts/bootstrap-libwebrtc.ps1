param(
    [string]$DependencyRoot = "D:\OmniDeps",
    [switch]$Sync,
    [switch]$RunHooks,
    # The libwebrtc commit Omni is built and tested against; the release workflow builds this one too.
    # Moving it is a deliberate change: rebuild (scripts\build-libwebrtc.ps1) and rerun the native smoke.
    [string]$Revision = "85a228f95fc2b273f732b8f68c8c966d861a3702"
)

$ErrorActionPreference = "Stop"

$depotToolsDir = Join-Path $DependencyRoot "depot_tools"
$depotToolsGitConfig = Join-Path $DependencyRoot "depot_tools.gitconfig"
$webrtcRoot = Join-Path $DependencyRoot "webrtc"
$webrtcSrc = Join-Path $webrtcRoot "src"

New-Item -ItemType Directory -Force -Path $DependencyRoot | Out-Null

if (!(Test-Path $depotToolsDir)) {
    git clone https://chromium.googlesource.com/chromium/tools/depot_tools.git $depotToolsDir
}

$env:PATH = "$depotToolsDir;$env:PATH"
$env:DEPOT_TOOLS_WIN_TOOLCHAIN = "0"
$env:GIT_CONFIG_GLOBAL = $depotToolsGitConfig

if (!(Test-Path $depotToolsGitConfig)) {
    @"
[core]
	autocrlf = false
	filemode = false
	fscache = true
	preloadindex = true
[depot-tools]
	allowGlobalGitConfig = false
"@ | Set-Content -Path $depotToolsGitConfig -Encoding utf8
}

Write-Host "depot_tools=$depotToolsDir"
Write-Host "git_config_global=$depotToolsGitConfig"
if (!(Test-Path (Join-Path $depotToolsDir "gclient.bat"))) {
    throw "gclient.bat was not found in depot_tools."
}

if (!$Sync) {
    Write-Host "Depot tools are ready. Re-run with -Sync to create or update the large WebRTC checkout at $webrtcRoot."
    return
}

New-Item -ItemType Directory -Force -Path $webrtcRoot | Out-Null
Push-Location $webrtcRoot
try {
    if (!(Test-Path (Join-Path $webrtcRoot ".gclient"))) {
        # Sources only, without git history: the history is many gigabytes nobody builds from.
        & fetch --nohooks --no-history webrtc
        if ($LASTEXITCODE -ne 0) {
            throw "fetch webrtc failed with exit code $LASTEXITCODE"
        }
    }
    # Pinned: src and every DEPS entry at the revision above, not whatever main is today.
    & gclient sync --nohooks --no-history --revision "src@$Revision"
    if ($LASTEXITCODE -ne 0) {
        throw "gclient sync failed with exit code $LASTEXITCODE"
    }

    if ($RunHooks) {
        & gclient runhooks
        if ($LASTEXITCODE -ne 0) {
            throw "gclient runhooks failed with exit code $LASTEXITCODE"
        }
    }
} finally {
    Pop-Location
}

if (!(Test-Path $webrtcSrc)) {
    throw "WebRTC source checkout was not found at $webrtcSrc"
}

Write-Host "libwebrtc source is available at $webrtcSrc"
