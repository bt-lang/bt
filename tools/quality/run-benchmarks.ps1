param(
    [string]$BtPath = "",
    [int]$Iterations = 7,
    [int]$Warmup = 1,
    [int]$TimeoutSeconds = 30,
    [int]$WebRequests = 80,
    [int]$WebWarmupRequests = 5,
    [switch]$Build,
    [string]$Output = ""
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"

Add-Type -AssemblyName System.Net.Http

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$ExeSuffix = ""
if ($env:OS -eq "Windows_NT") {
    $ExeSuffix = ".exe"
}

if ([string]::IsNullOrWhiteSpace($BtPath)) {
    $BtPath = Join-Path $RepoRoot ("target/release/bt" + $ExeSuffix)
}

if ([string]::IsNullOrWhiteSpace($Output)) {
    $Output = Join-Path $RepoRoot "target/quality/benchmark-baseline.json"
}

function ConvertTo-CommandArgument {
    param([string]$Value)

    if ($Value -notmatch '[\s"]') {
        return $Value
    }

    return '"' + ($Value -replace '"', '\"') + '"'
}

function Start-BtResident {
    param(
        [string]$ScriptPath,
        [string[]]$Arguments = @()
    )

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $BtPath
    $psi.Arguments = ((@($ScriptPath) + $Arguments | ForEach-Object { ConvertTo-CommandArgument $_ }) -join " ")
    $psi.WorkingDirectory = [string]$RepoRoot
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true

    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $psi
    [void]$process.Start()
    [void]$process.StandardOutput.ReadToEndAsync()
    [void]$process.StandardError.ReadToEndAsync()
    return $process
}

function Stop-BtProcess {
    param([System.Diagnostics.Process]$Process)

    if ($null -ne $Process -and -not $Process.HasExited) {
        try {
            $Process.Kill()
        } catch {
        }
    }
}

function Get-ProcessProbe {
    param([System.Diagnostics.Process]$Process)

    if ($null -eq $Process) {
        return [ordered]@{
            working_set_mb = 0.0
            threads = 0
        }
    }

    if ($Process.HasExited) {
        return [ordered]@{
            working_set_mb = 0.0
            threads = 0
        }
    }

    try {
        $Process.Refresh()
        return [ordered]@{
            working_set_mb = [Math]::Round($Process.WorkingSet64 / 1MB, 2)
            threads = $Process.Threads.Count
        }
    } catch {
        return [ordered]@{
            working_set_mb = 0.0
            threads = 0
        }
    }
}

function Invoke-MeasuredProcess {
    param(
        [string]$FilePath,
        [string[]]$Arguments,
        [int]$TimeoutSeconds
    )

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $FilePath
    $psi.Arguments = (($Arguments | ForEach-Object { ConvertTo-CommandArgument $_ }) -join " ")
    $psi.WorkingDirectory = [string]$RepoRoot
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true

    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $psi

    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    [void]$process.Start()
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()

    $maxThreads = 0
    $maxWorkingSet = 0
    while (-not $process.HasExited) {
        try {
            $process.Refresh()
            $count = $process.Threads.Count
            if ($count -gt $maxThreads) {
                $maxThreads = $count
            }
            $workingSet = $process.WorkingSet64
            if ($workingSet -gt $maxWorkingSet) {
                $maxWorkingSet = $workingSet
            }
        } catch {
        }

        if ($watch.Elapsed.TotalSeconds -gt $TimeoutSeconds) {
            Stop-BtProcess -Process $process
            throw "command timeout: $FilePath $($psi.Arguments)"
        }

        Start-Sleep -Milliseconds 20
    }

    $watch.Stop()
    $stdout = $stdoutTask.Result
    $stderr = $stderrTask.Result
    $exitCode = $process.ExitCode
    $peakBytes = $process.PeakWorkingSet64
    if ($maxWorkingSet -gt $peakBytes) {
        $peakBytes = $maxWorkingSet
    }
    $peakWorkingSetMb = [Math]::Round($peakBytes / 1MB, 2)

    if ($exitCode -ne 0) {
        throw "command failed: $FilePath $($psi.Arguments)`n$stdout`n$stderr"
    }

    [ordered]@{
        elapsed_ms = [Math]::Round($watch.Elapsed.TotalMilliseconds, 3)
        peak_working_set_mb = $peakWorkingSetMb
        max_threads = $maxThreads
        stdout = $stdout.Trim()
        stderr = $stderr.Trim()
    }
}

function Get-Percentile {
    param(
        [double[]]$Values,
        [double]$Percentile
    )

    if ($Values.Count -eq 0) {
        return 0
    }

    $sorted = @($Values | Sort-Object)
    $index = [Math]::Ceiling(($Percentile / 100.0) * $sorted.Count) - 1
    if ($index -lt 0) {
        $index = 0
    }
    if ($index -ge $sorted.Count) {
        $index = $sorted.Count - 1
    }

    return [Math]::Round([double]$sorted[$index], 3)
}

function Get-PackageVersion {
    $cargoToml = Join-Path $RepoRoot "Cargo.toml"
    $line = Get-Content -Encoding UTF8 $cargoToml | Where-Object { $_ -match '^version\s*=' } | Select-Object -First 1
    if ($line -match '"([^"]+)"') {
        return $Matches[1]
    }
    return "unknown"
}

function Get-CargoCliVersion {
    try {
        return (cargo --version 2>$null).Trim()
    } catch {
        return "unknown"
    }
}

function Get-CargoProfile {
    param([string]$Path)

    $normalized = $Path -replace '/', '\'
    if ($normalized -match '\\release\\') {
        return "release"
    }
    if ($normalized -match '\\debug\\') {
        return "debug"
    }
    return "custom"
}

function Get-PlatformInfo {
    [ordered]@{
        os = [System.Environment]::OSVersion.VersionString
        machine = [System.Environment]::MachineName
        is_64bit_os = [System.Environment]::Is64BitOperatingSystem
        is_64bit_process = [System.Environment]::Is64BitProcess
        processor_count = [System.Environment]::ProcessorCount
    }
}

function Invoke-HttpGet {
    param(
        [System.Net.Http.HttpClient]$Client,
        [string]$Url
    )

    $response = $Client.GetAsync($Url).GetAwaiter().GetResult()
    $body = $response.Content.ReadAsStringAsync().GetAwaiter().GetResult()
    if (-not $response.IsSuccessStatusCode) {
        throw "http request failed: $Url, status $([int]$response.StatusCode), body $body"
    }
    return $body
}

function Invoke-MeasuredHttpGet {
    param(
        [System.Net.Http.HttpClient]$Client,
        [string]$Url,
        [string]$Expected
    )

    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    $body = Invoke-HttpGet -Client $Client -Url $Url
    $watch.Stop()

    if ($body -notlike "*$Expected*") {
        throw "unexpected web benchmark response: $body"
    }

    [ordered]@{
        elapsed_ms = [Math]::Round($watch.Elapsed.TotalMilliseconds, 3)
    }
}

function Wait-WebReady {
    param(
        [System.Net.Http.HttpClient]$Client,
        [string]$Url
    )

    for ($i = 0; $i -lt 80; $i++) {
        try {
            [void](Invoke-HttpGet -Client $Client -Url $Url)
            return $true
        } catch {
            Start-Sleep -Milliseconds 250
        }
    }
    return $false
}

function New-ScenarioResult {
    param(
        [System.Collections.Specialized.OrderedDictionary]$Scenario,
        [object[]]$Runs,
        [int]$Iterations,
        [int]$Warmup
    )

    $times = @($Runs | ForEach-Object { [double]$_.elapsed_ms })
    $avgMs = [Math]::Round(($times | Measure-Object -Average).Average, 3)
    $minMs = [Math]::Round(($times | Measure-Object -Minimum).Minimum, 3)
    $maxMs = [Math]::Round(($times | Measure-Object -Maximum).Maximum, 3)
    $peakWorkingSetMb = [Math]::Round((@($Runs | ForEach-Object { [double]$_.peak_working_set_mb }) | Measure-Object -Maximum).Maximum, 2)
    $maxThreads = (@($Runs | ForEach-Object { [int]$_.max_threads }) | Measure-Object -Maximum).Maximum
    $throughput = 0
    if ($avgMs -gt 0) {
        $throughput = [Math]::Round(([double]$Scenario.operations) / ($avgMs / 1000.0), 2)
    }

    [ordered]@{
        name = $Scenario.name
        category = $Scenario.category
        script = $Scenario.script
        input_scale = [ordered]@{
            operations = $Scenario.operations
            unit = $Scenario.unit
        }
        operations = $Scenario.operations
        unit = $Scenario.unit
        runs = $Iterations
        iterations = $Iterations
        warmup = $Warmup
        avg_ms = $avgMs
        min_ms = $minMs
        max_ms = $maxMs
        p50_ms = Get-Percentile -Values $times -Percentile 50
        p95_ms = Get-Percentile -Values $times -Percentile 95
        p99_ms = Get-Percentile -Values $times -Percentile 99
        throughput_per_sec = $throughput
        peak_working_set_mb = $peakWorkingSetMb
        max_threads = $maxThreads
    }
}

if ($Build) {
    Push-Location $RepoRoot
    try {
        cargo build --release --bin bt
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build --release --bin bt failed"
        }
    } finally {
        Pop-Location
    }
}

