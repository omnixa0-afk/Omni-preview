<#
.SYNOPSIS
    Regenerate auto-maintained architecture docs from source.

.DESCRIPTION
    Walks the source tree and produces three markdown maps that survive
    refactors because they're regenerated, not hand-edited:

      docs/architecture/code-map.md       -- symbol -> file table
      docs/architecture/storage-schema.md -- SQLite table schema dump
      docs/architecture/ffi-surface.md    -- every `omni_*` FFI export

    Run with no args to regen all three. Use -CodeMap / -StorageSchema /
    -FfiSurface to limit. Idempotent; output deterministic (sorted).

    The intent: give a reader a single lookup target instead of a repo-wide grep.

.NOTES
    Designed to run pre-commit or on demand. Should not require any
    external tools beyond a stock Windows PowerShell + .NET regex.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\regen-docs.ps1
#>

param(
    [switch]$CodeMap,
    [switch]$StorageSchema,
    [switch]$FfiSurface,
    [switch]$All
)

$ErrorActionPreference = 'Stop'

$repoRoot   = Resolve-Path (Join-Path $PSScriptRoot '..')
$rustRoot   = Join-Path $repoRoot 'core-rust\crates'
$nativeRoot = Join-Path $repoRoot 'native'
$csharpRoot = Join-Path $repoRoot 'windows-app\src'
$docsRoot   = Join-Path $repoRoot 'docs\architecture'

if (-not (Test-Path $docsRoot)) {
    New-Item -ItemType Directory -Path $docsRoot -Force | Out-Null
}

# If no specific flag was set, regen all.
$doAll = $All -or (-not ($CodeMap -or $StorageSchema -or $FfiSurface))

$timestamp = (Get-Date).ToString('yyyy-MM-dd HH:mm')

