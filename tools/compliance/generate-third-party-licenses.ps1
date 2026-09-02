[CmdletBinding()]
param(
    [switch]$Check,

    [ValidateSet("windows-x64", "linux-x64", "macos-arm64", "macos-x64")]
    [string]$ReleaseProfile,

    [string]$OutputPath
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$CargoAboutVersion = "0.9.2"
$SqliteRoot = Join-Path $RepoRoot "examples\extension-development\sqlite"
$SqliteOutput = Join-Path $SqliteRoot "THIRD_PARTY_LICENSES.txt"
$ConfigPath = Join-Path $RepoRoot "about.toml"
$TemplatePath = Join-Path $PSScriptRoot "about.hbs"
$DistributionPath = Join-Path $PSScriptRoot "non-rust-distribution.json"

function Invoke-CargoAbout {
    param(
        [string]$ManifestPath,
        [string]$Target,
        [string]$Destination,
        [string[]]$Features = @(),
        [switch]$AllFeatures,
        [switch]$NoDefaultFeatures,
        [switch]$Json,
        [switch]$Workspace
    )

    $arguments = @(
        "about", "generate",
        "--locked",
        "--fail",
        "--config", $ConfigPath,
        "--manifest-path", $ManifestPath,
        "--output-file", $Destination,
        "--target", $Target
    )
    if ($Workspace) {
        $arguments += "--workspace"
    }
    if ($AllFeatures) {
        $arguments += "--all-features"
    }
    if ($NoDefaultFeatures) {
        $arguments += "--no-default-features"
    }
    if ($Features.Count -ne 0) {
        $arguments += @("--features", ($Features -join ","))
    }
    if ($Json) {
        $arguments += @("--format", "json")
    } else {
        $arguments += $TemplatePath
    }

    & cargo @arguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo-about failed for $ManifestPath ($Target)."
    }
}

function Convert-ToLf {
    param([string]$Text)

    $lines = (($Text -replace "`r`n", "`n") -replace "`r", "`n") -split "`n"
    $normalized = ($lines | ForEach-Object { $_.TrimEnd([char[]]" `t") }) -join "`n"
    return $normalized.TrimEnd([char[]]"`n") + "`n"
}