if (-not (Test-Path $BtPath)) {
    throw "bt executable not found: $BtPath; run cargo build --release --bin bt or pass -Build."
}

if ($Iterations -lt 1) {
    throw "Iterations must be greater than 0."
}

if ($WebRequests -lt 1) {
    throw "WebRequests must be greater than 0."
}

$Scenarios = @(
    [ordered]@{ name = "vm_arithmetic"; category = "vm"; script = "benches/vm-arithmetic.bt"; operations = 200000; unit = "loop" },
    [ordered]@{ name = "function_closure"; category = "vm"; script = "benches/function-closure.bt"; operations = 50000; unit = "call" },
    [ordered]@{ name = "object_array"; category = "runtime"; script = "benches/object-array.bt"; operations = 20000; unit = "item" },
    [ordered]@{ name = "stdlib_dispatch"; category = "stdlib"; script = "benches/stdlib-dispatch.bt"; operations = 30000; unit = "dispatch" },
    [ordered]@{ name = "include_cache"; category = "compiler"; script = "benches/include-cache.bt"; operations = 3000; unit = "include" },
    [ordered]@{ name = "bytes_modbus"; category = "bytes"; script = "benches/bytes-modbus.bt"; operations = 20000; unit = "frame" },
    [ordered]@{ name = "process_pipe"; category = "process"; script = "benches/process-pipe.bt"; operations = 200; unit = "line" }
)

