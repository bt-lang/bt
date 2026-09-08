<#
.SYNOPSIS
Installs the built image package into an isolated project and verifies its actual BT API.
#>
[CmdletBinding()]
param([string]$BtPath = "")

$ErrorActionPreference = "Stop"
$extensionRoot = $PSScriptRoot
$repoRoot = [IO.Path]::GetFullPath((Join-Path $extensionRoot "../.."))
if (!$BtPath) { $BtPath = Join-Path $repoRoot "target/debug/bt.exe" }
$BtPath = (Resolve-Path -LiteralPath $BtPath).Path
$package = Join-Path $extensionRoot "target/image-1.0.0.bts"
$project = Join-Path $extensionRoot "target/acceptance-project"
New-Item -ItemType Directory -Force -Path $project | Out-Null
Copy-Item -LiteralPath (Join-Path $extensionRoot "smoke.bt") -Destination (Join-Path $project "main.bt") -Force
[IO.File]::WriteAllBytes((Join-Path $extensionRoot "target/outside-image.png"), [byte[]]@(0, 1, 2))
$installOutput = & $BtPath ext install $package $project 2>&1
$installOutput | Write-Output
if ($LASTEXITCODE -ne 0 -or !($installOutput -match "Installed extension package:")) { throw "Image installation failed." }

$cases = @(
    @{name="format"; code="image('canvas.png').create(2, 2, [0, 0, 0, 0]).encode('tiff', {})"; pattern="unsupported image format"},
    @{name="gamma"; code="image('canvas.png').create(2, 2, [0, 0, 0, 0]).adjust({gamma: 0})"; pattern="gamma must be in"},
    @{name="rotation"; code="image('canvas.png').create(2, 2, [0, 0, 0, 0]).rotate(45)"; pattern="degrees must be"},
    @{name="crop"; code="image('canvas.png').create(2, 2, [0, 0, 0, 0]).crop(1, 1, 2, 2)"; pattern="crop rectangle is outside"},
    @{name="text"; code="image('canvas.png').create(2, 2, [0, 0, 0, 0]).text('中文', 0, 0, 1, [255, 255, 255, 255])"; pattern="text supports at most"},
    @{name="quality"; code="image('canvas.png').create(2, 2, [0, 0, 0, 0]).encode('jpeg', {quality: 101})"; pattern="quality must be an integer"},
    @{name="matte"; code="image('canvas.png').create(2, 2, [0, 0, 0, 0]).encode('jpeg', {background: [0, 0, 0, 0]})"; pattern="background alpha must be"},
    @{name="unknown_option"; code="image('canvas.png').create(2, 2, [0, 0, 0, 0]).adjust({gamam: 1})"; pattern="unknown image option"},
    @{name="malformed"; code="image('decoded.png').decode(bytes('000102', 'hex'))"; pattern="unsupported image input"},
    @{name="stale_handle"; code="img = image('canvas.png').create(2, 2, [0, 0, 0, 0])`nimg.close()`nimg.info()"; pattern="has expired|no longer valid|disposed|closed"},
    @{name="lazy_missing"; code="img = image('never-written.png')`nprint 'BOUND_WITHOUT_READ'`nimg.info()"; pattern="image open failed"},
    @{name="lazy_malformed"; code="img = image('malformed-source.png')`nprint 'BOUND_WITHOUT_READ'`nimg.info()"; pattern="unsupported image input"},
    @{name="closed_unloaded"; code="img = image('never-written.png')`nimg.close()`nimg.create(1, 1, [0, 0, 0, 0])"; pattern="has expired|no longer valid|disposed|closed"},
    @{name="path_escape"; code="image('../outside-image.png')"; pattern="escapes project root"}
)
$results = @()
Push-Location $project
try {
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $smokeOutput = & $BtPath main.bt 2>&1
    $watch.Stop()
    $smokeOutput | Write-Output
    if ($LASTEXITCODE -ne 0 -or !($smokeOutput -match "^image-smoke:ok;")) { throw "Image BT smoke failed." }
    foreach ($name in @("canvas.png", "decoded.png", "never-written.png")) {
        if (Test-Path -LiteralPath (Join-Path $project $name)) { throw "Unexpected implicit image output: $name" }
    }
    if ([IO.File]::ReadAllText((Join-Path $project "malformed-source.png")) -ne "not an image") { throw "Create changed the bound source file." }
    $results += [ordered]@{case="complete_api"; status="passed"; elapsed_ms=$watch.ElapsedMilliseconds; output=($smokeOutput -join "`n")}
    foreach ($case in $cases) {
        $scriptPath = Join-Path $project ($case.name + ".bt")
        [IO.File]::WriteAllText($scriptPath, $case.code + "`nprint 'UNEXPECTED_SUCCESS'`n", (New-Object Text.UTF8Encoding($false)))
        $output = & $BtPath $scriptPath 2>&1
        $joined = $output -join "`n"
        # Some BT CLI business errors currently return exit code zero; inspect the actual error.
        if ($joined -notmatch $case.pattern -or $joined -match "(?m)^UNEXPECTED_SUCCESS") { throw "$($case.name): $joined" }
        if ($case.name -like "lazy_*" -and $joined -notmatch "(?m)^BOUND_WITHOUT_READ") { throw "Lazy binding failed before the pixel operation: $joined" }
        $results += [ordered]@{case=$case.name; status="passed"; output=$joined}
        Write-Output "$($case.name): passed"
    }
} finally {
    Pop-Location
}
[IO.File]::WriteAllText((Join-Path $project "results.json"), ($results | ConvertTo-Json -Depth 6), (New-Object Text.UTF8Encoding($false)))
Write-Output "Image acceptance evidence: $(Join-Path $project 'results.json')"
