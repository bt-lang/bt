<#
.SYNOPSIS
Builds and checks the standalone SQLite WASI package in target/.
#>
[CmdletBinding()]
param([string]$BtPath = "", [switch]$SkipTests)

$ErrorActionPreference = "Stop"
$extensionRoot = $PSScriptRoot
$repoRoot = [IO.Path]::GetFullPath((Join-Path $extensionRoot "../.."))
if (!$BtPath) { $BtPath = Join-Path $repoRoot "target/debug/bt.exe" }
$BtPath = (Resolve-Path -LiteralPath $BtPath).Path
$manifestPath = Join-Path $extensionRoot "Cargo.toml"
$stage = Join-Path $extensionRoot "target/package-stage"

& cargo fmt --manifest-path $manifestPath -- --check
if ($LASTEXITCODE -ne 0) { throw "SQLite formatting check failed." }
if (!$SkipTests) {
    & cargo test --locked --manifest-path $manifestPath
    if ($LASTEXITCODE -ne 0) { throw "SQLite tests failed." }
}
& cargo build --locked --manifest-path $manifestPath --target wasm32-wasip1 --release
if ($LASTEXITCODE -ne 0) { throw "SQLite WASI build failed." }

New-Item -ItemType Directory -Force -Path $stage | Out-Null
foreach ($name in @("manifest.json", "bindings.json", "LICENSE-MIT", "LICENSE-APACHE", "COPYRIGHT", "THIRD_PARTY_LICENSES.txt", "README.md")) {
    Copy-Item -LiteralPath (Join-Path $extensionRoot $name) -Destination (Join-Path $stage $name) -Force
}
Copy-Item -LiteralPath (Join-Path $extensionRoot "target/wasm32-wasip1/release/sqlite.wasm") -Destination (Join-Path $stage "module.wasm") -Force
$version = (Get-Content -Encoding UTF8 -LiteralPath (Join-Path $extensionRoot "manifest.json") | ConvertFrom-Json).version
$package = Join-Path $extensionRoot "target/sqlite-$version.bts"
$buildOutput = & $BtPath ext build $stage -o $package 2>&1
$buildOutput | Write-Output
if ($LASTEXITCODE -ne 0 -or !($buildOutput -match "^Built extension package:")) { throw "SQLite package build failed." }
$checkOutput = & $BtPath ext check $package 2>&1
$checkOutput | Write-Output
if ($LASTEXITCODE -ne 0 -or !($checkOutput -match "Extension package check passed:")) { throw "SQLite package validation failed." }
Write-Output "SQLite package: $package"
