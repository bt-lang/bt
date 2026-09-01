param(
    [string]$BtPath = "",
    [int]$TimeoutSeconds = 30,
    [switch]$Build,
    [string]$Output = ""
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$ExeSuffix = ""
if ($env:OS -eq "Windows_NT") {
    $ExeSuffix = ".exe"
}

if ([string]::IsNullOrWhiteSpace($BtPath)) {
    $BtPath = Join-Path $RepoRoot ("target/debug/bt" + $ExeSuffix)
}

if ([string]::IsNullOrWhiteSpace($Output)) {
    $Output = Join-Path $RepoRoot "target/quality/examples-regression.json"
}

# Convert arguments for ProcessStartInfo.Arguments.
function ConvertTo-CommandArgument {
    param([string]$Value)

    if ($Value -notmatch '[\s"]') {
        return $Value
    }

    return '"' + ($Value -replace '"', '\"') + '"'
}

# Run one BT example and capture output.
function Invoke-BtScript {
    param(
        [string]$ScriptPath,
        [int]$TimeoutSeconds,
        [System.Collections.IDictionary]$Environment = $null
    )

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $BtPath
    $psi.Arguments = ConvertTo-CommandArgument $ScriptPath
    $psi.WorkingDirectory = [string]$RepoRoot
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    $psi.EnvironmentVariables.Remove("BT_PERMISSION_ALLOW")
    $psi.EnvironmentVariables.Remove("BT_PERMISSION_DENY")
    if ($null -ne $Environment) {
        foreach ($key in $Environment.Keys) {
            $psi.EnvironmentVariables[$key] = [string]$Environment[$key]
        }
    }

    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $psi

    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    [void]$process.Start()
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()

    while (-not $process.HasExited) {
        if ($watch.Elapsed.TotalSeconds -gt $TimeoutSeconds) {
            try {
                $process.Kill()
            } catch {
            }
            throw "example timeout: $ScriptPath"
        }
        Start-Sleep -Milliseconds 20
    }

    $watch.Stop()
    $stdout = $stdoutTask.Result
    $stderr = $stderrTask.Result

    if ($process.ExitCode -ne 0) {
        throw "example failed: $ScriptPath`n$stdout`n$stderr"
    }

    [ordered]@{
        script = $ScriptPath
        elapsed_ms = [Math]::Round($watch.Elapsed.TotalMilliseconds, 3)
        stdout = $stdout.Trim()
        stderr = $stderr.Trim()
    }
}

if ($Build) {
    Push-Location $RepoRoot
    try {
        cargo build --bin bt
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build --bin bt failed"
        }
    } finally {
        Pop-Location
    }
}

if (-not (Test-Path $BtPath)) {
    throw "bt executable not found: $BtPath; run cargo build --bin bt or pass -Build."
}

$Cases = @(
    [ordered]@{ name = "process_pipe"; script = "examples/process-pipe.bt"; contains = @("BT_PIPE", "BT_ERR", "stdout") },
    [ordered]@{ name = "runtime_stats"; script = "examples/runtime-stats.bt"; contains = @("256") },
    [ordered]@{ name = "runtime_pools_stats"; script = "examples/runtime-pools-stats.bt"; contains = @("32", "8", "0") },
    [ordered]@{
        name = "permission_stats"
        script = "examples/permission-stats.bt"
        env = [ordered]@{
            BT_PERMISSION_ALLOW = "fs,env"
            BT_PERMISSION_DENY = "process"
        }
        contains = @("permission-stats:fs,env|process|0")
    },
    [ordered]@{ name = "bytes_modbus"; script = "examples/bytes-modbus.bt"; contains = @("000100000006010300000002", "10") },
    [ordered]@{ name = "net_phase3_stats"; script = "examples/net-phase3-stats.bt"; contains = @("event_queue_bounded:true", "event_queue_limit:4096") },
    [ordered]@{ name = "compat_block_return"; script = "examples/compat/block-return.bt"; contains = @("compat-block-return:ok") },
    [ordered]@{ name = "compat_empty_null"; script = "examples/compat/empty-null.bt"; contains = @("empty", "compat-empty-null:ok") },
    [ordered]@{ name = "compat_chain_class_closure"; script = "examples/compat/chain-class-closure.bt"; contains = @("compat-chain-class-closure:ok") },
    [ordered]@{ name = "compat_destructure_loop"; script = "examples/compat/destructure-loop.bt"; contains = @("compat-destructure-loop:ok") },
    [ordered]@{ name = "compat_snake_case_stdlib"; script = "examples/compat/snake-case-stdlib.bt"; contains = @("compat-snake-case-stdlib:ok") }
)

$results = @()

foreach ($case in $Cases) {
    $scriptPath = Join-Path $RepoRoot $case.script
    if (-not (Test-Path $scriptPath)) {
        throw "example missing: $scriptPath"
    }

    Write-Host "example $($case.name)"
    $caseEnvironment = $null
    if ($case.Contains("env")) {
        $caseEnvironment = $case["env"]
    }
    $result = Invoke-BtScript -ScriptPath $scriptPath -TimeoutSeconds $TimeoutSeconds -Environment $caseEnvironment

    foreach ($needle in $case.contains) {
        if ($result.stdout -notlike "*$needle*") {
            throw "unexpected example output: $($case.script), missing $needle, actual: $($result.stdout)"
        }
    }

    $results += [ordered]@{
        name = $case.name
        script = $case.script
        elapsed_ms = $result.elapsed_ms
        stdout = $result.stdout
    }
}

$payload = [ordered]@{
    generated_at = (Get-Date).ToUniversalTime().ToString("o")
    bt_path = $BtPath
    cases = $results
}

$outputDir = Split-Path -Parent $Output
New-Item -ItemType Directory -Force $outputDir | Out-Null
$payload | ConvertTo-Json -Depth 8 | Set-Content -Encoding UTF8 $Output

$results | ForEach-Object { [pscustomobject]$_ } | Format-Table name, elapsed_ms -AutoSize
Write-Host "examples result: $Output"
