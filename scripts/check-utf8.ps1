$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$tracked = git -C $repoRoot ls-files

$textExtensions = @(
    ".cs",
    ".csproj",
    ".json",
    ".md",
    ".props",
    ".ps1",
    ".rs",
    ".sln",
    ".targets",
    ".toml",
    ".txt",
    ".xaml",
    ".xml",
    ".yml",
    ".yaml"
)

$denyList = @(
    "РћС‡РµСЂРµРґСЊ",
    "РЎРІРѕРґРєР°",
    "РџСЂРёС‘Рј Рё РїРѕРёСЃРє",
    "РќРµ СѓРґР°Р»РѕСЃСЊ",
    "Р’С‹Р±РµСЂРёС‚Рµ С‡Р°С‚",
    "РЎРѕРѕР±С‰РµРЅРёР№",
    "РЎС‚Р°С‚СѓСЃ:",
    "РёРЅРёС†РёР°Р»РёР·РёСЂРѕРІР°РЅ",
    "РђР»РёСЃР°",
    "Р‘РѕР±",
    "пїЅ"
)

$utf8NoBom = [System.Text.UTF8Encoding]::new($false, $true)
$failures = New-Object System.Collections.Generic.List[string]

foreach ($relativePath in $tracked) {
    if ($relativePath -eq "scripts/check-utf8.ps1") {
        continue
    }

    $fullPath = Join-Path $repoRoot $relativePath
    if (-not (Test-Path -LiteralPath $fullPath -PathType Leaf)) {
        continue
    }

    $extension = [System.IO.Path]::GetExtension($fullPath)
    if (-not $textExtensions.Contains($extension)) {
        continue
    }

    try {
        $bytes = [System.IO.File]::ReadAllBytes($fullPath)
        $text = $utf8NoBom.GetString($bytes)
    }
    catch {
        $failures.Add("${relativePath}: invalid UTF-8")
        continue
    }

    if ($text.Contains([char]0xFFFD)) {
        $failures.Add("${relativePath}: contains replacement character U+FFFD")
    }

    foreach ($fragment in $denyList) {
        if ($text.Contains($fragment)) {
            $failures.Add("${relativePath}: contains mojibake fragment '$fragment'")
        }
    }
}

if ($failures.Count -gt 0) {
    $failures | Sort-Object -Unique | ForEach-Object { Write-Error $_ }
    exit 1
}

Write-Host "UTF-8 guardrail passed."
