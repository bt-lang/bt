[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ArchivePath,

    [ValidateSet("windows", "unix")]
    [string]$Platform,

    [Parameter(Mandatory = $true)]
    [ValidateSet("windows-x64", "linux-x64", "macos-arm64", "macos-x64")]
    [string]$ReleaseProfile
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"

$Archive = (Resolve-Path $ArchivePath).Path
$TempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("bt-release-verify-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $TempRoot | Out-Null
try {
    if ([System.IO.Path]::GetExtension($Archive).ToLowerInvariant() -eq ".zip") {
        Expand-Archive -LiteralPath $Archive -DestinationPath $TempRoot
    } else {
        if (-not (Get-Command tar -ErrorAction SilentlyContinue)) {
            throw "tar is required to verify non-ZIP release archives."
        }
        tar -xf $Archive -C $TempRoot
        if ($LASTEXITCODE -ne 0) {
            throw "Unable to extract release archive: $Archive"
        }
    }

    if (Test-Path -LiteralPath (Join-Path $TempRoot ".git")) {
        throw "Release archive must not contain Git metadata."
    }

    $programs = if ($Platform -eq "windows") { @("bt.exe", "bt_app.exe") } else { @("bt", "bt_app") }
    $required = @(
        "README.md",
        "README.zh-CN.md",
        "LICENSE-APACHE",
        "LICENSE-MIT",
        "COPYRIGHT",
        "THIRD-PARTY-NOTICES.txt"
    ) + $programs
    foreach ($name in $required) {
        $matches = @(Get-ChildItem -LiteralPath $TempRoot -Recurse -File -Filter $name)
        if ($matches.Count -ne 1 -or $matches[0].Length -eq 0) {
            throw "Release archive must contain exactly one non-empty $name file."
        }
    }

    $expectedPlatform = if ($ReleaseProfile -eq "windows-x64") { "windows" } else { "unix" }
    if ($Platform -ne $expectedPlatform) {
        throw "Release profile $ReleaseProfile does not match archive platform $Platform."
    }
    $noticePath = (Get-ChildItem -LiteralPath $TempRoot -Recurse -File -Filter "THIRD-PARTY-NOTICES.txt").FullName
    $notice = Get-Content -Raw -Encoding UTF8 $noticePath
    if ($notice -notmatch "generated for the $([regex]::Escape($ReleaseProfile)) release archive") {
        throw "Release archive contains a third-party notice for the wrong profile."
    }

    $unexpected = @(Get-ChildItem -LiteralPath $TempRoot -Recurse -Force | Where-Object { $_.Name -eq ".git" })
    if ($unexpected.Count -ne 0) {
        throw "Release archive contains nested Git metadata."
    }
} finally {
    Remove-Item -LiteralPath $TempRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "Release archive contents verified: $Archive"