function Assert-DistributionInventory {
    $distribution = Get-Content -Raw -Encoding UTF8 $DistributionPath | ConvertFrom-Json
    if ($distribution.schema_version -ne 1) {
        throw "Unsupported non-Rust distribution inventory schema."
    }

    Push-Location $RepoRoot
    try {
        $tracked = @(git ls-files)
        if ($LASTEXITCODE -ne 0) {
            throw "Unable to enumerate tracked files."
        }
    } finally {
        Pop-Location
    }

    $distributed = @($tracked | Where-Object {
        $_ -match '\.(btr|bts|zip)$' -or
        $_ -match '\.(cjs|js|jsx|mjs|ts|tsx)$' -or
        $_ -match '\.(eot|otf|ttf|woff|woff2)$' -or
        $_ -match '\.(gif|ico|jpe?g|png|svg|webp)$' -or
        $_ -match '(^|/)(package(-lock)?\.json|pnpm-lock\.yaml|yarn\.lock)$'
    } | Sort-Object -Unique)

    $declared = @($distribution.project_owned)
    $declared += @($distribution.bundled_packages | ForEach-Object { $_.path })
    $declared = @($declared | Sort-Object -Unique)

    $difference = @(Compare-Object -ReferenceObject $declared -DifferenceObject $distributed)
    if ($difference.Count -ne 0) {
        $details = $difference | ForEach-Object { "$($_.SideIndicator) $($_.InputObject)" }
        throw "Non-Rust distribution inventory is stale:`n$($details -join "`n")"
    }

    foreach ($bundle in $distribution.bundled_packages) {
        $noticePath = Join-Path $RepoRoot ($bundle.notice -replace '/', '\')
        if (-not (Test-Path -LiteralPath $noticePath -PathType Leaf)) {
            throw "Bundled package notice is missing: $($bundle.notice)"
        }
    }
}

function Test-PlainApacheOption {
    param([string]$Expression)

    return $Expression -match '(^|[ (])Apache-2\.0(?!\s+WITH)(?=$|[ )])'
}

<#
.SYNOPSIS
Checks whether an SPDX expression offers the plain MIT license as an alternative.
#>
function Test-PlainMitOption {
    param([string]$Expression)

    return $Expression -match '(^|[ (])MIT(?=$|[ )])'
}

function Get-DistributionLicenseLabel {
    param([string]$Expression)

    if ($Expression -notmatch '\bAND\b') {
        if (Test-PlainApacheOption $Expression) {
            if ($Expression -eq "Apache-2.0") {
                return "Apache-2.0"
            }
            return "Apache-2.0 (selected from: $Expression)"
        }
        if (Test-PlainMitOption $Expression) {
            if ($Expression -eq "MIT") {
                return "MIT"
            }
            return "MIT (selected from: $Expression)"
        }
    }
    return $Expression
}

function Test-LicenseTextRequired {
    param(
        [string]$Expression,
        $License
    )

    if ($Expression -notmatch '\bAND\b') {
        if (Test-PlainApacheOption $Expression) {
            if ($License.id -eq "Apache-2.0") {
                return $Expression -match 'Apache-2\.0 WITH LLVM-exception' -and
                    ($License.source_path -match 'LLVM' -or $License.text -match 'LLVM Exceptions')
            }
            return $false
        }
        if (Test-PlainMitOption $Expression) {
            return $License.id -eq "MIT"
        }
    }
    return $true
}

function Get-PackageKey {
    param($Package)

    if ($Package.id) {
        return [string]$Package.id
    }
    return "$($Package.name)@$($Package.version)"
}

function Get-PackageSource {
    param($Package)

    if ($Package.source -match '^registry\+') {
        return "https://crates.io/crates/$($Package.name)/$($Package.version)"
    }
    if ($Package.repository) {
        return [string]$Package.repository
    }
    return "https://crates.io/crates/$($Package.name)/$($Package.version)"
}

<#
.SYNOPSIS
Removes implementation bodies from license text harvested from source files.
.DESCRIPTION
cargo-about may return an entire source file when its leading comment carries a
file-specific license. This function keeps only a clearly identified leading
line-comment or block-comment license. Ambiguous inputs remain unchanged.
#>
function Get-DistributionLicenseText {
    param($License)

    $text = [string]$License.text
    $sourcePath = [string]$License.source_path
    if ([string]::IsNullOrWhiteSpace($text) -or [string]::IsNullOrWhiteSpace($sourcePath)) {
        return $text
    }

    $sourceExtensions = @(".asm", ".c", ".cc", ".cpp", ".h", ".hpp", ".rs", ".s")
    $extension = [System.IO.Path]::GetExtension($sourcePath).ToLowerInvariant()
    if ($extension -notin $sourceExtensions) {
        return $text
    }

    $normalized = ($text -replace "`r`n", "`n") -replace "`r", "`n"
    $lines = @($normalized -split "`n")
    $signalIndex = -1
    for ($index = 0; $index -lt $lines.Count; $index++) {
        if ($lines[$index] -match '(?i)copyright|permission is hereby granted|redistribution and use|licensed under') {
            $signalIndex = $index
            break
        }
    }
    if ($signalIndex -lt 0) {
        return $text
    }

    $signalLine = $lines[$signalIndex].TrimStart()
    if ($signalLine.StartsWith("//")) {
        $startIndex = $signalIndex
        while ($startIndex -gt 0) {
            $previous = $lines[$startIndex - 1].TrimStart()
            if ($previous.StartsWith("//") -or [string]::IsNullOrWhiteSpace($previous)) {
                $startIndex--
                continue
            }
            break
        }

        $endIndex = $signalIndex
        while ($endIndex + 1 -lt $lines.Count -and $lines[$endIndex + 1].TrimStart().StartsWith("//")) {
            $endIndex++
        }
    } else {
        $startIndex = $signalIndex
        while ($startIndex -ge 0 -and $lines[$startIndex] -notmatch '/\*') {
            $startIndex--
        }
        if ($startIndex -lt 0) {
            return $text
        }

        $endIndex = $signalIndex
        while ($endIndex -lt $lines.Count -and $lines[$endIndex] -notmatch '\*/') {
            $endIndex++
        }
        if ($endIndex -ge $lines.Count) {
            return $text
        }

        $afterBlock = ($lines[$endIndex] -split '\*/', 2)[1]
        if (-not [string]::IsNullOrWhiteSpace($afterBlock)) {
            return $text
        }
    }

    if ($endIndex + 1 -ge $lines.Count) {
        return $text
    }
    $implementation = ($lines[($endIndex + 1)..($lines.Count - 1)] -join "`n").Trim()
    if ([string]::IsNullOrWhiteSpace($implementation)) {
        return $text
    }

    return ($lines[$startIndex..$endIndex] -join "`n").Trim()
}

function Get-NativeNotice {
    $metadataRaw = & cargo metadata --locked --format-version 1 --filter-platform x86_64-pc-windows-msvc --features desktop
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to resolve the locked Windows desktop dependency graph."
    }
    $metadata = $metadataRaw | ConvertFrom-Json
    $libffi = @($metadata.packages | Where-Object { $_.name -eq "libffi" })
    $libffiSys = @($metadata.packages | Where-Object { $_.name -eq "libffi-sys" })
    if ($libffi.Count -ne 1 -or $libffiSys.Count -ne 1) {
        throw "Expected exactly one libffi and one libffi-sys package in the Windows desktop graph."
    }

    $libffiSysRoot = Split-Path -Parent $libffiSys[0].manifest_path
    $libffiLicensePath = Join-Path $libffiSysRoot "libffi\LICENSE"
    if (-not (Test-Path -LiteralPath $libffiLicensePath -PathType Leaf)) {
        throw "The bundled libffi license is missing from libffi-sys $($libffiSys[0].version)."
    }

    $template = Get-Content -Raw -Encoding UTF8 (Join-Path $PSScriptRoot "NATIVE_NOTICES.txt")
    $notice = $template.Replace("{{LIBFFI_VERSION}}", [string]$libffi[0].version)
    $notice = $notice.Replace("{{LIBFFI_SYS_VERSION}}", [string]$libffiSys[0].version)
    $notice = $notice.Replace("{{LIBFFI_LICENSE_TEXT}}", (Get-Content -Raw -Encoding UTF8 $libffiLicensePath).Trim())
    if ($notice -match "\{\{[A-Z0-9_]+\}\}") {
        throw "An unresolved native notice placeholder remains."
    }
    return $notice.Trim()
}

