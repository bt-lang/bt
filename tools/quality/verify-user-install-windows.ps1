param(
    [string]$TargetDirectory = ""
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"
if ($env:OS -ne "Windows_NT") {
    throw "This isolated registry acceptance test requires Windows."
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
if ([string]::IsNullOrWhiteSpace($TargetDirectory)) {
    if ([string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR)) {
        $TargetDirectory = Join-Path $repoRoot "target"
    } else {
        $TargetDirectory = $env:CARGO_TARGET_DIR
    }
}
if (-not [System.IO.Path]::IsPathRooted($TargetDirectory)) {
    $TargetDirectory = Join-Path $repoRoot $TargetDirectory
}
$targetRoot = (Resolve-Path -LiteralPath $TargetDirectory).Path
$dependencies = Join-Path $targetRoot "debug\deps"
# Resolve one coherent Cargo dependency graph; timestamps can mix incompatible feature builds.
$builtLibraries = @{}
& cargo build --manifest-path (Join-Path $repoRoot "Cargo.toml") --locked --no-default-features --bin bt --target-dir $targetRoot --message-format=json | ForEach-Object {
    $artifact = $_ | ConvertFrom-Json
    if ($artifact.reason -eq 'compiler-artifact') {
        foreach ($filename in $artifact.filenames) {
            if ($filename.EndsWith('.rlib')) {
                $builtLibraries[$artifact.package_id] = $filename
            }
        }
    }
}
if ($LASTEXITCODE -ne 0) { throw "Cannot build the isolated installer's dependency graph." }

$graph = (& cargo metadata --manifest-path (Join-Path $repoRoot "Cargo.toml") --locked --format-version 1 --no-default-features --filter-platform x86_64-pc-windows-msvc | ConvertFrom-Json)
if ($LASTEXITCODE -ne 0) { throw "Cannot resolve installer dependencies." }
$rootNode = $graph.resolve.nodes | Where-Object { $_.id -eq $graph.resolve.root }

function Find-TestDependency {
    param([string]$Crate)
    $libraryName = $Crate.Replace('-', '_')
    $dependency = $rootNode.deps | Where-Object { $_.name -eq $libraryName }
    if (-not $dependency -or -not $builtLibraries.ContainsKey($dependency.pkg)) {
        throw "Cargo did not produce the required $Crate library."
    }
    return $builtLibraries[$dependency.pkg]
}

$windows = Find-TestDependency 'windows-sys'
$semver = Find-TestDependency 'semver'
$uuid = Find-TestDependency 'uuid'
$manifest = Get-Content -LiteralPath (Join-Path $repoRoot "Cargo.toml") -Raw -Encoding UTF8
$package = [regex]::Match($manifest, '(?ms)^\[package\]\s*(.*?)(?=^\[|\z)').Groups[1].Value
$version = [regex]::Match($package, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
if ([string]::IsNullOrWhiteSpace($version)) { throw "Cannot read the root package version." }
$updateDependencies = @()
foreach ($crate in @('serde', 'serde_json', 'sha2', 'tokio', 'reqwest', 'zip', 'rustls')) {
    $library = Find-TestDependency $crate
    $updateDependencies += @('--extern', "$crate=$library")
}
$previousVersion = $env:CARGO_PKG_VERSION
try {
    $env:CARGO_PKG_VERSION = $version
    $source = Join-Path $PSScriptRoot "verify-user-install-windows.rs"
    $executable = Join-Path $targetRoot "verify-user-install-windows.exe"
    & rustc @updateDependencies --edition 2021 -D warnings $source --extern "windows_sys=$windows" --extern "semver=$semver" --extern "uuid=$uuid" -L "dependency=$dependencies" -o $executable
    if ($LASTEXITCODE -ne 0) { throw "Windows installer acceptance harness did not compile." }
    & $executable
    if ($LASTEXITCODE -ne 0) { throw "Windows installer acceptance harness failed." }
} finally {
    $env:CARGO_PKG_VERSION = $previousVersion
}
