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
$fingerprints = Join-Path $targetRoot "debug\.fingerprint"

# Select an already built library with the needed features; this step never runs Cargo.
function Find-TestDependency {
    param([string]$Crate, [string[]]$Features = @())
    $libraryName = $Crate.Replace("-", "_")
    $libraries = @(Get-ChildItem -LiteralPath $dependencies -Filter "lib$libraryName-*.rlib" |
        Sort-Object LastWriteTimeUtc -Descending)
    foreach ($library in $libraries) {
        $hash = $library.BaseName.Substring(("lib$libraryName-").Length)
        $metadataPath = Join-Path $fingerprints "$Crate-$hash\lib-$libraryName.json"
        if (-not (Test-Path -LiteralPath $metadataPath)) { continue }
        $metadata = Get-Content -LiteralPath $metadataPath -Raw -Encoding UTF8 | ConvertFrom-Json
        $enabled = @($metadata.features | ConvertFrom-Json)
        $missing = @($Features | Where-Object { $enabled -notcontains $_ })
        if ($missing.Count -eq 0) { return $library.FullName }
    }
    throw "No built $Crate library with the needed features was found. First run cargo test --locked --no-default-features --bin bt 'install::'."
}

$windows = Find-TestDependency "windows-sys" @("Win32_System_Registry", "Win32_UI_Shell", "Win32_UI_WindowsAndMessaging", "Win32_Storage_FileSystem")
$semver = Find-TestDependency "semver"
$uuid = Find-TestDependency "uuid" @("v4")
$manifest = Get-Content -LiteralPath (Join-Path $repoRoot "Cargo.toml") -Raw -Encoding UTF8
$package = [regex]::Match($manifest, '(?ms)^\[package\]\s*(.*?)(?=^\[|\z)').Groups[1].Value
$version = [regex]::Match($package, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
if ([string]::IsNullOrWhiteSpace($version)) { throw "Cannot read the root package version." }
$previousVersion = $env:CARGO_PKG_VERSION
try {
    $env:CARGO_PKG_VERSION = $version
    $source = Join-Path $PSScriptRoot "verify-user-install-windows.rs"
    $executable = Join-Path $targetRoot "verify-user-install-windows.exe"
    & rustc --edition 2021 -D warnings $source --extern "windows_sys=$windows" --extern "semver=$semver" --extern "uuid=$uuid" -L "dependency=$dependencies" -o $executable
    if ($LASTEXITCODE -ne 0) { throw "Windows installer acceptance harness did not compile." }
    & $executable
    if ($LASTEXITCODE -ne 0) { throw "Windows installer acceptance harness failed." }
} finally {
    $env:CARGO_PKG_VERSION = $previousVersion
}