<#
.SYNOPSIS
Builds one deterministic third-party notice from the dependency graphs shipped in a release archive.
#>
function New-ReleaseNotice {
    param(
        [object[]]$AboutData,
        [string]$Profile,
        [object[]]$Graphs,
        [string]$Programs,
        [switch]$IncludeBundledLibffi
    )

    $packageByKey = @{}
    foreach ($data in $AboutData) {
        foreach ($crate in $data.crates) {
            $package = $crate.package
            $packageByKey[(Get-PackageKey $package)] = $package
        }
    }
    $packages = @($packageByKey.Values | Sort-Object name, version, id)

    $lines = New-Object System.Collections.Generic.List[string]
    $lines.Add("BT third-party notices")
    $lines.Add("======================")
    $lines.Add("")
    $lines.Add("This file was generated for the $Profile release archive from Cargo.lock")
    $lines.Add("by tools/compliance/generate-third-party-licenses.ps1 using cargo-about")
    $lines.Add("$CargoAboutVersion. It covers only the resolved Rust dependency graphs for:")
    $lines.Add("")
    $lines.Add("- programs: $Programs")
    foreach ($graph in $Graphs) {
        $lines.Add("- $($graph.Programs): target $($graph.Target); features: $($graph.FeatureDescription)")
    }
    $lines.Add("")
    $lines.Add("The accompanying LICENSE-APACHE contains the Apache-2.0 text. When a")
    $lines.Add("dependency offers Apache-2.0 as one of several alternative licenses, BT")
    $lines.Add("selects Apache-2.0 for this binary redistribution. Conjunctive license")
    $lines.Add("expressions retain the additional required license texts below.")
    $lines.Add("When a license is carried in a source-file comment, only that comment is")
    $lines.Add("reproduced below; the unrelated implementation body is omitted.")
    $lines.Add("")
    $lines.Add("Resolved components")
    $lines.Add("===================")
    $lines.Add("")
    foreach ($package in $packages) {
        $license = Get-DistributionLicenseLabel ([string]$package.license)
        $source = Get-PackageSource $package
        $lines.Add("- $($package.name) $($package.version) | $license | $source")
    }
    $lines.Add("")

    $upstreamNotices = New-Object System.Collections.Generic.List[object]
    foreach ($package in $packages) {
        $packageRoot = Split-Path -Parent ([string]$package.manifest_path)
        $noticeFiles = @(Get-ChildItem -LiteralPath $packageRoot -File -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match '^NOTICE(?:\..+)?$' })
        foreach ($noticeFile in $noticeFiles) {
            $upstreamNotices.Add([pscustomobject]@{
                Package = $package
                FileName = $noticeFile.Name
                Text = Get-Content -Raw -Encoding UTF8 $noticeFile.FullName
            })
        }
    }
    if ($upstreamNotices.Count -ne 0) {
        $lines.Add("Required upstream notices")
        $lines.Add("=========================")
        $lines.Add("")
        foreach ($notice in $upstreamNotices) {
            $lines.Add("-------------------------------------------------------------------------------")
            $lines.Add("$($notice.Package.name) $($notice.Package.version) - $($notice.FileName)")
            $lines.Add("")
            $lines.Add($notice.Text.Trim())
            $lines.Add("")
        }
    }

    if ($IncludeBundledLibffi) {
        $lines.Add((Get-NativeNotice))
        $lines.Add("")
    }

    # cargo-about emits one result per target/feature graph. Group identical
    # license texts and package IDs so mixed-target archives stay deterministic
    # without repeating licenses shared by both programs.
    $licenseSectionsByKey = @{}
    foreach ($data in $AboutData) {
        foreach ($license in $data.licenses) {
            $licenseText = Get-DistributionLicenseText $license
            $sectionKey = "$($license.id)`0$($license.name)`0$licenseText"
            if (-not $licenseSectionsByKey.ContainsKey($sectionKey)) {
                $licenseSectionsByKey[$sectionKey] = [pscustomobject]@{
                    Name = [string]$license.name
                    Id = [string]$license.id
                    Text = $licenseText
                    Users = @{}
                }
            }
            $section = $licenseSectionsByKey[$sectionKey]
            foreach ($usage in $license.used_by) {
                $key = Get-PackageKey $usage.crate
                if (-not $packageByKey.ContainsKey($key)) {
                    continue
                }
                $package = $packageByKey[$key]
                if (Test-LicenseTextRequired ([string]$package.license) $license) {
                    $section.Users[$key] = $package
                }
            }
        }
    }

    $licenseSections = @($licenseSectionsByKey.Values |
        Where-Object { $_.Users.Count -ne 0 } |
        Sort-Object Id, Name, Text)
    if ($licenseSections.Count -ne 0) {
        $lines.Add("Retained license texts")
        $lines.Add("======================")
        $lines.Add("")
        $lines.Add("The following texts preserve license-specific copyright and attribution")
        $lines.Add("notices for dependencies not distributed solely under Apache-2.0.")
        $lines.Add("")
        foreach ($section in $licenseSections) {
            $lines.Add("-------------------------------------------------------------------------------")
            $lines.Add("$($section.Name) ($($section.Id))")
            $lines.Add("")
            $lines.Add("Used by:")
            foreach ($package in @($section.Users.Values | Sort-Object name, version, id)) {
                $lines.Add("- $($package.name) $($package.version) ($(Get-PackageSource $package))")
            }
            $lines.Add("")
            $lines.Add($section.Text.Trim())
            $lines.Add("")
        }
    }

    return $lines -join "`n"
}

