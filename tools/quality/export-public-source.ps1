[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Destination,

    [switch]$InitializeRepository,

    [switch]$CreateInitialCommit,

    [string]$InitialCommitMessage = "Initial open-source release"
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$DestinationPath = [System.IO.Path]::GetFullPath($Destination)
$DirectorySeparator = [System.IO.Path]::DirectorySeparatorChar
$RepoPrefix = $RepoRoot.TrimEnd('\', '/') + $DirectorySeparator

if ($CreateInitialCommit -and -not $InitializeRepository) {
    throw "CreateInitialCommit requires InitializeRepository."
}

if ($DestinationPath.Equals($RepoRoot, [System.StringComparison]::OrdinalIgnoreCase) -or
    $DestinationPath.StartsWith($RepoPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "The destination must be outside the source repository."
}

foreach ($commandName in @("git", "tar")) {
    if (-not (Get-Command $commandName -ErrorAction SilentlyContinue)) {
        throw "Required command is not available: $commandName"
    }
}

if (Test-Path -LiteralPath $DestinationPath) {
    $destinationItem = Get-Item -LiteralPath $DestinationPath -Force
    if (($destinationItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "The destination must not be a reparse point: $DestinationPath"
    }
    $existingItems = @(Get-ChildItem -LiteralPath $DestinationPath -Force)
    if ($existingItems.Count -ne 0) {
        throw "The destination already exists and is not empty: $DestinationPath"
    }
} else {
    New-Item -ItemType Directory -Path $DestinationPath | Out-Null
}

Push-Location $RepoRoot
try {
    $status = @(git status --porcelain=v1 --untracked-files=all)
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to inspect the source repository status."
    }
    if ($status.Count -ne 0) {
        throw "The source repository must be clean before creating a public export."
    }

    $sourceCommit = (git rev-parse --verify HEAD).Trim()
    if ($LASTEXITCODE -ne 0 -or -not $sourceCommit) {
        throw "Unable to resolve the source commit."
    }

    $archivePath = Join-Path ([System.IO.Path]::GetTempPath()) ("bt-public-" + [guid]::NewGuid().ToString("N") + ".tar")
    try {
        git archive --format=tar --output=$archivePath HEAD
        if ($LASTEXITCODE -ne 0) {
            throw "git archive failed."
        }

        tar -xf $archivePath -C $DestinationPath
        if ($LASTEXITCODE -ne 0) {
            throw "Unable to extract the public source archive."
        }
    } finally {
        if (Test-Path -LiteralPath $archivePath) {
            Remove-Item -LiteralPath $archivePath -Force
        }
    }
} finally {
    Pop-Location
}

if (Test-Path -LiteralPath (Join-Path $DestinationPath ".git")) {
    throw "The exported source unexpectedly contains Git metadata."
}

$requiredFiles = @(
    "Cargo.lock",
    "Cargo.toml",
    "CODE_OF_CONDUCT.md",
    "COPYRIGHT",
    "LICENSE-APACHE",
    "LICENSE-MIT",
    "README.md",
    "SECURITY.md",
    "examples/extension-development/extensions/sqlite.bts",
    "examples/extension-development/sqlite/sqlite-1.0.0.bts"
)
foreach ($relativePath in $requiredFiles) {
    if (-not (Test-Path -LiteralPath (Join-Path $DestinationPath $relativePath) -PathType Leaf)) {
        throw "The public export is missing required file: $relativePath"
    }
}

if ($InitializeRepository) {
    Push-Location $DestinationPath
    try {
        git init --initial-branch=main
        if ($LASTEXITCODE -ne 0) {
            throw "Unable to initialize the public staging repository."
        }

        if ($CreateInitialCommit) {
            # The export already contains only files tracked by the reviewed source commit. Force
            # addition so historically tracked release packages remain present even when the
            # public repository's ignore rules correctly ignore newly generated package files.
            git add --force --all
            if ($LASTEXITCODE -ne 0) {
                throw "Unable to stage the public source tree."
            }

            git commit -m $InitialCommitMessage
            if ($LASTEXITCODE -ne 0) {
                throw "Unable to create the public staging repository's initial commit."
            }
        }
    } finally {
        Pop-Location
    }
}

Write-Host "Public source export created."
Write-Host "Source commit: $sourceCommit"
Write-Host "Destination: $DestinationPath"
