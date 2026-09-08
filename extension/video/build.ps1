param([Parameter(Mandatory = $true)][string]$BtExe)
$ErrorActionPreference = 'Stop'
$videoRoot = $PSScriptRoot
$videoBt = (Resolve-Path -LiteralPath $BtExe).Path
Push-Location $videoRoot
try {
    & cargo build --locked --target wasm32-wasip1 --release
    if ($LASTEXITCODE -ne 0) { throw 'Video WASI build failed.' }
    $videoStage = Join-Path $videoRoot ('target/package-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $videoStage | Out-Null
    foreach ($videoFile in @('manifest.json', 'bindings.json', 'LICENSE-MIT', 'LICENSE-APACHE', 'COPYRIGHT', 'THIRD_PARTY_LICENSES.txt', 'README.md')) {
        Copy-Item -LiteralPath (Join-Path $videoRoot $videoFile) -Destination $videoStage
    }
    Copy-Item -LiteralPath (Join-Path $videoRoot 'target/wasm32-wasip1/release/video.wasm') -Destination (Join-Path $videoStage 'module.wasm')
    $videoPackage = Join-Path $videoRoot 'target/video-1.0.0.bts'
    $videoBuildOutput = & $videoBt ext build $videoStage -o $videoPackage
    if ($LASTEXITCODE -ne 0 -or ($videoBuildOutput -join "`n") -notmatch 'Built extension package:') { throw "Video package build failed: $videoBuildOutput" }
    Write-Output $videoBuildOutput
    $videoCheckOutput = & $videoBt ext check $videoPackage
    if ($LASTEXITCODE -ne 0 -or ($videoCheckOutput -join "`n") -notmatch 'Extension package check passed:') { throw "Video package check failed: $videoCheckOutput" }
    Write-Output $videoCheckOutput
    Write-Output $videoPackage
} finally { Pop-Location }
