<#
.SYNOPSIS
    Check the Rust core's dependencies (docs/decisions/dependency-gate.md).

.DESCRIPTION
    cargo-deny against core-rust/deny.toml: known vulnerabilities, yanked versions, unmaintained and
    unsound crates, sources and licenses.

    -Release  the release gate (scripts/release.ps1): any finding that is an error fails, and so does a
              check that cannot run (cargo-deny missing, no advisory database) - it fails closed.
    default   the daily run (scripts/test-core.ps1): findings are reported, nothing fails.

    Unmaintained and unsound crates are errors only in the crypto and network path (everything
    $CriticalCrates depend on); elsewhere they are warnings. An exception in deny.toml must end its
    reason with its review date ("... пересмотреть до YYYY-MM-DD"); past it, or without one, it fails.
#>
param([switch]$Release)

$ErrorActionPreference = 'Stop'
$CargoDenyVersion = '0.20.2'
$TargetTriple = 'x86_64-pc-windows-msvc'
$CriticalCrates = @('omni-crypto', 'omni-network', 'omni-transport', 'omni-messaging')

$repoRoot = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $repoRoot 'core-rust\Cargo.toml'
$config = Join-Path $repoRoot 'core-rust\deny.toml'

function Stop-Check([string]$message) {
    if ($Release) {
        Write-Host "DEPENDENCY CHECK FAILED: $message" -ForegroundColor Red
        exit 1
    }
    Write-Host "Dependency check: $message" -ForegroundColor Yellow
    exit 0
}

$installed = ''
try { $installed = (& cargo deny --version 2>$null) -join ' ' } catch { $installed = '' }
if ($installed -notmatch [regex]::Escape($CargoDenyVersion)) {
    Stop-Check "cargo-deny $CargoDenyVersion is not installed (found: '$installed'). Install: cargo install --locked cargo-deny --version $CargoDenyVersion"
}

# Exceptions expire: each needs its review date, and the date must not have passed.
$today = Get-Date -Format 'yyyy-MM-dd'
$expired = @()
foreach ($line in Get-Content $config -Encoding UTF8) {
    if ($line -match '^\s*#') { continue }
    if ($line -match 'id\s*=\s*"(RUSTSEC-[0-9]{4}-[0-9]{4})"') {
        $id = $Matches[1]
        if ($line -notmatch 'до\s+([0-9]{4}-[0-9]{2}-[0-9]{2})') {
            $expired += "$id (no review date)"
        } elseif ($Matches[1] -lt $today) {
            $expired += "$id (review date $($Matches[1]) passed)"
        }
    }
}

# What the crypto and network path pulls in: there an unmaintained or unsound crate stays an error.
$critical = @{}
$treeArgs = @('tree', '--manifest-path', $manifest, '--target', $TargetTriple, '-e', 'normal,build', '--prefix', 'none', '--format', '{p}')
foreach ($crate in $CriticalCrates) { $treeArgs += @('-p', $crate) }
# Windows PowerShell treats Cargo's download progress on stderr as error records.
# Preserve the command's exit status while allowing a fresh checkout to fetch dependencies.
$previous = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
try {
    $tree = & cargo @treeArgs 2>$null
    $treeExit = $LASTEXITCODE
} finally {
    $ErrorActionPreference = $previous
}
if ($treeExit -ne 0) { Stop-Check 'cargo tree failed: the crypto and network path is unknown' }
foreach ($line in $tree) {
    if ($line -match '^(\S+) v(\S+)') { $critical["$($Matches[1]) $($Matches[2])"] = $true }
}

$denyArgs = @('deny', '--manifest-path', $manifest, '--config', $config, '--locked', '--format', 'json', '--color', 'never', 'check', 'advisories', 'bans', 'licenses', 'sources')
$previous = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
$lines = & cargo @denyArgs 2>&1 | ForEach-Object { "$_" }
$denyExit = $LASTEXITCODE
$ErrorActionPreference = $previous

$errors = @()
$warnings = @()
$reported = 0
$unparsed = @()
foreach ($line in $lines) {
    $entry = $null
    try { $entry = $line | ConvertFrom-Json } catch { $unparsed += $line; continue }
    if ($entry.type -ne 'diagnostic') { continue }
    $fields = $entry.fields
    $code = "$($fields.code)"
    $crate = ''
    if ($fields.graphs -and $fields.graphs.Count -gt 0) { $crate = "$($fields.graphs[0].Krate.name) $($fields.graphs[0].Krate.version)" }
    $advisory = ''
    if ($fields.advisory) { $advisory = " $($fields.advisory.id)" }
    $text = "[$code]$advisory $crate - $($fields.message)"
    if ($fields.severity -eq 'error') { $reported++ }
    if ($fields.severity -ne 'error') {
        $warnings += $text
    } elseif (($code -eq 'unmaintained' -or $code -eq 'unsound') -and -not $critical.ContainsKey($crate)) {
        $warnings += "$text (outside the crypto and network path)"
    } else {
        $errors += $text
    }
}

# Two versions of one crate are common in a tree this size: counted, not listed.
$duplicates = @($warnings | Where-Object { $_ -like '`[duplicate`]*' }).Count
$warnings = @($warnings | Where-Object { $_ -notlike '`[duplicate`]*' })
if ($duplicates -gt 0) { Write-Host "$duplicates crate(s) in two versions or more (cargo deny check bans lists them)" }
foreach ($warning in $warnings) { Write-Host "warning: $warning" -ForegroundColor Yellow }
foreach ($id in $expired) { $errors += "exception $id" }
foreach ($finding in $errors) { Write-Host "error: $finding" -ForegroundColor Red }

if ($denyExit -ne 0 -and $reported -eq 0) {
    # cargo-deny failed without reporting a finding: it could not check (no advisory database, a bad
    # config). Not a pass.
    $unparsed | Select-Object -Last 20 | ForEach-Object { Write-Host $_ }
    Stop-Check "cargo-deny could not complete the check (exit $denyExit)"
}
if ($errors.Count -gt 0) {
    Stop-Check "$($errors.Count) finding(s) stop a release; see docs/decisions/dependency-gate.md"
}
Write-Host "Dependency check passed ($($warnings.Count) warning(s))." -ForegroundColor Green
exit 0