$results = @()

foreach ($scenario in $Scenarios) {
    $scriptPath = Join-Path $RepoRoot $scenario.script
    if (-not (Test-Path $scriptPath)) {
        throw "benchmark script missing: $scriptPath"
    }

    Write-Host "benchmark $($scenario.name)"

    for ($i = 0; $i -lt $Warmup; $i++) {
        [void](Invoke-MeasuredProcess -FilePath $BtPath -Arguments @($scriptPath) -TimeoutSeconds $TimeoutSeconds)
    }

    $runs = @()
    for ($i = 0; $i -lt $Iterations; $i++) {
        $runs += Invoke-MeasuredProcess -FilePath $BtPath -Arguments @($scriptPath) -TimeoutSeconds $TimeoutSeconds
    }

    $results += New-ScenarioResult -Scenario $scenario -Runs $runs -Iterations $Iterations -Warmup $Warmup
}

$webScript = Join-Path $RepoRoot "benches/web-route/main.bt"
if (-not (Test-Path $webScript)) {
    throw "benchmark script missing: $webScript"
}

Write-Host "benchmark web_route"
$webProcess = $null
$client = New-Object System.Net.Http.HttpClient
$client.Timeout = [TimeSpan]::FromSeconds($TimeoutSeconds)

try {
    $webProcess = Start-BtResident -ScriptPath $webScript
    $webUrl = "http://127.0.0.1:18291/?n=20"

    if (-not (Wait-WebReady -Client $client -Url $webUrl)) {
        throw "web benchmark example did not start within 20 seconds."
    }

    for ($i = 0; $i -lt $WebWarmupRequests; $i++) {
        [void](Invoke-MeasuredHttpGet -Client $client -Url $webUrl -Expected '"ok":true')
    }

    $webRuns = @()
    $peakWorkingSetMb = 0.0
    $maxThreads = 0
    for ($i = 0; $i -lt $WebRequests; $i++) {
        if ($webProcess.HasExited) {
            throw "web benchmark process exited early, PID=$($webProcess.Id)"
        }
        $run = Invoke-MeasuredHttpGet -Client $client -Url $webUrl -Expected '"ok":true'
        $probe = Get-ProcessProbe -Process $webProcess
        if ($probe.working_set_mb -gt $peakWorkingSetMb) {
            $peakWorkingSetMb = $probe.working_set_mb
        }
        if ($probe.threads -gt $maxThreads) {
            $maxThreads = $probe.threads
        }
        $webRuns += [ordered]@{
            elapsed_ms = $run.elapsed_ms
            peak_working_set_mb = $peakWorkingSetMb
            max_threads = $maxThreads
        }
    }

    $webScenario = [ordered]@{
        name = "web_route"
        category = "web"
        script = "benches/web-route/main.bt"
        operations = $WebRequests
        unit = "request"
    }
    $results += New-ScenarioResult -Scenario $webScenario -Runs $webRuns -Iterations $WebRequests -Warmup $WebWarmupRequests
} finally {
    Stop-BtProcess -Process $webProcess
    $client.Dispose()
}

$gitRevision = ""
try {
    $gitRevision = (git -C $RepoRoot rev-parse --short HEAD 2>$null).Trim()
} catch {
}

$gitDirty = $false
try {
    $dirty = (git -C $RepoRoot status --short 2>$null)
    $gitDirty = -not [string]::IsNullOrWhiteSpace($dirty)
} catch {
}

$payload = [ordered]@{
    generated_at = (Get-Date).ToUniversalTime().ToString("o")
    package_version = Get-PackageVersion
    version = Get-PackageVersion
    cargo_version = Get-CargoCliVersion
    cargo_profile = Get-CargoProfile -Path $BtPath
    git_revision = $gitRevision
    git_dirty = $gitDirty
    platform = Get-PlatformInfo
    bt_path = $BtPath
    iterations = $Iterations
    warmup = $Warmup
    web_requests = $WebRequests
    web_warmup_requests = $WebWarmupRequests
    timeout_seconds = $TimeoutSeconds
    scenarios = $results
}

$outputDir = Split-Path -Parent $Output
New-Item -ItemType Directory -Force $outputDir | Out-Null
$payload | ConvertTo-Json -Depth 10 | Set-Content -Encoding UTF8 $Output

$results | ForEach-Object { [pscustomobject]$_ } | Format-Table name, category, p50_ms, p95_ms, p99_ms, throughput_per_sec, peak_working_set_mb, max_threads -AutoSize
Write-Host "benchmark result: $Output"
