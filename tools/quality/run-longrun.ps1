param(
    [string]$BtPath = "",
    [ValidateSet("custom", "1h", "8h", "24h")]
    [string]$Profile = "custom",
    [int]$DurationMinutes = 10,
    [int]$RequestDelayMs = 25,
    [int]$SnapshotSeconds = 30,
    [switch]$Build,
    [switch]$IncludeNet,
    [switch]$FaultInjection,
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

if ($Profile -ne "custom" -and -not $PSBoundParameters.ContainsKey("DurationMinutes")) {
    switch ($Profile) {
        "1h" { $DurationMinutes = 60 }
        "8h" { $DurationMinutes = 480 }
        "24h" { $DurationMinutes = 1440 }
    }
}

if ([string]::IsNullOrWhiteSpace($BtPath)) {
    $BtPath = Join-Path $RepoRoot ("target/debug/bt" + $ExeSuffix)
}

if ([string]::IsNullOrWhiteSpace($Output)) {
    if ($Profile -eq "custom") {
        $Output = Join-Path $RepoRoot "target/quality/longrun.json"
    } else {
        $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
        $Output = Join-Path $RepoRoot "target/quality/longrun-$Profile-$stamp.json"
    }
}

function ConvertTo-CommandArgument {
    param([string]$Value)

    if ($Value -notmatch '[\s"]') {
        return $Value
    }

    return '"' + ($Value -replace '"', '\"') + '"'
}

function Start-BtResident {
    param([string]$ScriptPath)

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $BtPath
    $psi.Arguments = ConvertTo-CommandArgument $ScriptPath
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

function Invoke-BtOnce {
    param(
        [string]$ScriptPath,
        [int]$TimeoutSeconds
    )

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $BtPath
    $psi.Arguments = ConvertTo-CommandArgument $ScriptPath
    $psi.WorkingDirectory = [string]$RepoRoot
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true

    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $psi
    [void]$process.Start()
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()

    if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
        Stop-BtProcess -Process $process
        throw "script timeout: $ScriptPath"
    }

    $stdout = $stdoutTask.Result
    $stderr = $stderrTask.Result
    if ($process.ExitCode -ne 0) {
        throw "script failed: $ScriptPath`n$stdout`n$stderr"
    }
    return $stdout.Trim()
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

function Invoke-ExpectedHttpFailure {
    param(
        [System.Net.Http.HttpClient]$Client,
        [string]$Url
    )

    $response = $Client.GetAsync($Url).GetAwaiter().GetResult()
    $body = $response.Content.ReadAsStringAsync().GetAwaiter().GetResult()
    if ($response.IsSuccessStatusCode) {
        throw "expected http failure but got success: $Url, body $body"
    }

    [ordered]@{
        status = [int]$response.StatusCode
        body = $body
    }
}

function Get-ProcessSnapshot {
    param(
        [string]$Name,
        [System.Diagnostics.Process]$Process
    )

    if ($null -eq $Process) {
        return [ordered]@{
            name = $Name
            pid = $null
            exited = $true
            working_set_mb = 0.0
            threads = 0
        }
    }

    if ($Process.HasExited) {
        return [ordered]@{
            name = $Name
            pid = $Process.Id
            exited = $true
            working_set_mb = 0.0
            threads = 0
        }
    }

    try {
        $Process.Refresh()
        return [ordered]@{
            name = $Name
            pid = $Process.Id
            exited = $false
            working_set_mb = [Math]::Round($Process.WorkingSet64 / 1MB, 2)
            threads = $Process.Threads.Count
        }
    } catch {
        return [ordered]@{
            name = $Name
            pid = $Process.Id
            exited = $false
            working_set_mb = 0.0
            threads = 0
        }
    }
}

function Read-StatsSnapshot {
    param(
        [System.Net.Http.HttpClient]$Client,
        [string]$Url
    )

    $body = Invoke-HttpGet -Client $Client -Url $Url
    return $body | ConvertFrom-Json
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

if ($DurationMinutes -lt 1) {
    throw "DurationMinutes must be greater than 0."
}

if ($SnapshotSeconds -lt 1) {
    throw "SnapshotSeconds must be greater than 0."
}

$webScript = Join-Path $RepoRoot "examples/longrun-audit/main.bt"
$probeScript = Join-Path $RepoRoot "examples/longrun-audit/probe.bt"
$tcpServerScript = Join-Path $RepoRoot "examples/net-phase2-tcp-server.bt"
$tcpClientScript = Join-Path $RepoRoot "examples/net-phase3-tcp-burst-client.bt"
$webProcess = $null
$tcpProcess = $null
$client = New-Object System.Net.Http.HttpClient
$client.Timeout = [TimeSpan]::FromSeconds(20)
$requests = 0
$statsRequests = 0
$tcpRuns = 0
$probeRuns = 0
$faultRuns = 0
$errors = @()
$snapshots = @()
$peakWorkingSetMb = 0.0
$maxThreads = 0

try {
    $webProcess = Start-BtResident -ScriptPath $webScript

    if ($IncludeNet) {
        $tcpProcess = Start-BtResident -ScriptPath $tcpServerScript
    }

    $started = Get-Date
    $deadline = $started.AddMinutes($DurationMinutes)
    $nextSnapshot = $started
    $webUrl = "http://127.0.0.1:18282/"
    $workUrl = "http://127.0.0.1:18282/?mode=work"
    $statsUrl = "http://127.0.0.1:18282/?mode=stats"
    $faultUrl = "http://127.0.0.1:18282/?mode=reject"

    $ready = $false
    for ($i = 0; $i -lt 80; $i++) {
        try {
            [void](Invoke-HttpGet -Client $client -Url $webUrl)
            $ready = $true
            break
        } catch {
            Start-Sleep -Milliseconds 250
        }
    }

    if (-not $ready) {
        throw "web longrun audit example did not start within 20 seconds."
    }

    while ((Get-Date) -lt $deadline) {
        try {
            $body = Invoke-HttpGet -Client $client -Url $workUrl
            if ($body -notlike '*"ok":true*') {
                throw "unexpected web response: $body"
            }
            $requests += 1

            if ($IncludeNet -and (($requests % 25) -eq 0)) {
                $tcpOut = Invoke-BtOnce -ScriptPath $tcpClientScript -TimeoutSeconds 15
                if ($tcpOut -notlike "*100*") {
                    throw "unexpected TCP burst output: $tcpOut"
                }
                $tcpRuns += 1
            }

            if (($requests % 30) -eq 0) {
                $probeOut = Invoke-BtOnce -ScriptPath $probeScript -TimeoutSeconds 15
                if ($probeOut -notlike '*"ok":true*') {
                    throw "unexpected probe output: $probeOut"
                }
                $probeRuns += 1
            }

            if ($FaultInjection -and (($requests % 40) -eq 0)) {
                [void](Invoke-ExpectedHttpFailure -Client $client -Url $faultUrl)
                $faultRuns += 1
            }

            foreach ($process in @($webProcess, $tcpProcess)) {
                if ($null -eq $process) {
                    continue
                }
                if ($process.HasExited) {
                    throw "resident process exited early, PID=$($process.Id)"
                }
                $sample = Get-ProcessSnapshot -Name "resident" -Process $process
                if ($sample.working_set_mb -gt $peakWorkingSetMb) {
                    $peakWorkingSetMb = $sample.working_set_mb
                }
                if ($sample.threads -gt $maxThreads) {
                    $maxThreads = $sample.threads
                }
            }

            $now = Get-Date
            if ($now -ge $nextSnapshot) {
                $stats = Read-StatsSnapshot -Client $client -Url $statsUrl
                $statsRequests += 1
                $snapshots += [ordered]@{
                    timestamp = $now.ToUniversalTime().ToString("o")
                    elapsed_ms = [Math]::Round(($now - $started).TotalMilliseconds, 3)
                    web_requests = $requests
                    stats_requests = $statsRequests
                    tcp_burst_runs = $tcpRuns
                    probe_runs = $probeRuns
                    fault_injections = $faultRuns
                    error_count = $errors.Count
                    processes = @(
                        Get-ProcessSnapshot -Name "web" -Process $webProcess
                        Get-ProcessSnapshot -Name "tcp" -Process $tcpProcess
                    )
                    stats = $stats.stats
                }
                $nextSnapshot = $now.AddSeconds($SnapshotSeconds)
            }
        } catch {
            $errors += [string]$_
        }

        Start-Sleep -Milliseconds $RequestDelayMs
    }

    try {
        $now = Get-Date
        $stats = Read-StatsSnapshot -Client $client -Url $statsUrl
        $statsRequests += 1
        $snapshots += [ordered]@{
            timestamp = $now.ToUniversalTime().ToString("o")
            elapsed_ms = [Math]::Round(($now - $started).TotalMilliseconds, 3)
            web_requests = $requests
            stats_requests = $statsRequests
            tcp_burst_runs = $tcpRuns
            probe_runs = $probeRuns
            fault_injections = $faultRuns
            error_count = $errors.Count
            processes = @(
                Get-ProcessSnapshot -Name "web" -Process $webProcess
                Get-ProcessSnapshot -Name "tcp" -Process $tcpProcess
            )
            stats = $stats.stats
        }
    } catch {
        $errors += [string]$_
    }
} finally {
    foreach ($process in @($webProcess, $tcpProcess)) {
        Stop-BtProcess -Process $process
    }
    $client.Dispose()
}

if ($errors.Count -gt 0) {
    Write-Host "longrun errors:"
    $errors | ForEach-Object { Write-Host $_ }
    throw "longrun found errors: $($errors.Count)"
}

$payload = [ordered]@{
    generated_at = (Get-Date).ToUniversalTime().ToString("o")
    bt_path = $BtPath
    profile = $Profile
    duration_minutes = $DurationMinutes
    request_delay_ms = $RequestDelayMs
    snapshot_seconds = $SnapshotSeconds
    include_net = [bool]$IncludeNet
    fault_injection = [bool]$FaultInjection
    web_requests = $requests
    stats_requests = $statsRequests
    tcp_burst_runs = $tcpRuns
    probe_runs = $probeRuns
    fault_injections = $faultRuns
    error_count = $errors.Count
    peak_working_set_mb = $peakWorkingSetMb
    max_threads = $maxThreads
    snapshots = $snapshots
}

$outputDir = Split-Path -Parent $Output
New-Item -ItemType Directory -Force $outputDir | Out-Null
$payload | ConvertTo-Json -Depth 16 | Set-Content -Encoding UTF8 $Output

[pscustomobject]@{
    generated_at = $payload.generated_at
    profile = $payload.profile
    duration_minutes = $payload.duration_minutes
    web_requests = $payload.web_requests
    stats_requests = $payload.stats_requests
    tcp_burst_runs = $payload.tcp_burst_runs
    probe_runs = $payload.probe_runs
    fault_injections = $payload.fault_injections
    snapshots = $payload.snapshots.Count
    peak_working_set_mb = $payload.peak_working_set_mb
    max_threads = $payload.max_threads
} | Format-List

Write-Host "longrun result: $Output"