function To-RelPath([string]$absolute) {
    $rel = $absolute.Substring($repoRoot.Path.Length).TrimStart('\','/')
    return $rel -replace '\\','/'
}

# ----------------------------------------------------------------------
# Code map -- Rust pub items, statics, FFI exports; C# public types.
# ----------------------------------------------------------------------

function Get-RustSymbols([string]$crateRoot) {
    $results = @()
    $rsFiles = Get-ChildItem -Path $crateRoot -Filter '*.rs' -Recurse -File -ErrorAction SilentlyContinue
    foreach ($file in $rsFiles) {
        # Skip generated / vendored / target -- Get-ChildItem already excludes
        # target since it lives at workspace level, but be defensive.
        if ($file.FullName -match '\\target\\') { continue }

        $lineNum = 0
        foreach ($line in Get-Content -LiteralPath $file.FullName) {
            $lineNum++
            $trimmed = $line.TrimStart()

            # pub fn / pub(crate) fn / pub async fn / pub(crate) async fn
            if ($trimmed -match '^pub(?:\(crate\))?\s+(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)') {
                $results += [pscustomobject]@{
                    Symbol = $matches[1]
                    Kind   = 'fn'
                    File   = To-RelPath $file.FullName
                    Line   = $lineNum
                }
                continue
            }
            # pub struct / pub(crate) struct
            if ($trimmed -match '^pub(?:\(crate\))?\s+struct\s+(\w+)') {
                $results += [pscustomobject]@{
                    Symbol = $matches[1]
                    Kind   = 'struct'
                    File   = To-RelPath $file.FullName
                    Line   = $lineNum
                }
                continue
            }
            # pub enum
            if ($trimmed -match '^pub(?:\(crate\))?\s+enum\s+(\w+)') {
                $results += [pscustomobject]@{
                    Symbol = $matches[1]
                    Kind   = 'enum'
                    File   = To-RelPath $file.FullName
                    Line   = $lineNum
                }
                continue
            }
            # pub trait
            if ($trimmed -match '^pub(?:\(crate\))?\s+trait\s+(\w+)') {
                $results += [pscustomobject]@{
                    Symbol = $matches[1]
                    Kind   = 'trait'
                    File   = To-RelPath $file.FullName
                    Line   = $lineNum
                }
                continue
            }
            # pub type
            if ($trimmed -match '^pub(?:\(crate\))?\s+type\s+(\w+)') {
                $results += [pscustomobject]@{
                    Symbol = $matches[1]
                    Kind   = 'type'
                    File   = To-RelPath $file.FullName
                    Line   = $lineNum
                }
                continue
            }
            # static UPPER_SNAKE (module-level constants of interest, esp.
            # registries). Skip lowercase / mixed-case statics.
            if ($trimmed -match '^static\s+([A-Z][A-Z0-9_]{2,})\s*:') {
                $results += [pscustomobject]@{
                    Symbol = $matches[1]
                    Kind   = 'static'
                    File   = To-RelPath $file.FullName
                    Line   = $lineNum
                }
                continue
            }
            # const UPPER_SNAKE (compile-time constants -- useful navigation)
            if ($trimmed -match '^(?:pub(?:\(crate\))?\s+)?const\s+([A-Z][A-Z0-9_]{2,})\s*:') {
                $results += [pscustomobject]@{
                    Symbol = $matches[1]
                    Kind   = 'const'
                    File   = To-RelPath $file.FullName
                    Line   = $lineNum
                }
                continue
            }
        }
    }
    return $results
}

function Get-NativeSymbols([string]$root) {
    # Extract C ABI declarations from headers + class declarations from .cpp.
    # C++ is hard to parse reliably with regex; we intentionally keep this
    # narrow: only the highest-value navigation targets.
    $results = @()
    $files = Get-ChildItem -Path $root -Include '*.h','*.hpp','*.cpp','*.cc' -Recurse -File -ErrorAction SilentlyContinue
    foreach ($file in $files) {
        if ($file.FullName -match '\\build\\') { continue }
        $lineNum = 0
        foreach ($line in Get-Content -LiteralPath $file.FullName) {
            $lineNum++
            $trimmed = $line.TrimStart()

            # Header C ABI: `OMNI_WEBRTC_MEDIA_API ReturnType omni_xxx(...)`.
            if ($trimmed -match '^OMNI_WEBRTC_MEDIA_API\s+\w[\w\s\*&]*\s+(\w+)\s*\(') {
                $results += [pscustomobject]@{
                    Symbol = $matches[1]
                    Kind   = 'c-abi'
                    File   = To-RelPath $file.FullName
                    Line   = $lineNum
                }
                continue
            }

            # typedef struct / enum name
            if ($trimmed -match '^typedef\s+(?:struct|enum)\s+(?:\w+\s+)?\{') {
                # multi-line typedef — name comes at the closing brace; skip.
                continue
            }
            if ($trimmed -match '^typedef\s+(?:struct|enum)\s+(\w+)\s+\w+\s*;') {
                $results += [pscustomobject]@{
                    Symbol = $matches[1]
                    Kind   = 'typedef'
                    File   = To-RelPath $file.FullName
                    Line   = $lineNum
                }
                continue
            }
            # `} OmniWebRtcMediaXxx;` — closing of typedef struct/enum.
            if ($trimmed -match '^\}\s*(\w+)\s*;\s*$') {
                $results += [pscustomobject]@{
                    Symbol = $matches[1]
                    Kind   = 'typedef'
                    File   = To-RelPath $file.FullName
                    Line   = $lineNum
                }
                continue
            }

            # class Name { or class Name :
            if ($trimmed -match '^class\s+(\w+)\s*[:\{]') {
                $results += [pscustomobject]@{
                    Symbol = $matches[1]
                    Kind   = 'class'
                    File   = To-RelPath $file.FullName
                    Line   = $lineNum
                }
                continue
            }

            # Out-of-line member function definition: Type Class::method(
            if ($trimmed -match '^\w[\w\s\*&<>,]*\s+(\w+)::(\w+)\s*\(') {
                $results += [pscustomobject]@{
                    Symbol = ('{0}::{1}' -f $matches[1], $matches[2])
                    Kind   = 'method'
                    File   = To-RelPath $file.FullName
                    Line   = $lineNum
                }
                continue
            }
        }
    }
    return $results
}

function Get-CSharpSymbols([string]$root) {
    $results = @()
    $csFiles = Get-ChildItem -Path $root -Filter '*.cs' -Recurse -File -ErrorAction SilentlyContinue
    foreach ($file in $csFiles) {
        if ($file.FullName -match '\\(bin|obj)\\') { continue }
        $lineNum = 0
        foreach ($line in Get-Content -LiteralPath $file.FullName) {
            $lineNum++
            $trimmed = $line.TrimStart()
            # public class / internal class / public sealed class / etc.
            # Also record, struct, enum, interface -- single regex for all type kinds.
            if ($trimmed -match '^(?:public|internal)\s+(?:(?:static|sealed|abstract|partial|readonly|unsafe)\s+)*(class|struct|enum|interface|record)\s+(\w+)') {
                $results += [pscustomobject]@{
                    Symbol = $matches[2]
                    Kind   = $matches[1]
                    File   = To-RelPath $file.FullName
                    Line   = $lineNum
                }
            }
        }
    }
    return $results
}

function Write-CodeMap {
    Write-Host '  Code map: scanning Rust crates...' -ForegroundColor Cyan
    $crateDirs = Get-ChildItem -Path $rustRoot -Directory -ErrorAction SilentlyContinue |
        Sort-Object Name
    $rustByCrate = [ordered]@{}
    foreach ($crate in $crateDirs) {
        $crateSrc = Join-Path $crate.FullName 'src'
        if (-not (Test-Path $crateSrc)) { continue }
        $syms = Get-RustSymbols $crateSrc | Sort-Object Kind, Symbol
        $rustByCrate[$crate.Name] = $syms
    }

    Write-Host '  Code map: scanning C# (windows-app)...' -ForegroundColor Cyan
    $csSyms = @()
    if (Test-Path $csharpRoot) {
        $csSyms = Get-CSharpSymbols $csharpRoot | Sort-Object Symbol
    }

    Write-Host '  Code map: scanning native (omni-webrtc-media)...' -ForegroundColor Cyan
    $nativeSyms = @()
    if (Test-Path $nativeRoot) {
        $nativeSyms = Get-NativeSymbols $nativeRoot | Sort-Object Kind, Symbol
    }

    $sb = [System.Text.StringBuilder]::new()
    [void]$sb.AppendLine('---')
    [void]$sb.AppendLine('tags: [architecture, code-map, auto-generated]')
    [void]$sb.AppendLine("generated_at: $timestamp")
    [void]$sb.AppendLine('---')
    [void]$sb.AppendLine('')
    [void]$sb.AppendLine('# Code Map')
    [void]$sb.AppendLine('')
    [void]$sb.AppendLine('**Auto-generated. Do not edit.** Regenerate with `scripts/regen-docs.ps1`.')
    [void]$sb.AppendLine('')
    [void]$sb.AppendLine('Includes Rust `pub` / `pub(crate)` items, module-level `static`/`const` constants,')
    [void]$sb.AppendLine("and C# `public`/`internal` type declarations. Private items are intentionally")
    [void]$sb.AppendLine("excluded -- grep the file directly when you need them.")
    [void]$sb.AppendLine('')

    foreach ($crateName in $rustByCrate.Keys) {
        $syms = $rustByCrate[$crateName]
        if (-not $syms -or $syms.Count -eq 0) { continue }
        [void]$sb.AppendLine("## Rust: ``$crateName``")
        [void]$sb.AppendLine('')
        [void]$sb.AppendLine('| Symbol | Kind | File:Line |')
        [void]$sb.AppendLine('|---|---|---|')
        foreach ($s in $syms) {
            $cell = '`' + $s.Symbol + '`'
            $loc = $s.File + ':' + $s.Line
            [void]$sb.AppendLine(('| {0} | {1} | {2} |' -f $cell, $s.Kind, $loc))
        }
        [void]$sb.AppendLine('')
    }

    if ($csSyms.Count -gt 0) {
        [void]$sb.AppendLine('## C#: ``windows-app``')
        [void]$sb.AppendLine('')
        [void]$sb.AppendLine('| Symbol | Kind | File:Line |')
        [void]$sb.AppendLine('|---|---|---|')
        foreach ($s in $csSyms) {
            $cell = '`' + $s.Symbol + '`'
            $loc = $s.File + ':' + $s.Line
            [void]$sb.AppendLine(('| {0} | {1} | {2} |' -f $cell, $s.Kind, $loc))
        }
        [void]$sb.AppendLine('')
    }

    if ($nativeSyms.Count -gt 0) {
        [void]$sb.AppendLine('## Native C++: ``omni-webrtc-media``')
        [void]$sb.AppendLine('')
        [void]$sb.AppendLine('Narrow extraction: header C ABI (`OMNI_WEBRTC_MEDIA_API`), typedefs,')
        [void]$sb.AppendLine('class declarations, and out-of-line member function definitions.')
        [void]$sb.AppendLine('For everything else (anon namespace helpers, lambdas) — grep the file.')
        [void]$sb.AppendLine('')
        [void]$sb.AppendLine('| Symbol | Kind | File:Line |')
        [void]$sb.AppendLine('|---|---|---|')
        foreach ($s in $nativeSyms) {
            $cell = '`' + $s.Symbol + '`'
            $loc = $s.File + ':' + $s.Line
            [void]$sb.AppendLine(('| {0} | {1} | {2} |' -f $cell, $s.Kind, $loc))
        }
        [void]$sb.AppendLine('')
    }

    $output = $sb.ToString()
    $outPath = Join-Path $docsRoot 'code-map.md'
    Set-Content -LiteralPath $outPath -Value $output -Encoding UTF8 -NoNewline
    Write-Host "  Code map: wrote $(To-RelPath $outPath)" -ForegroundColor Green
}

# ----------------------------------------------------------------------
# Storage schema -- parses CREATE TABLE blocks out of the migrations file.
# ----------------------------------------------------------------------

function Write-StorageSchema {
    Write-Host '  Storage schema: parsing migrations...' -ForegroundColor Cyan
    $migrationsPath = Join-Path $rustRoot 'omni-storage\src\storage_migrations.rs'
    if (-not (Test-Path $migrationsPath)) {
        Write-Warning "Migrations file not found at $migrationsPath"
        return
    }
    $text = Get-Content -LiteralPath $migrationsPath -Raw

    # Match: CREATE TABLE IF NOT EXISTS name (... );
    # Note: SQLite columns can contain parens (in CHECK / DEFAULT). We
    # rely on the fact that the outermost ); is followed by whitespace
    # then ", or by " (end of Rust raw string). We match lazily to avoid
    # eating multiple CREATE TABLEs.
    $regex = [regex]::new(
        'CREATE\s+TABLE(?:\s+IF\s+NOT\s+EXISTS)?\s+(\w+)\s*\((.*?)\)\s*;',
        [System.Text.RegularExpressions.RegexOptions]::Singleline -bor
        [System.Text.RegularExpressions.RegexOptions]::IgnoreCase
    )

    $tables = @()
    foreach ($m in $regex.Matches($text)) {
        $name = $m.Groups[1].Value
        $body = $m.Groups[2].Value
        # Skip duplicates (migrations may CREATE TABLE IF NOT EXISTS for
        # safety; first occurrence wins as the canonical schema).
        if ($tables.Symbol -contains $name) { continue }
        $tables += [pscustomobject]@{
            Symbol = $name
            Body   = $body
        }
    }

    # Ensure deterministic order.
    $tables = $tables | Sort-Object Symbol

    # Find ensure_table_column calls for each table -- additive migrations.
    $alterRegex = [regex]::new(
        'ensure_table_column\s*\(\s*"(\w+)"\s*,\s*"(\w+)"\s*,\s*"([^"]+)"',
        [System.Text.RegularExpressions.RegexOptions]::Singleline
    )
    $alterByTable = @{}
    foreach ($m in $alterRegex.Matches($text)) {
        $t = $m.Groups[1].Value
        $col = $m.Groups[2].Value
        $type = $m.Groups[3].Value
        if (-not $alterByTable.ContainsKey($t)) { $alterByTable[$t] = @() }
        $alterByTable[$t] += [pscustomobject]@{ Column = $col; Type = $type }
    }

    $sb = [System.Text.StringBuilder]::new()
    [void]$sb.AppendLine('---')
    [void]$sb.AppendLine('tags: [architecture, storage, schema, auto-generated]')
    [void]$sb.AppendLine("generated_at: $timestamp")
    [void]$sb.AppendLine('---')
    [void]$sb.AppendLine('')
    [void]$sb.AppendLine('# Storage Schema')
    [void]$sb.AppendLine('')
    [void]$sb.AppendLine('**Auto-generated. Do not edit.** Regenerate with `scripts/regen-docs.ps1`.')
    [void]$sb.AppendLine('')
    [void]$sb.AppendLine("Tables defined by `CREATE TABLE` in `omni-storage/src/storage_migrations.rs`.")
    [void]$sb.AppendLine("Additive columns added via `ensure_table_column` are listed under each table.")
    [void]$sb.AppendLine('')
    [void]$sb.AppendLine('## Table index')
    [void]$sb.AppendLine('')
    foreach ($t in $tables) {
        [void]$sb.AppendLine("- [``$($t.Symbol)``](#$($t.Symbol -replace '_','-'))")
    }
    [void]$sb.AppendLine('')

    foreach ($t in $tables) {
        [void]$sb.AppendLine("## ``$($t.Symbol)``")
        [void]$sb.AppendLine('')
        # Split body into "column or constraint" lines. Naive split on commas
        # at the top nesting level -- sufficient for SQLite migration style.
        $depth = 0
        $current = New-Object System.Text.StringBuilder
        $parts = @()
        foreach ($ch in $t.Body.ToCharArray()) {
            if ($ch -eq '(') { $depth++ }
            elseif ($ch -eq ')') { $depth-- }
            elseif ($ch -eq ',' -and $depth -eq 0) {
                $parts += $current.ToString().Trim()
                $current.Clear() | Out-Null
                continue
            }
            [void]$current.Append($ch)
        }
        if ($current.Length -gt 0) {
            $parts += $current.ToString().Trim()
        }

        [void]$sb.AppendLine('| Column / Constraint | Definition |')
        [void]$sb.AppendLine('|---|---|')
        foreach ($p in $parts) {
            $clean = ($p -replace '\s+',' ').Trim()
            if (-not $clean) { continue }
            if ($clean -match '^([A-Za-z_]\w*)\s+(.+)$' -and
                -not ($clean -match '^(PRIMARY|FOREIGN|UNIQUE|CHECK|CONSTRAINT)\b')) {
                $col = $matches[1]
                $rest = $matches[2]
                [void]$sb.AppendLine(('| `{0}` | {1} |' -f $col, $rest))
            }
            else {
                [void]$sb.AppendLine(('| _constraint_ | {0} |' -f $clean))
            }
        }

        if ($alterByTable.ContainsKey($t.Symbol)) {
            [void]$sb.AppendLine('')
            [void]$sb.AppendLine('**Added via `ensure_table_column` (additive migrations):**')
            [void]$sb.AppendLine('')
            [void]$sb.AppendLine('| Column | Type |')
            [void]$sb.AppendLine('|---|---|')
            foreach ($a in $alterByTable[$t.Symbol]) {
                [void]$sb.AppendLine(('| `{0}` | {1} |' -f $a.Column, $a.Type))
            }
        }
        [void]$sb.AppendLine('')
    }

    $outPath = Join-Path $docsRoot 'storage-schema.md'
    Set-Content -LiteralPath $outPath -Value $sb.ToString() -Encoding UTF8 -NoNewline
    Write-Host "  Storage schema: wrote $(To-RelPath $outPath) ($($tables.Count) tables)" -ForegroundColor Green
}

# ----------------------------------------------------------------------
# FFI surface -- every `#[no_mangle] pub extern "C" fn omni_*` export.
# ----------------------------------------------------------------------

function Write-FfiSurface {
    Write-Host '  FFI surface: scanning omni-ffi exports...' -ForegroundColor Cyan
    $ffiRoot = Join-Path $rustRoot 'omni-ffi\src'
    if (-not (Test-Path $ffiRoot)) {
        Write-Warning "omni-ffi/src not found at $ffiRoot"
        return
    }

    $exports = @()
    $regex = [regex]::new(
        'pub\s+extern\s+"C"\s+fn\s+(omni_\w+)\s*\((.*?)\)\s*(?:->\s*([^{]+))?\s*\{',
        [System.Text.RegularExpressions.RegexOptions]::Singleline
    )

    $files = Get-ChildItem -Path $ffiRoot -Filter '*.rs' -Recurse -File
    foreach ($file in $files) {
        $text = Get-Content -LiteralPath $file.FullName -Raw
        foreach ($m in $regex.Matches($text)) {
            $name = $m.Groups[1].Value
            $params = ($m.Groups[2].Value -replace '\s+',' ').Trim()
            $ret = ''
            if ($m.Groups[3].Success) {
                $ret = ($m.Groups[3].Value -replace '\s+',' ').Trim()
            }
            # Compute approximate source line by counting newlines before match.
            $lineNum = 1 + ($text.Substring(0, $m.Index) -split "`n").Count - 1
            $exports += [pscustomobject]@{
                Symbol = $name
                Params = $params
                Return = $ret
                File   = To-RelPath $file.FullName
                Line   = $lineNum
            }
        }
    }

    $exports = $exports | Sort-Object Symbol

    # Group by domain prefix: omni_app_, omni_chat_, omni_calls_,
    # omni_identity_, omni_contacts_, omni_realtime_media_, omni_signaling_,
    # omni_devices_, omni_files_, omni_settings_, omni_app, omni_string_, etc.
    # Group by first segment after omni_. Known multi-word domains
    # (realtime_media, public_profile) collapse into one group too.
    $byDomain = [ordered]@{}
    foreach ($e in $exports) {
        $domain = 'misc'
        if ($e.Symbol -match '^omni_(realtime_media|public_profile)_') {
            $domain = $matches[1]
        }
        elseif ($e.Symbol -match '^omni_([a-z]+)') {
            $domain = $matches[1]
        }
        if (-not $byDomain.Contains($domain)) {
            $byDomain[$domain] = @()
        }
        $byDomain[$domain] += $e
    }
    # Sort domains alphabetically for deterministic output.
    $sortedKeys = @($byDomain.Keys) | Sort-Object
    $sortedByDomain = [ordered]@{}
    foreach ($d in $sortedKeys) { $sortedByDomain[$d] = $byDomain[$d] }
    $byDomain = $sortedByDomain

    $sb = [System.Text.StringBuilder]::new()
    [void]$sb.AppendLine('---')
    [void]$sb.AppendLine('tags: [architecture, ffi, auto-generated]')
    [void]$sb.AppendLine("generated_at: $timestamp")
    [void]$sb.AppendLine('---')
    [void]$sb.AppendLine('')
    [void]$sb.AppendLine('# FFI Surface')
    [void]$sb.AppendLine('')
    [void]$sb.AppendLine('**Auto-generated. Do not edit.** Regenerate with `scripts/regen-docs.ps1`.')
    [void]$sb.AppendLine('')
    [void]$sb.AppendLine('Every `pub extern "C" fn omni_*` export from `omni-ffi`. C# side imports')
    [void]$sb.AppendLine('these via `[DllImport]` in `windows-app/src/Omni.Windows.App/Core/NativeMethods.*.cs`.')
    [void]$sb.AppendLine('')
    [void]$sb.AppendLine("Total: **$($exports.Count) exports** across $($byDomain.Keys.Count) domain prefixes.")
    [void]$sb.AppendLine('')

    foreach ($domain in $byDomain.Keys) {
        $items = $byDomain[$domain]
        [void]$sb.AppendLine("## ``omni_${domain}_*`` ($($items.Count) exports)")
        [void]$sb.AppendLine('')
        [void]$sb.AppendLine('| Export | Signature | File:Line |')
        [void]$sb.AppendLine('|---|---|---|')
        foreach ($e in $items) {
            $sig = '(' + $e.Params + ')'
            if ($e.Return) { $sig += ' -> ' + $e.Return }
            $loc = $e.File + ':' + $e.Line
            $sigCell = $sig.Replace('|','\|')
            [void]$sb.AppendLine(('| `{0}` | `{1}` | {2} |' -f $e.Symbol, $sigCell, $loc))
        }
        [void]$sb.AppendLine('')
    }

    $outPath = Join-Path $docsRoot 'ffi-surface.md'
    Set-Content -LiteralPath $outPath -Value $sb.ToString() -Encoding UTF8 -NoNewline
    Write-Host "  FFI surface: wrote $(To-RelPath $outPath) ($($exports.Count) exports)" -ForegroundColor Green
}

# ----------------------------------------------------------------------
# Dispatch
# ----------------------------------------------------------------------

Write-Host "Regenerating docs from source (root: $($repoRoot.Path))" -ForegroundColor Yellow
if ($doAll -or $CodeMap)      { Write-CodeMap }
if ($doAll -or $StorageSchema) { Write-StorageSchema }
if ($doAll -or $FfiSurface)   { Write-FfiSurface }
Write-Host 'Done.' -ForegroundColor Yellow
