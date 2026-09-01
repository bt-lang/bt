[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Source,

    [Parameter(Mandatory = $true)]
    [string]$Destination,

    [ValidateRange(1, 4)]
    [int]$MaxDepth = 2,

    [ValidateRange(1, 4096)]
    [int]$MaxArchiveMiB = 512,

    [ValidateRange(1, 8192)]
    [int]$MaxExpandedMiB = 2048
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

$SourcePath = [System.IO.Path]::GetFullPath($Source)
$DestinationPath = [System.IO.Path]::GetFullPath($Destination)
if (-not (Test-Path -LiteralPath $SourcePath)) {
    throw "Archive scan source does not exist: $SourcePath"
}
if ($DestinationPath -eq $SourcePath -or $DestinationPath.StartsWith($SourcePath + [System.IO.Path]::DirectorySeparatorChar, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Archive expansion destination must be outside the source tree."
}
if (Test-Path -LiteralPath $DestinationPath) {
    if (@(Get-ChildItem -LiteralPath $DestinationPath -Force).Count -ne 0) {
        throw "Archive expansion destination must be empty: $DestinationPath"
    }
} else {
    New-Item -ItemType Directory -Path $DestinationPath | Out-Null
}

$archiveLimit = [int64]$MaxArchiveMiB * 1024 * 1024
$expandedLimit = [int64]$MaxExpandedMiB * 1024 * 1024
$totalExpanded = [int64]0
$archiveIndex = 0
$queue = New-Object 'System.Collections.Generic.Queue[object]'

function Test-ZipLikeArchive {
    param([string]$Path)
    return [System.IO.Path]::GetExtension($Path).ToLowerInvariant() -in @(".btr", ".bts", ".zip")
}

function Add-ArchivesFromDirectory {
    param(
        [string]$Directory,
        [int]$Depth
    )
    foreach ($file in Get-ChildItem -LiteralPath $Directory -Recurse -File) {
        if (Test-ZipLikeArchive $file.FullName) {
            $queue.Enqueue([pscustomobject]@{ Path = $file.FullName; Depth = $Depth })
        }
    }
}

if (Test-Path -LiteralPath $SourcePath -PathType Leaf) {
    if (-not (Test-ZipLikeArchive $SourcePath)) {
        throw "Only .bts, .btr, and .zip archives are supported."
    }
    $queue.Enqueue([pscustomobject]@{ Path = $SourcePath; Depth = 1 })
} else {
    Add-ArchivesFromDirectory -Directory $SourcePath -Depth 1
}

while ($queue.Count -gt 0) {
    $item = $queue.Dequeue()
    if ($item.Depth -gt $MaxDepth) {
        continue
    }

    $archive = Get-Item -LiteralPath $item.Path
    if ($archive.Length -gt $archiveLimit) {
        throw "Archive exceeds the compressed size limit: $($archive.FullName)"
    }

    $archiveIndex++
    $extractRoot = Join-Path $DestinationPath ("depth-{0}\archive-{1:D4}" -f $item.Depth, $archiveIndex)
    New-Item -ItemType Directory -Path $extractRoot | Out-Null
    $extractPrefix = $extractRoot.TrimEnd('\', '/') + [System.IO.Path]::DirectorySeparatorChar

    $zip = [System.IO.Compression.ZipFile]::OpenRead($archive.FullName)
    try {
        foreach ($entry in $zip.Entries) {
            if ([string]::IsNullOrWhiteSpace($entry.FullName) -or $entry.FullName.EndsWith('/')) {
                continue
            }
            if ([System.IO.Path]::IsPathRooted($entry.FullName)) {
                throw "Archive contains a rooted entry: $($entry.FullName)"
            }
            $nativeEntryName = $entry.FullName.Replace([char]'/', [System.IO.Path]::DirectorySeparatorChar)
            $entryPath = [System.IO.Path]::GetFullPath((Join-Path $extractRoot $nativeEntryName))
            if (-not $entryPath.StartsWith($extractPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
                throw "Archive entry escapes the extraction root: $($entry.FullName)"
            }
            if (Test-Path -LiteralPath $entryPath) {
                throw "Archive contains a duplicate entry: $($entry.FullName)"
            }

            $totalExpanded += [int64]$entry.Length
            if ($entry.Length -gt $archiveLimit -or $totalExpanded -gt $expandedLimit) {
                throw "Expanded archive data exceeds the configured limit."
            }

            $parent = Split-Path -Parent $entryPath
            if (-not (Test-Path -LiteralPath $parent)) {
                New-Item -ItemType Directory -Path $parent -Force | Out-Null
            }
            $input = $entry.Open()
            $output = [System.IO.File]::Open($entryPath, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
            try {
                $input.CopyTo($output)
            } finally {
                $output.Dispose()
                $input.Dispose()
            }
        }
    } finally {
        $zip.Dispose()
    }

    if ($item.Depth -lt $MaxDepth) {
        Add-ArchivesFromDirectory -Directory $extractRoot -Depth ($item.Depth + 1)
    }
}

Write-Host "Expanded $archiveIndex archive(s) through depth $MaxDepth."
Write-Host "Expanded bytes: $totalExpanded"