function Write-Utf8 {
    param(
        [string]$Path,
        [string]$Content
    )

    $parent = Split-Path -Parent $Path
    if ($parent -and -not (Test-Path -LiteralPath $parent -PathType Container)) {
        New-Item -ItemType Directory -Path $parent -Force | Out-Null
    }
    [System.IO.File]::WriteAllText($Path, (Convert-ToLf $Content), (New-Object System.Text.UTF8Encoding($false)))
}

function Write-OrCheckUtf8 {
    param(
        [string]$Path,
        [string]$Content
    )

    $normalized = Convert-ToLf $Content
    if ($Check) {
        if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
            throw "Generated notice file is missing: $Path"
        }
        $existing = Convert-ToLf (Get-Content -Raw -Encoding UTF8 $Path)
        if ($existing -cne $normalized) {
            throw "Generated notice file is stale: $Path"
        }
        return
    }

    Write-Utf8 -Path $Path -Content $normalized
}

if ($ReleaseProfile -and -not $OutputPath) {
    throw "-OutputPath is required with -ReleaseProfile."
}
if ($OutputPath -and -not $ReleaseProfile) {
    throw "-ReleaseProfile is required with -OutputPath."
}
if ($Check -and $ReleaseProfile) {
    throw "Use -Check and -ReleaseProfile in separate invocations."
}

$versionOutput = (& cargo about --version | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $versionOutput -notmatch "cargo-about $([regex]::Escape($CargoAboutVersion))($|\s)") {
    throw "cargo-about $CargoAboutVersion is required; found: $versionOutput"
}

Assert-DistributionInventory

