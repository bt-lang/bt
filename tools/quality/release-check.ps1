[CmdletBinding()]
param(
    [switch]$SkipDesktop,
    [switch]$SkipExamples,
    [switch]$SkipBenchmarks,
    [int]$BenchmarkIterations = 5
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$ExeSuffix = if ($env:OS -eq "Windows_NT") { ".exe" } else { "" }
$TempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("bt-release-gate-" + [guid]::NewGuid().ToString("N"))

function Invoke-GateCommand {
    param(
        [string]$Title,
        [scriptblock]$Command
    )

    Write-Host ""
    Write-Host "== $Title =="
    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "Release gate failed: $Title"
    }
}

function Assert-ToolVersion {
    param(
        [string]$Name,
        [scriptblock]$Command,
        [string]$Pattern
    )

    $output = (& $Command 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $output -notmatch $Pattern) {
        throw "$Name is missing or has an unsupported version. Found: $output"
    }
    Write-Host "${Name}: $output"
}

New-Item -ItemType Directory -Path $TempRoot | Out-Null
Push-Location $RepoRoot
try {
    Assert-ToolVersion -Name "cargo-about" -Command { cargo about --version } -Pattern 'cargo-about 0\.9\.2($|\s)'
    Assert-ToolVersion -Name "cargo-audit" -Command { cargo audit --version } -Pattern 'cargo-audit(?:-audit)? 0\.22\.2($|\s)'
    Assert-ToolVersion -Name "Gitleaks" -Command { gitleaks version } -Pattern '8\.30\.1'

    Invoke-GateCommand -Title "cargo fmt --all -- --check" -Command { cargo fmt --all -- --check }
    Invoke-GateCommand -Title "locked root metadata" -Command { cargo metadata --locked --all-features --format-version 1 | Out-Null }
    Invoke-GateCommand -Title "locked SQLite metadata" -Command { cargo metadata --locked --all-features --format-version 1 --manifest-path examples/extension-development/sqlite/Cargo.toml | Out-Null }
    Invoke-GateCommand -Title "third-party license inventory" -Command { tools/compliance/generate-third-party-licenses.ps1 -Check }
    Invoke-GateCommand -Title "SQLite package consistency" -Command { tools/compliance/verify-sqlite-packages.ps1 }
    Invoke-GateCommand -Title "exact RustSec advisory policy" -Command { tools/compliance/verify-rustsec-policy.ps1 }
    Invoke-GateCommand -Title "cargo test --locked" -Command { cargo test --locked }
    Invoke-GateCommand -Title "all-target, all-feature check" -Command { cargo check --locked --workspace --all-targets --all-features }

    if (-not $SkipDesktop) {
        Invoke-GateCommand -Title "cargo test --locked --features desktop" -Command { cargo test --locked --features desktop }
    }

    Invoke-GateCommand -Title "debug CLI build" -Command { cargo build --locked --bin bt }
    Invoke-GateCommand -Title "release CLI build" -Command { cargo build --locked --release --bin bt }
    if (-not $SkipDesktop) {
        Invoke-GateCommand -Title "release desktop build" -Command { cargo build --locked --release --features desktop --bin bt_app }
    }

    $debugBt = Join-Path $RepoRoot ("target/debug/bt" + $ExeSuffix)
    $releaseBt = Join-Path $RepoRoot ("target/release/bt" + $ExeSuffix)
    if (-not $SkipExamples) {
        Invoke-GateCommand -Title "critical example regressions" -Command { tools/quality/run-examples.ps1 -BtPath $debugBt }
    }
    if (-not $SkipBenchmarks) {
        Invoke-GateCommand -Title "performance baseline" -Command {
            tools/quality/run-benchmarks.ps1 -BtPath $releaseBt -Iterations $BenchmarkIterations -Warmup 1
        }
    }

    $publicExport = Join-Path $TempRoot "public-source"
    Invoke-GateCommand -Title "no-history public export" -Command {
        tools/quality/export-public-source.ps1 -Destination $publicExport
    }
    Invoke-GateCommand -Title "Gitleaks public file-tree scan" -Command {
        gitleaks dir $publicExport --redact --no-banner --exit-code 1
    }

    $expanded = Join-Path $TempRoot "expanded-archives"
    Invoke-GateCommand -Title "two-level package expansion" -Command {
        tools/compliance/expand-release-archives.ps1 -Source $publicExport -Destination $expanded -MaxDepth 2
    }
    Invoke-GateCommand -Title "Gitleaks expanded-package scan" -Command {
        gitleaks dir $expanded --redact --no-banner --exit-code 1
    }

    if (-not $SkipDesktop) {
        $stage = Join-Path $TempRoot "archive"
        New-Item -ItemType Directory -Path $stage | Out-Null
        Copy-Item -LiteralPath README.md,README.zh-CN.md,LICENSE-APACHE,LICENSE-MIT,COPYRIGHT -Destination $stage

        if ($env:OS -eq "Windows_NT") {
            Copy-Item -LiteralPath $releaseBt -Destination $stage
            Copy-Item -LiteralPath (Join-Path $RepoRoot "target/release/bt_app.exe") -Destination $stage
            Invoke-GateCommand -Title "Windows release third-party notice" -Command {
                tools/compliance/generate-third-party-licenses.ps1 `
                    -ReleaseProfile windows-x64 `
                    -OutputPath (Join-Path $stage "THIRD-PARTY-NOTICES.txt")
            }
            $archive = Join-Path $TempRoot "bt-windows-x64.zip"
            Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $archive
            Invoke-GateCommand -Title "release archive contents" -Command {
                tools/compliance/verify-release-archive.ps1 `
                    -ArchivePath $archive `
                    -Platform windows `
                    -ReleaseProfile windows-x64
            }
        } else {
            $hostLine = (& rustc -vV | Select-String '^host:' | Out-String).Trim()
            if ($hostLine -match 'aarch64-apple-darwin') {
                $profile = "macos-arm64"
                $archiveBt = $releaseBt
            } elseif ($hostLine -match 'x86_64-apple-darwin') {
                $profile = "macos-x64"
                $archiveBt = $releaseBt
            } else {
                $profile = "linux-x64"
                Invoke-GateCommand -Title "Linux musl release CLI build" -Command {
                    cargo build --locked --release `
                        --target x86_64-unknown-linux-musl `
                        --no-default-features `
                        --features extensions `
                        --bin bt
                }
                $archiveBt = Join-Path $RepoRoot "target/x86_64-unknown-linux-musl/release/bt"
            }
            Copy-Item -LiteralPath $archiveBt -Destination $stage
            Copy-Item -LiteralPath (Join-Path $RepoRoot "target/release/bt_app") -Destination $stage
            Invoke-GateCommand -Title "$profile release third-party notice" -Command {
                tools/compliance/generate-third-party-licenses.ps1 `
                    -ReleaseProfile $profile `
                    -OutputPath (Join-Path $stage "THIRD-PARTY-NOTICES.txt")
            }
            $archive = Join-Path $TempRoot "bt-local.tar.gz"
            Invoke-GateCommand -Title "create local release archive" -Command { tar -czf $archive -C $stage . }
            Invoke-GateCommand -Title "release archive contents" -Command {
                tools/compliance/verify-release-archive.ps1 `
                    -ArchivePath $archive `
                    -Platform unix `
                    -ReleaseProfile $profile
            }
        }
    }
} finally {
    Pop-Location
    Remove-Item -LiteralPath $TempRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host "Release gate passed."
