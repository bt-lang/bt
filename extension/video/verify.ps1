param(
    [Parameter(Mandatory = $true)][string]$BtExe,
    [Parameter(Mandatory = $true)][string]$Package
)
$ErrorActionPreference = 'Stop'
$videoBt = (Resolve-Path -LiteralPath $BtExe).Path
$videoPackage = (Resolve-Path -LiteralPath $Package).Path
$videoProject = Join-Path $PSScriptRoot ('target/bt-smoke-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $videoProject | Out-Null
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'smoke.bt') -Destination (Join-Path $videoProject 'main.bt')
Push-Location $videoProject
try {
    & ffmpeg -v error -nostdin -y -f lavfi -i 'testsrc2=size=320x180:rate=25:duration=2' -f lavfi -i 'sine=frequency=440:duration=2' -c:v mpeg4 -q:v 4 -c:a aac -shortest source.mp4
    if ($LASTEXITCODE -ne 0) { throw 'Video fixture generation failed.' }
    & ffmpeg -v error -nostdin -y -f lavfi -i 'sine=frequency=880:duration=0.6' tone.wav
    if ($LASTEXITCODE -ne 0) { throw 'Audio fixture generation failed.' }
    $videoInstallOutput = & $videoBt ext install $videoPackage $videoProject
    if ($LASTEXITCODE -ne 0 -or ($videoInstallOutput -join "`n") -notmatch 'Installed extension package:' -or !(Test-Path -LiteralPath 'extensions/video/video-1.0.0.bts')) { throw "Video package installation failed: $videoInstallOutput" }
    Write-Output $videoInstallOutput
    $videoWatch = [Diagnostics.Stopwatch]::StartNew()
    $videoSmokeOutput = & $videoBt main.bt
    $videoSmokeOutput | Set-Content -Encoding UTF8 -LiteralPath 'bt-smoke.log'
    if ($LASTEXITCODE -ne 0 -or ($videoSmokeOutput -join "`n") -notmatch 'VIDEO_SMOKE_PASS') { throw "Installed BT video smoke failed: $videoSmokeOutput" }
    Write-Output $videoSmokeOutput
    $videoWatch.Stop()
    # The CLI may return exit code zero for script errors: assert the exact error
    # category and ensure execution did not reach the unexpected-success marker.
    $videoNegativeCases = @(
        @{name='different_source'; code="clip = video('@/source.mp4', {}); job = clip.info(); other = video('@/tone.wav', {}); other.job(job.id())"; error='different source path'},
        @{name='missing_job'; code="clip = video('@/source.mp4', {}); clip.job(999999)"; error='no longer valid'},
        @{name='invalid_job_id'; code="clip = video('@/source.mp4', {}); clip.job(0)"; error='positive integer'},
        @{name='closed_job_alias'; code="clip = video('@/source.mp4', {}); job = clip.info(); restored = clip.job(job.id()); restored.close(); job.status()"; error='no longer valid|expired|disposed'},
        @{name='closed_source'; code="clip = video('@/source.mp4', {}); job = clip.info(); id = job.id(); clip.close(); clip.job(id)"; error='no longer valid|expired|disposed'}
    )
    foreach ($videoCase in $videoNegativeCases) {
        $videoCasePath = Join-Path $videoProject ($videoCase.name + '.bt')
        [IO.File]::WriteAllText($videoCasePath, $videoCase.code + "`nprint 'VIDEO_UNEXPECTED_SUCCESS'`n", [Text.UTF8Encoding]::new($false))
        $videoCaseOutput = (& $videoBt $videoCasePath 2>&1 | Out-String)
        $videoCaseOutput | Set-Content -Encoding UTF8 -LiteralPath ($videoCase.name + '.log')
        if ($videoCaseOutput -notmatch $videoCase.error -or $videoCaseOutput -match 'VIDEO_UNEXPECTED_SUCCESS') {
            throw "Video negative case failed: $($videoCase.name): $videoCaseOutput"
        }
    }
    $videoRows = @()
    foreach ($videoName in @('transcoded.mp4','converted.webm','trimmed.mp4','joined.mp4','joined_silent.webm','frame.png','frame.jpg','small.mp4','audio.wav','audio.flac','audio.m4a','replaced.mp4')) {
        & ffmpeg -v error -nostdin -i $videoName -f null -
        if ($LASTEXITCODE -ne 0) { throw "Output decode failed: $videoName" }
        $videoProbe = (& ffprobe -v error -show_entries 'format=duration,size:stream=codec_type,codec_name,width,height,avg_frame_rate,duration' -of json $videoName | ConvertFrom-Json)
        if ($LASTEXITCODE -ne 0) { throw "Output probe failed: $videoName" }
        $videoRows += [pscustomobject]@{file=$videoName; metadata=$videoProbe}
    }
    $videoSmall = ($videoRows | Where-Object file -eq 'small.mp4').metadata.streams | Where-Object codec_type -eq 'video'
    if ($videoSmall.width -ne 160 -or $videoSmall.height -ne 90) { throw 'Resize dimensions are incorrect.' }
    foreach ($videoCheck in @(@('trimmed.mp4',0.8),@('joined.mp4',4.0),@('replaced.mp4',2.0))) {
        $videoDuration = [double]($videoRows | Where-Object file -eq $videoCheck[0]).metadata.format.duration
        if ([Math]::Abs($videoDuration - [double]$videoCheck[1]) -gt 0.12) { throw "Duration outside tolerance: $($videoCheck[0])" }
    }
    $videoSilent = ($videoRows | Where-Object file -eq 'joined_silent.webm').metadata.streams | Where-Object codec_type -eq 'audio'
    if ($null -ne $videoSilent) { throw 'Video-only concat retained audio.' }
    foreach ($videoAbsent in @('cancelled.mp4','bad.mp4','unpolled_cancel.mp4','unpolled_close.mp4')) {
        if (Test-Path -LiteralPath $videoAbsent) { throw "Incomplete output was not cleaned: $videoAbsent" }
    }
    $videoReport = [pscustomobject]@{ok=$true; project=$videoProject; elapsed_ms=$videoWatch.ElapsedMilliseconds; negative_cases=$videoNegativeCases.Count; outputs=$videoRows}
    $videoReport | ConvertTo-Json -Depth 10 | Set-Content -Encoding UTF8 -LiteralPath 'verification.json'
    Write-Output ([pscustomobject]@{ok=$true; project=$videoProject; elapsed_ms=$videoWatch.ElapsedMilliseconds; output_count=$videoRows.Count} | ConvertTo-Json)
} finally { Pop-Location }
