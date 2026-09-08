<#
.SYNOPSIS
Installs SQLite in a fresh BT project and verifies canonical and legacy entry points.
#>
[CmdletBinding()]
param([string]$BtPath = "")

$ErrorActionPreference = "Stop"
$extensionRoot = $PSScriptRoot
$repoRoot = [IO.Path]::GetFullPath((Join-Path $extensionRoot "../.."))
if (!$BtPath) { $BtPath = Join-Path $repoRoot "target/debug/bt.exe" }
$BtPath = (Resolve-Path -LiteralPath $BtPath).Path
$version = (Get-Content -Encoding UTF8 -LiteralPath (Join-Path $extensionRoot "manifest.json") | ConvertFrom-Json).version
$package = Join-Path $extensionRoot "target/sqlite-$version.bts"
$project = Join-Path $extensionRoot ("target/entry-acceptance-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $project | Out-Null
Copy-Item -LiteralPath (Join-Path $extensionRoot "smoke.bt") -Destination (Join-Path $project "main.bt")
$installOutput = & $BtPath ext install $package $project 2>&1
$installOutput | Write-Output
if ($LASTEXITCODE -ne 0 -or !($installOutput -match "Installed extension package:")) { throw "SQLite installation failed." }

# Packaging and executable registration are checked together by actual VM dispatch.
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [IO.Compression.ZipFile]::OpenRead($package)
try {
    $expected = @("manifest.json", "bindings.json", "module.wasm", "README.md", "LICENSE-MIT", "LICENSE-APACHE", "COPYRIGHT", "THIRD_PARTY_LICENSES.txt")
    $actual = @($archive.Entries | ForEach-Object { $_.FullName })
    if (@(Compare-Object $expected $actual).Count -ne 0) { throw "Unexpected SQLite package entries." }
} finally { $archive.Dispose() }

$cases = @(
    @{name="canonical_missing_options"; code="sqlite('@/entry.db')"; pattern="requires 2 arguments"},
    @{name="legacy_missing_options"; code="sqlite_open('@/entry.db')"; pattern="requires 2 arguments"},
    @{name="canonical_invalid_options"; code="sqlite('@/entry.db', null)"; pattern="object"},
    @{name="legacy_invalid_options"; code="sqlite_open('@/entry.db', null)"; pattern="object"},
    @{name="canonical_stale_handle"; code="db = sqlite('@/entry.db', {})`ndb.close()`ndb.query('SELECT 1')"; pattern="expired|no longer valid|disposed|closed"},
    @{name="legacy_stale_handle"; code="db = sqlite_open('@/entry.db', {})`ndb.close()`ndb.query('SELECT 1')"; pattern="expired|no longer valid|disposed|closed"}
)
$results = @()
Push-Location $project
try {
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $output = & $BtPath main.bt 2>&1
    $watch.Stop()
    $output | Write-Output
    if ($LASTEXITCODE -ne 0 -or !($output -match "^sqlite-entry-smoke:ok$")) { throw "SQLite BT smoke failed." }
    $results += [ordered]@{case="entry_equivalence"; status="passed"; elapsed_ms=$watch.ElapsedMilliseconds; output=($output -join "`n")}
    foreach ($case in $cases) {
        $script = Join-Path $project ($case.name + ".bt")
        [IO.File]::WriteAllText($script, $case.code + "`nprint 'UNEXPECTED_SUCCESS'`n", (New-Object Text.UTF8Encoding($false)))
        $output = & $BtPath $script 2>&1
        $joined = $output -join "`n"
        # CLI business errors may exit zero; the expected error must precede the marker.
        if ($joined -notmatch $case.pattern -or $joined -match "(?m)^UNEXPECTED_SUCCESS") { throw "$($case.name): $joined" }
        $results += [ordered]@{case=$case.name; status="passed"; output=$joined}
        Write-Output "$($case.name): passed"
    }
} finally { Pop-Location }
[IO.File]::WriteAllText((Join-Path $project "results.json"), ($results | ConvertTo-Json -Depth 6), (New-Object Text.UTF8Encoding($false)))
Write-Output "SQLite acceptance evidence: $(Join-Path $project 'results.json')"
