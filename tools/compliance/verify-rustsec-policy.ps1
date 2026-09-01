[CmdletBinding()]
param(
    [switch]$NoFetch
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$PolicyPath = Join-Path $RepoRoot ".cargo\audit-informational-policy.json"

if (-not (Test-Path -LiteralPath $PolicyPath -PathType Leaf)) {
    throw "RustSec informational policy is missing: $PolicyPath"
}

$Policy = Get-Content -LiteralPath $PolicyPath -Raw -Encoding UTF8 | ConvertFrom-Json
if ($Policy.schema_version -ne 1) {
    throw "Unsupported RustSec informational policy schema: $($Policy.schema_version)"
}

$Expected = @{}
$Today = [DateTime]::UtcNow.Date
foreach ($Group in @($Policy.groups)) {
    foreach ($Field in @("name", "category", "rationale", "review_due", "removal_condition")) {
        if ([string]::IsNullOrWhiteSpace([string]$Group.$Field)) {
            throw "RustSec policy group is missing '$Field': $($Group.name)"
        }
    }
    if (@($Group.features).Count -eq 0 -or @($Group.targets).Count -eq 0 -or @($Group.dependency_paths).Count -eq 0) {
        throw "RustSec policy group must document features, targets, and dependency paths: $($Group.name)"
    }

    $ReviewDue = [DateTime]::ParseExact(
        [string]$Group.review_due,
        "yyyy-MM-dd",
        [Globalization.CultureInfo]::InvariantCulture
    )
    if ($ReviewDue.Date -lt $Today) {
        throw "RustSec policy review is overdue for '$($Group.name)': $($Group.review_due)"
    }

    foreach ($Advisory in @($Group.advisories)) {
        foreach ($Field in @("id", "package", "version")) {
            if ([string]::IsNullOrWhiteSpace([string]$Advisory.$Field)) {
                throw "RustSec policy advisory is missing '$Field' in group '$($Group.name)'"
            }
        }
        $Key = "$($Advisory.id)|$($Group.category)|$($Advisory.package)|$($Advisory.version)"
        if ($Expected.ContainsKey($Key)) {
            throw "Duplicate RustSec policy advisory: $Key"
        }
        $Expected[$Key] = $Group.name
    }
}

$AuditArguments = @("audit", "--json")
if ($NoFetch) {
    $AuditArguments += "--no-fetch"
}

Push-Location $RepoRoot
try {
    $AuditJson = (& cargo @AuditArguments | Out-String)
    $AuditExitCode = $LASTEXITCODE
}
finally {
    Pop-Location
}

if ([string]::IsNullOrWhiteSpace($AuditJson)) {
    throw "cargo audit did not produce JSON output (exit $AuditExitCode)"
}
$Audit = $AuditJson | ConvertFrom-Json

if ([int]$Audit.vulnerabilities.count -ne 0) {
    $VulnerabilityIds = @($Audit.vulnerabilities.list | ForEach-Object { $_.advisory.id }) -join ", "
    throw "cargo audit found $($Audit.vulnerabilities.count) vulnerabilities: $VulnerabilityIds"
}
if ($AuditExitCode -ne 0) {
    throw "cargo audit failed with exit code $AuditExitCode"
}

$Actual = @{}
foreach ($WarningCategory in @($Audit.warnings.PSObject.Properties)) {
    foreach ($Warning in @($WarningCategory.Value)) {
        $AdvisoryId = if ($null -ne $Warning.advisory -and $null -ne $Warning.advisory.id) {
            [string]$Warning.advisory.id
        }
        else {
            "NO-ADVISORY-ID"
        }
        $Key = "$AdvisoryId|$($WarningCategory.Name)|$($Warning.package.name)|$($Warning.package.version)"
        $Actual[$Key] = $true
    }
}

$Unexpected = @($Actual.Keys | Where-Object { -not $Expected.ContainsKey($_) } | Sort-Object)
$Missing = @($Expected.Keys | Where-Object { -not $Actual.ContainsKey($_) } | Sort-Object)
if ($Unexpected.Count -ne 0 -or $Missing.Count -ne 0) {
    $Parts = @("RustSec informational warning set changed and requires policy review.")
    if ($Unexpected.Count -ne 0) {
        $Parts += "Unexpected: $($Unexpected -join ', ')"
    }
    if ($Missing.Count -ne 0) {
        $Parts += "Missing: $($Missing -join ', ')"
    }
    throw ($Parts -join [Environment]::NewLine)
}

Write-Host "RustSec policy verified: 0 vulnerabilities and $($Actual.Count) exact informational warnings."