if ($ReleaseProfile) {
    $profiles = @{
        "windows-x64" = @{
            Programs = "bt.exe and bt-app.exe"
            IncludeBundledLibffi = $true
            Graphs = @(
                @{
                    Programs = "bt.exe and bt-app.exe"
                    Target = "x86_64-pc-windows-msvc"
                    Features = @("desktop")
                    NoDefaultFeatures = $false
                    FeatureDescription = "default (extensions, ffi) + desktop"
                }
            )
        }
        "linux-x64" = @{
            Programs = "bt and bt-app"
            IncludeBundledLibffi = $false
            Graphs = @(
                @{
                    Programs = "bt"
                    Target = "x86_64-unknown-linux-musl"
                    Features = @("extensions")
                    NoDefaultFeatures = $true
                    FeatureDescription = "extensions; default features disabled"
                },
                @{
                    Programs = "bt-app"
                    Target = "x86_64-unknown-linux-gnu"
                    Features = @("desktop")
                    NoDefaultFeatures = $false
                    FeatureDescription = "default (extensions, ffi) + desktop"
                }
            )
        }
        "macos-arm64" = @{
            Programs = "bt and bt-app"
            IncludeBundledLibffi = $false
            Graphs = @(
                @{
                    Programs = "bt and bt-app"
                    Target = "aarch64-apple-darwin"
                    Features = @("desktop")
                    NoDefaultFeatures = $false
                    FeatureDescription = "default (extensions, ffi) + desktop"
                }
            )
        }
        "macos-x64" = @{
            Programs = "bt and bt-app"
            IncludeBundledLibffi = $false
            Graphs = @(
                @{
                    Programs = "bt and bt-app"
                    Target = "x86_64-apple-darwin"
                    Features = @("desktop")
                    NoDefaultFeatures = $false
                    FeatureDescription = "default (extensions, ffi) + desktop"
                }
            )
        }
    }
    $profile = $profiles[$ReleaseProfile]
    $jsonTemps = New-Object System.Collections.Generic.List[string]
    $aboutData = New-Object System.Collections.Generic.List[object]
    try {
        foreach ($graph in $profile.Graphs) {
            $jsonTemp = [System.IO.Path]::GetTempFileName()
            $jsonTemps.Add($jsonTemp)
            Invoke-CargoAbout `
                -ManifestPath (Join-Path $RepoRoot "Cargo.toml") `
                -Target $graph.Target `
                -Destination $jsonTemp `
                -Features $graph.Features `
                -NoDefaultFeatures:$graph.NoDefaultFeatures `
                -Json
            $aboutData.Add((Get-Content -Raw -Encoding UTF8 $jsonTemp | ConvertFrom-Json))
        }
        $notice = New-ReleaseNotice `
            -AboutData $aboutData.ToArray() `
            -Profile $ReleaseProfile `
            -Graphs $profile.Graphs `
            -Programs $profile.Programs `
            -IncludeBundledLibffi:$profile.IncludeBundledLibffi
        Write-Utf8 -Path $OutputPath -Content $notice
    } finally {
        foreach ($jsonTemp in $jsonTemps) {
            Remove-Item -LiteralPath $jsonTemp -Force -ErrorAction SilentlyContinue
        }
    }

    Write-Host "Release notice generated: $OutputPath"
    return
}

$sqliteTemp = [System.IO.Path]::GetTempFileName()
try {
    Invoke-CargoAbout `
        -ManifestPath (Join-Path $SqliteRoot "Cargo.toml") `
        -Target "wasm32-wasip1" `
        -Destination $sqliteTemp `
        -AllFeatures

    $sqliteCargo = Get-Content -Raw -Encoding UTF8 $sqliteTemp
    $sqlitePublicDomain = Get-Content -Raw -Encoding UTF8 (Join-Path $PSScriptRoot "SQLITE_PUBLIC_DOMAIN.txt")
    $sqliteNotice = @"
BT sqlite extension third-party licenses
========================================

This file is generated from the extension's locked Cargo dependency graph by
tools/compliance/generate-third-party-licenses.ps1 using cargo-about
$CargoAboutVersion. The extension's own source is Copyright 2026 Lifeng Yan
and is available under MIT OR Apache-2.0; see LICENSE-MIT, LICENSE-APACHE, and
COPYRIGHT in this package.

$sqlitePublicDomain

$sqliteCargo
"@
    Write-OrCheckUtf8 -Path $SqliteOutput -Content $sqliteNotice
} finally {
    Remove-Item -LiteralPath $sqliteTemp -Force -ErrorAction SilentlyContinue
}

if ($Check) {
    Write-Host "Tracked SQLite extension notices and the distribution inventory are current."
} else {
    Write-Host "Tracked SQLite extension notices generated."
}
