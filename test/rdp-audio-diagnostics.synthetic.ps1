#requires -Version 5.1
# Synthetic-only tests: mocks prevent any Windows log/counter access.
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$fixtures = Join-Path $PSScriptRoot 'fixtures/rdp-audio-diagnostics'
$temp = Join-Path ([IO.Path]::GetTempPath()) ('rdp-diagnostic-test-' + [Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($temp)
$id = '0123456789abcdef0123456789abcdef'
function Assert($Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
try {
    $inputs = @((Join-Path $fixtures 'host.jsonl.fixture'), (Join-Path $fixtures 'guest.jsonl.fixture'), (Join-Path $fixtures 'damaged.jsonl.fixture'))
    $original = @($inputs | ForEach-Object { [Convert]::ToBase64String([IO.File]::ReadAllBytes($_)) })
    $reportPath = Join-Path $temp 'report.json'
    & (Join-Path $root 'scripts/analyze-rdp-audio-diagnostics.ps1') -InputPath $inputs -CaptureId $id -OutputPath $reportPath
    $report = Get-Content -Raw $reportPath | ConvertFrom-Json
    Assert ($report.stop_observations.Count -eq 1) 'Expected one stop observation'
    Assert ($report.stop_observations[0].delta_ms -eq 150) 'Threshold equality must not count as below threshold'
    Assert ($report.timeline.Count -eq 10) 'Only matching valid rows belong in timeline'
    Assert ($report.raw_lines.Count -eq 13) 'All raw lines must remain'
    Assert (@($report.raw_lines | Where-Object { $_.status -eq 'invalid' }).Count -eq 2) 'Malformed lines must be reported'
    Assert (@($report.raw_lines | Where-Object { $_.status -eq 'unmatched' }).Count -eq 1) 'Capture mismatch must remain visible'
    Assert ($report.warnings -contains 'unterminated_final_line:3:3') 'Truncation warning missing'
    for ($i = 0; $i -lt $inputs.Count; $i++) {
        Assert ([Convert]::ToBase64String([IO.File]::ReadAllBytes($inputs[$i])) -eq $original[$i]) 'Input changed'
        Assert ($report.raw_inputs[$i].base64 -eq $original[$i]) 'Exact input bytes were not preserved'
    }
    $broken = Join-Path $temp 'broken.jsonl'
    [IO.File]::WriteAllBytes($broken, [byte[]]@(123, 34, 195))
    & (Join-Path $root 'scripts/analyze-rdp-audio-diagnostics.ps1') -InputPath $broken -CaptureId $id -OutputPath (Join-Path $temp 'broken-report.json')
    $brokenReport = Get-Content -Raw (Join-Path $temp 'broken-report.json') | ConvertFrom-Json
    Assert ($brokenReport.warnings -contains 'invalid_utf8:1') 'Broken UTF-8 must remain detectable'
    Assert ($brokenReport.raw_inputs[0].base64 -eq 'eyLD') 'Broken UTF-8 bytes must survive'

    # Delayed marker serialization must not hide a sample after handler arrival.
    $interleaved = @(
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 0; utc_unix_ms = 946684800000; elapsed_ms = 0; event = 'capture_started' },
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 1; utc_unix_ms = 946684800200; elapsed_ms = 200; acquired_elapsed_ms = 200; acquired_utc_unix_ms = 946684800200; event = 'meter_sample'; peak = 0.001 },
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 2; utc_unix_ms = 946684800400; elapsed_ms = 400; event = 'marker'; marker = 'stop_pressed'; received_elapsed_ms = 150; received_utc_unix_ms = 946684800150 }
    )
    $interleavedPath = Join-Path $temp 'interleaved.jsonl'
    [IO.File]::WriteAllText($interleavedPath, (($interleaved | ForEach-Object { $_ | ConvertTo-Json -Compress }) -join "`n") + "`n")
    & (Join-Path $root 'scripts/analyze-rdp-audio-diagnostics.ps1') -InputPath $interleavedPath -CaptureId $id -OutputPath (Join-Path $temp 'interleaved-report.json')
    $interleavedReport = Get-Content -Raw (Join-Path $temp 'interleaved-report.json') | ConvertFrom-Json
    Assert ($interleavedReport.stop_observations[0].delta_ms -eq 50) 'Handler-arrival marker interleaving must be respected'
    Assert ($interleavedReport.warnings -contains 'guest_missing') 'Missing guest must be explicit'
    Assert ($interleavedReport.warnings -contains 'capture_interrupted_or_incomplete:host') 'Interrupted capture must be explicit'
    $interleaved += @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 3; utc_unix_ms = 946684800500; elapsed_ms = 500; event = 'marker'; marker = 'playback_started'; received_elapsed_ms = 175 }
    [IO.File]::WriteAllText($interleavedPath, (($interleaved | ForEach-Object { $_ | ConvertTo-Json -Compress }) -join "`n") + "`n")
    & (Join-Path $root 'scripts/analyze-rdp-audio-diagnostics.ps1') -InputPath $interleavedPath -CaptureId $id -OutputPath (Join-Path $temp 'restart-report.json')
    $restartReport = Get-Content -Raw (Join-Path $temp 'restart-report.json') | ConvertFrom-Json
    Assert ($restartReport.stop_observations[0].status -eq 'playback_restarted_before_threshold') 'Playback restart arrival must end observation'
    Assert ($null -eq $restartReport.stop_observations[0].delta_ms) 'Unobserved difference must not be fabricated'

    function Analyze-Synthetic($Records, [string]$Name) {
        $inputFile = Join-Path $temp ($Name + '.jsonl')
        $outputFile = Join-Path $temp ($Name + '-report.json')
        [IO.File]::WriteAllText($inputFile, (($Records | ForEach-Object { $_ | ConvertTo-Json -Compress }) -join "`n") + "`n")
        & (Join-Path $root 'scripts/analyze-rdp-audio-diagnostics.ps1') -InputPath $inputFile -CaptureId $id -OutputPath $outputFile
        return (Get-Content -Raw $outputFile | ConvertFrom-Json)
    }
    $stale = @(
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 0; utc_unix_ms = 946684800000; elapsed_ms = 0; event = 'capture_started' },
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 1; utc_unix_ms = 946684800400; elapsed_ms = 400; received_elapsed_ms = 200; event = 'marker'; marker = 'stop_pressed' },
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 2; utc_unix_ms = 946684800500; elapsed_ms = 500; acquired_elapsed_ms = 100; event = 'meter_sample'; peak = 0.001 }
    )
    $staleReport = Analyze-Synthetic $stale 'stale-acquisition'
    Assert ($null -eq $staleReport.stop_observations[0].delta_ms) 'Pre-stop acquisition must not count even when appended later'
    $stale[2].acquired_elapsed_ms = 600
    $invalidTimeReport = Analyze-Synthetic $stale 'invalid-acquisition'
    Assert ($invalidTimeReport.warnings -contains 'invalid_acquired_time:1:3') 'Acquisition after append must be rejected'
    Assert ($null -eq $invalidTimeReport.stop_observations[0].delta_ms) 'Invalid acquisition must not count'
    $stale[2].Remove('acquired_elapsed_ms')
    $missingTimeReport = Analyze-Synthetic $stale 'missing-acquisition'
    Assert ($missingTimeReport.warnings -contains 'missing_acquired_time:1:3') 'Missing acquisition must not use append time'
    Assert ($null -eq $missingTimeReport.stop_observations[0].delta_ms) 'Missing acquisition must not fabricate latency'
    $arrivalOrder = @(
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 0; utc_unix_ms = 946684800000; elapsed_ms = 0; event = 'capture_started' },
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 2; utc_unix_ms = 946684800300; elapsed_ms = 300; received_elapsed_ms = 250; event = 'marker'; marker = 'playback_started' },
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 3; utc_unix_ms = 946684800400; elapsed_ms = 400; received_elapsed_ms = 150; event = 'marker'; marker = 'stop_pressed' },
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 4; utc_unix_ms = 946684800450; elapsed_ms = 450; acquired_elapsed_ms = 450; event = 'meter_sample'; peak = 0.001 }
    )
    $arrivalReport = Analyze-Synthetic $arrivalOrder 'arrival-order'
    Assert ($arrivalReport.stop_observations[0].status -eq 'playback_restarted_before_threshold') 'Restart arrival must override earlier append sequence'
    Assert ($null -eq $arrivalReport.stop_observations[0].delta_ms) 'Post-restart acquisition must not count'
    $arrivalOrder[2].received_elapsed_ms = 500
    $invalidMarkerReport = Analyze-Synthetic $arrivalOrder 'invalid-marker'
    Assert ($invalidMarkerReport.stop_observations[0].status -eq 'invalid_stop_time') 'Invalid present marker time must not silently fall back'
    Assert ($invalidMarkerReport.warnings -contains 'invalid_received_time:1:3') 'Invalid marker time warning missing'

    # No platform event log or counter cmdlets are invoked by these mocks.
    function Get-WinEvent {
        [CmdletBinding()]param($ListLog, $LogName, $MaxEvents, $FilterXPath, [switch]$Oldest)
        if ($ListLog) { return [pscustomobject]@{ IsEnabled = $true } }
        if (-not $FilterXPath) { return [pscustomobject]@{ RecordId = 1 } }
        $entry = [pscustomobject]@{ RecordId = 2; Id = 132; TimeCreated = [DateTime]::UtcNow }
        $entry | Add-Member ScriptMethod ToXml { '<Event><EventData><Data Name="ChannelName">rdpsnd</Data><Data Name="ClientAddress">synthetic-do-not-copy</Data><Data Name="UserName">synthetic-do-not-copy</Data></EventData></Event>' }
        $generic = [pscustomobject]@{ RecordId = 3; Id = 999; TimeCreated = [DateTime]::UtcNow }
        $generic | Add-Member ScriptMethod ToXml { '<Event><EventData><Data Name="UserName">rdpsnd</Data><Data Name="Other">AUDIO_PLAYBACK_DVC</Data></EventData></Event>' }
        return @($entry, $generic)
    }
    function Get-Counter { [CmdletBinding()]param($Counter, $MaxSamples) throw 'synthetic missing counter' }
    $capture = Join-Path $temp 'guest.jsonl.fixture'
    & (Join-Path $root 'scripts/collect-rdp-audio-diagnostics.ps1') -CaptureId $id -OutputPath $capture -DurationSeconds 1
    $raw = [IO.File]::ReadAllText($capture)
    $records = @($raw.TrimEnd().Split("`n") | ForEach-Object { $_ | ConvertFrom-Json })
    Assert ($records[0].event -eq 'capture_started') 'Start event missing'
    Assert ($records[-1].event -eq 'capture_stopped') 'Clean terminal event missing'
    Assert ($records[-1].reason -eq 'duration_limit') 'Duration stop reason missing'
    Assert (@($records | Where-Object { $_.event -eq 'rdp_audio_channel' }).Count -ge 1) 'Audio channel not captured'
    Assert (@($records | Where-Object { $_.state -eq 'unavailable' }).Count -ge 1) 'Missing counters must be tolerated'
    Assert (-not $raw.Contains('synthetic-do-not-copy')) 'Raw event data leaked'
    Assert (-not $raw.Contains('ChannelName')) 'Arbitrary event field leaked'
    Assert (@($records | Where-Object { $_.event -eq 'rdp_audio_channel' -and $_.event_id -eq 999 }).Count -eq 0) 'A generic field matching an audio name must not imply an audio channel'
    for ($i = 0; $i -lt $records.Count; $i++) { Assert ($records[$i].sequence -eq $i) 'Sequence gap' }
    $small = Join-Path $temp 'small.jsonl'
    & (Join-Path $root 'scripts/collect-rdp-audio-diagnostics.ps1') -CaptureId $id -OutputPath $small -DurationSeconds 2 -MaxBytes 4096
    $smallRecords = @(Get-Content $small | ForEach-Object { $_ | ConvertFrom-Json })
    Assert ((Get-Item $small).Length -le 4096) 'Size bound exceeded'
    Assert ($smallRecords[-1].event -eq 'capture_stopped') 'Size-limited capture must terminate cleanly'
    Assert ($smallRecords[-1].reason -eq 'size_limit') 'Size stop reason missing'
    Write-Output 'Synthetic diagnostics checks passed.'
} finally { [IO.Directory]::Delete($temp, $true) }
