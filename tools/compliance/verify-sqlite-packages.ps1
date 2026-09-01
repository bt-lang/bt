[CmdletBinding()]
param()

Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.IO.Compression.FileSystem

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$SqliteRoot = Join-Path $RepoRoot "examples\extension-development\sqlite"
$Packages = @(
    (Join-Path $RepoRoot "examples\extension-development\extensions\sqlite.bts"),
    (Join-Path $SqliteRoot "sqlite-1.0.0.bts")
)
$Required = @(
    "bindings.json",
    "Cargo.lock",
    "Cargo.toml",
    "COPYRIGHT",
    "LICENSE-APACHE",
    "LICENSE-MIT",
    "manifest.json",
    "module.wasm",
    "README.md",
    "src/lib.rs",
    "THIRD_PARTY_LICENSES.txt"
)
$TempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("bt-sqlite-verify-" + [guid]::NewGuid().ToString("N"))

# Reads a package text file using the repository's canonical LF representation.
function Get-CanonicalText {
    param([string]$Path)

    return ((Get-Content -LiteralPath $Path -Raw -Encoding UTF8) -replace "`r`n", "`n") -replace "`r", "`n"
}

New-Item -ItemType Directory -Path $TempRoot | Out-Null
try {
    $roots = @()
    for ($index = 0; $index -lt $Packages.Count; $index++) {
        $packagePath = $Packages[$index]
        if (-not (Test-Path -LiteralPath $packagePath -PathType Leaf)) {
            throw "SQLite package is missing: $packagePath"
        }
        $root = Join-Path $TempRoot ([string]$index)
        [System.IO.Compression.ZipFile]::ExtractToDirectory($packagePath, $root)
        $roots += $root

        $files = @(Get-ChildItem -LiteralPath $root -Recurse -File | ForEach-Object {
            $_.FullName.Substring($root.Length + 1).Replace('\', '/')
        } | Sort-Object)
        $difference = @(Compare-Object -ReferenceObject $Required -DifferenceObject $files)
        if ($difference.Count -ne 0) {
            $details = $difference | ForEach-Object { "$($_.SideIndicator) $($_.InputObject)" }
            throw "SQLite package entries are invalid:`n$($details -join "`n")"
        }

        $manifest = Get-Content -Raw -Encoding UTF8 (Join-Path $root "manifest.json") | ConvertFrom-Json
        if ($manifest.name -ne "sqlite" -or $manifest.version -ne "1.0.0" -or $manifest.author -ne "Lifeng Yan") {
            throw "SQLite manifest identity, version, or author is invalid."
        }
    }

    foreach ($relative in $Required) {
        $nativeRelative = $relative.Replace([char]'/', [System.IO.Path]::DirectorySeparatorChar)
        $left = (Get-FileHash -LiteralPath (Join-Path $roots[0] $nativeRelative) -Algorithm SHA256).Hash
        $right = (Get-FileHash -LiteralPath (Join-Path $roots[1] $nativeRelative) -Algorithm SHA256).Hash
        if ($left -ne $right) {
            throw "SQLite packages disagree at $relative"
        }
    }

    foreach ($relative in @("bindings.json", "Cargo.lock", "Cargo.toml", "COPYRIGHT", "LICENSE-APACHE", "LICENSE-MIT", "manifest.json", "README.md", "src/lib.rs", "THIRD_PARTY_LICENSES.txt")) {
        $nativeRelative = $relative.Replace([char]'/', [System.IO.Path]::DirectorySeparatorChar)
        $sourcePath = Join-Path $SqliteRoot $nativeRelative
        $packagePath = Join-Path $roots[0] $nativeRelative
        if ((Get-CanonicalText $sourcePath) -cne (Get-CanonicalText $packagePath)) {
            throw "SQLite package does not match current source at $relative"
        }
    }
} finally {
    Remove-Item -LiteralPath $TempRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "SQLite package contents are identical and current."
