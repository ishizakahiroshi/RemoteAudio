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
    $combinedGuest = Join-Path $temp 'guest-with-damage.jsonl'
    [IO.File]::WriteAllBytes($combinedGuest, [byte[]]([IO.File]::ReadAllBytes((Join-Path $fixtures 'guest.jsonl.fixture')) + [IO.File]::ReadAllBytes((Join-Path $fixtures 'damaged.jsonl.fixture'))))
    $inputs = @((Join-Path $fixtures 'host.jsonl.fixture'), $combinedGuest)
    $original = @($inputs | ForEach-Object { [Convert]::ToBase64String([IO.File]::ReadAllBytes($_)) })
    $reportPath = Join-Path $temp 'report.json'
    & (Join-Path $root 'scripts/analyze-rdp-audio-diagnostics.ps1') -InputPath $inputs -CaptureId $id -OutputPath $reportPath
    $report = Get-Content -Raw $reportPath | ConvertFrom-Json
    Assert ($report.stop_observations.Count -eq 1) 'Expected one stop observation'
    Assert ($report.stop_observations[0].delta_ms -eq 150) 'Threshold equality must not count as below threshold'
    $timelineDetails = @($report.timeline | ForEach-Object { "$($_.input_index):$($_.line_number):$($_.record.source):$($_.record.sequence)" }) -join ','
    $rowDetails = @($report.raw_lines | ForEach-Object { "$($_.input_index):$($_.line_number):$($_.status):$($_.issue)" }) -join ','
    Assert ($report.timeline.Count -eq 10) "Only matching valid rows belong in timeline: expected=10 actual=$($report.timeline.Count); timeline=[$timelineDetails]; raw=[$rowDetails]; warnings=[$($report.warnings -join ',')]"
    Assert ($report.raw_lines.Count -eq 13) "All raw lines must remain: expected=13 actual=$($report.raw_lines.Count); raw=[$rowDetails]"
    Assert (@($report.raw_lines | Where-Object { $_.status -eq 'invalid' }).Count -eq 2) 'Malformed lines must be reported'
    Assert (@($report.raw_lines | Where-Object { $_.status -eq 'unmatched' }).Count -eq 1) 'Capture mismatch must remain visible'
    Assert ($report.warnings -contains 'unterminated_final_line:2:7') 'Truncation warning missing'
    for ($i = 0; $i -lt $inputs.Count; $i++) {
        Assert ([Convert]::ToBase64String([IO.File]::ReadAllBytes($inputs[$i])) -eq $original[$i]) 'Input changed'
        Assert ($report.raw_inputs[$i].base64 -eq $original[$i]) 'Exact input bytes were not preserved'
    }
    Assert ($report.raw_lines[0].status -eq 'matched') 'BOM-less first host line must retain its opening brace'
    Assert ($report.raw_lines[6].status -eq 'matched') 'BOM-less first guest line must retain its opening brace'
    $bomPath = Join-Path $temp 'bom-host.jsonl'
    $hostBytes = [IO.File]::ReadAllBytes($inputs[0])
    $bomBytes = [byte[]](@(0xEF, 0xBB, 0xBF) + $hostBytes)
    [IO.File]::WriteAllBytes($bomPath, $bomBytes)
    & (Join-Path $root 'scripts/analyze-rdp-audio-diagnostics.ps1') -InputPath $bomPath -CaptureId $id -OutputPath (Join-Path $temp 'bom-report.json')
    $bomReport = Get-Content -Raw (Join-Path $temp 'bom-report.json') | ConvertFrom-Json
    Assert ($bomReport.timeline.Count -eq 6) "BOM-prefixed host must retain all rows: expected=6 actual=$($bomReport.timeline.Count)"
    Assert ($bomReport.raw_lines[0].status -eq 'matched') 'BOM first host line must parse'
    Assert ($bomReport.raw_inputs[0].base64 -eq [Convert]::ToBase64String($bomBytes)) 'Original BOM bytes must be preserved'
    $broken = Join-Path $temp 'broken.jsonl'
    [IO.File]::WriteAllBytes($broken, [byte[]]@(123, 34, 195))
    & (Join-Path $root 'scripts/analyze-rdp-audio-diagnostics.ps1') -InputPath $broken -CaptureId $id -OutputPath (Join-Path $temp 'broken-report.json')
    $brokenReport = Get-Content -Raw (Join-Path $temp 'broken-report.json') | ConvertFrom-Json
    Assert ($brokenReport.warnings -contains 'invalid_utf8:1') 'Broken UTF-8 must remain detectable'
    Assert ($brokenReport.raw_inputs[0].base64 -eq 'eyLD') 'Broken UTF-8 bytes must survive'

    # Delayed marker serialization must not hide a sample after handler arrival.
    $interleaved = @(
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 1; utc_unix_ms = 946684800000; elapsed_ms = 0; event = 'capture_started' },
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 2; utc_unix_ms = 946684800200; elapsed_ms = 200; acquired_elapsed_ms = 200; acquired_utc_unix_ms = 946684800200; event = 'meter_sample'; peak = 0.001 },
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 3; utc_unix_ms = 946684800400; elapsed_ms = 400; event = 'marker'; marker = 'stop_pressed'; received_elapsed_ms = 150; received_utc_unix_ms = 946684800150 }
    )
    $interleavedPath = Join-Path $temp 'interleaved.jsonl'
    [IO.File]::WriteAllText($interleavedPath, (($interleaved | ForEach-Object { $_ | ConvertTo-Json -Compress }) -join "`n") + "`n")
    & (Join-Path $root 'scripts/analyze-rdp-audio-diagnostics.ps1') -InputPath $interleavedPath -CaptureId $id -OutputPath (Join-Path $temp 'interleaved-report.json')
    $interleavedReport = Get-Content -Raw (Join-Path $temp 'interleaved-report.json') | ConvertFrom-Json
    Assert ($interleavedReport.stop_observations[0].delta_ms -eq 50) 'Handler-arrival marker interleaving must be respected'
    Assert ($interleavedReport.warnings -contains 'guest_missing') 'Missing guest must be explicit'
    Assert ($interleavedReport.warnings -contains 'capture_interrupted_or_incomplete:host') 'Interrupted capture must be explicit'
    $interleaved += @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 4; utc_unix_ms = 946684800500; elapsed_ms = 500; event = 'marker'; marker = 'playback_started'; received_elapsed_ms = 175 }
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
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 1; utc_unix_ms = 946684800000; elapsed_ms = 0; event = 'capture_started' },
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 2; utc_unix_ms = 946684800400; elapsed_ms = 400; received_elapsed_ms = 200; event = 'marker'; marker = 'stop_pressed' },
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 3; utc_unix_ms = 946684800500; elapsed_ms = 500; acquired_elapsed_ms = 100; event = 'meter_sample'; peak = 0.001 }
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
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 1; utc_unix_ms = 946684800000; elapsed_ms = 0; event = 'capture_started' },
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 2; utc_unix_ms = 946684800050; elapsed_ms = 50; event = 'marker'; marker = 'sound_stopped'; received_elapsed_ms = 50 },
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 3; utc_unix_ms = 946684800300; elapsed_ms = 300; received_elapsed_ms = 250; event = 'marker'; marker = 'playback_started' },
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 4; utc_unix_ms = 946684800400; elapsed_ms = 400; received_elapsed_ms = 150; event = 'marker'; marker = 'stop_pressed' },
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 5; utc_unix_ms = 946684800450; elapsed_ms = 450; acquired_elapsed_ms = 450; event = 'meter_sample'; peak = 0.001 }
    )
    $arrivalReport = Analyze-Synthetic $arrivalOrder 'arrival-order'
    Assert ($arrivalReport.stop_observations[0].status -eq 'playback_restarted_before_threshold') 'Restart arrival must override earlier append sequence'
    Assert ($null -eq $arrivalReport.stop_observations[0].delta_ms) 'Post-restart acquisition must not count'
    $arrivalOrder[3].received_elapsed_ms = 500
    $invalidMarkerReport = Analyze-Synthetic $arrivalOrder 'invalid-marker'
    Assert ($invalidMarkerReport.stop_observations[0].status -eq 'invalid_stop_time') 'Invalid present marker time must not silently fall back'
    Assert ($invalidMarkerReport.warnings -contains 'invalid_received_time:1:4') 'Invalid marker time warning missing'

    $sameMillisecond = @(
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 1; utc_unix_ms = 946684800000; elapsed_ms = 0; event = 'capture_started' },
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 2; utc_unix_ms = 946684800300; elapsed_ms = 300; received_elapsed_ms = 200; event = 'marker'; marker = 'stop_pressed' },
        @{ schema_version = 1; source = 'host'; capture_id = $id; sequence = 3; utc_unix_ms = 946684800400; elapsed_ms = 400; acquired_elapsed_ms = 200; event = 'meter_sample'; peak = 0.001 }
    )
    $sameReport = Analyze-Synthetic $sameMillisecond 'same-millisecond'
    Assert ($null -eq $sameReport.stop_observations[0].delta_ms) 'Same-ms acquisition must not be inferred later from append order'
    Assert ($sameReport.warnings -contains 'same_millisecond_sample_excluded:2') 'Same-ms ambiguity warning missing'
    $sameMillisecond[2].acquired_elapsed_ms = 250
    $sameMillisecond[2].sequence = 4
    $gapReport = Analyze-Synthetic $sameMillisecond 'sequence-gap'
    Assert ($gapReport.warnings -contains 'sequence_gap:host:3') 'Missing host sequence must be detected'
    Assert ($gapReport.stop_observations[0].status -eq 'incomplete_host_sequence') 'Host gaps must suppress latency'
    Assert ($null -eq $gapReport.stop_observations[0].delta_ms) 'Host gap must not yield a numeric difference'
    $aggregateSecond = Join-Path $temp 'aggregate-second.jsonl'
    [IO.File]::WriteAllBytes($aggregateSecond, [IO.File]::ReadAllBytes($inputs[0]))
    foreach ($limitCase in @('count', 'input_bytes', 'aggregate_bytes', 'report_bytes')) {
        $limitOutput = Join-Path $temp ('refused-' + $limitCase + '.json')
        $arguments = @{ InputPath = @($inputs[0]); CaptureId = $id; OutputPath = $limitOutput }
        if ($limitCase -eq 'count') { $arguments.InputPath = @($inputs[0], $inputs[0], $inputs[0]) }
        if ($limitCase -eq 'input_bytes') { $arguments.MaxInputBytes = 16 }
        if ($limitCase -eq 'aggregate_bytes') {
            $arguments.InputPath = @($inputs[0], $aggregateSecond)
            $arguments.MaxInputBytes = (Get-Item $inputs[0]).Length + 1
            Assert ((Get-Item $inputs[0]).Length -lt $arguments.MaxInputBytes -and (Get-Item $aggregateSecond).Length -lt $arguments.MaxInputBytes) 'Each aggregate fixture must individually fit under the byte limit'
        }
        if ($limitCase -eq 'report_bytes') { $arguments.MaxReportBytes = 16 }
        $refused = $false
        try { & (Join-Path $root 'scripts/analyze-rdp-audio-diagnostics.ps1') @arguments } catch { $refused = $true }
        Assert $refused "Analyzer must refuse $limitCase limit"
        Assert (-not [IO.File]::Exists($limitOutput)) "Analyzer must not create partial output for $limitCase limit"
    }

    # No platform event log or counter cmdlets are invoked by these mocks.
    function Get-WinEvent {
        [CmdletBinding()]param($ListLog, $LogName, $MaxEvents, $FilterXPath, [switch]$Oldest)
        if ($ListLog) { return [pscustomobject]@{ IsEnabled = $true } }
        if (-not $FilterXPath) { return [pscustomobject]@{ RecordId = 1 } }
        $entry = [pscustomobject]@{ RecordId = 2; Id = 132; TimeCreated = [DateTime]::UtcNow }
        $entry | Add-Member ScriptMethod ToXml { '<Event xmlns="http://schemas.microsoft.com/win/2004/08/events/event"><System><Provider Name="Microsoft-Windows-RemoteDesktopServices-RdpCoreTS"/><EventID>132</EventID></System><EventData><Data Name="ChannelName">rdpsnd</Data><Data Name="ClientAddress">synthetic-do-not-copy</Data><Data Name="UserName">synthetic-do-not-copy</Data></EventData></Event>' }
        $generic = [pscustomobject]@{ RecordId = 3; Id = 999; TimeCreated = [DateTime]::UtcNow }
        $generic | Add-Member ScriptMethod ToXml { '<Event><EventData><Data Name="UserName">rdpsnd</Data><Data Name="Other">AUDIO_PLAYBACK_DVC</Data></EventData></Event>' }
        return @($entry, $generic)
    }
    function Get-ItemProperty { [CmdletBinding()]param($LiteralPath, $Name) throw 'synthetic missing localization' }
    function Get-Counter { [CmdletBinding()]param($Counter, $MaxSamples, $ListSet) throw 'synthetic missing counter' }
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
    # Rollover retains newer IDs while losing unread events; a full batch means backlog.
    $eventMock = @{ initialized = $false }
    function Get-WinEvent {
        [CmdletBinding()]param($ListLog, $LogName, $MaxEvents, $FilterXPath, [switch]$Oldest)
        if ($ListLog) { return [pscustomobject]@{ IsEnabled = $true } }
        if (-not $FilterXPath) {
            if ($Oldest) { return [pscustomobject]@{ RecordId = 10 } }
            if (-not $eventMock.initialized) { $eventMock.initialized = $true; return [pscustomobject]@{ RecordId = 1 } }
            return [pscustomobject]@{ RecordId = 265 }
        }
        if ($FilterXPath.Contains('265')) { return }
        foreach ($recordId in 10..265) {
            $entry = [pscustomobject]@{ RecordId = $recordId; Id = 132; TimeCreated = [DateTime]::UtcNow }
            $entry | Add-Member ScriptMethod ToXml { '<Event xmlns="http://schemas.microsoft.com/win/2004/08/events/event"><System><EventID>132</EventID></System><EventData><Data Name="ChannelName">rdpsnd</Data><Data Name="UserName">synthetic-do-not-copy</Data></EventData></Event>' }
            Write-Output $entry
        }
    }
    $rolloverPath = Join-Path $temp 'rollover.jsonl'
    & (Join-Path $root 'scripts/collect-rdp-audio-diagnostics.ps1') -CaptureId $id -OutputPath $rolloverPath -DurationSeconds 2
    $rolloverRecords = @(Get-Content $rolloverPath | ForEach-Object { $_ | ConvertFrom-Json })
    $gaps = @($rolloverRecords | Where-Object { $_.event -eq 'collector_gap' -and $_.reason -eq 'log_rollover' })
    Assert ($gaps.Count -eq 1 -and $gaps[0].missing_record_count -eq 8) 'Rollover cursor gap must record eight missing headers exactly once'
    Assert (@($rolloverRecords | Where-Object { $_.event -eq 'rdp_audio_channel' }).Count -eq 256) 'Namespaced channel records must parse without replaying consumed cursor'
    Assert (@($rolloverRecords | Where-Object { $_.component -eq 'rdp_event_backlog' -and $_.state -eq 'pending' }).Count -eq 1) 'Full batch must signal backlog'
    Assert (@($rolloverRecords | Where-Object { $_.component -eq 'rdp_event_backlog' -and $_.state -eq 'clear' }).Count -eq 1) 'Drained cursor must clear backlog'
    # Failed conversion and malformed XML must each leave a gap and not discard
    # the later valid event in the same batch. No exception or XML text is stored.
    $conversionMock = @{ initialized = $false }
    function Get-WinEvent {
        [CmdletBinding()]param($ListLog, $LogName, $MaxEvents, $FilterXPath, [switch]$Oldest)
        if ($ListLog) { return [pscustomobject]@{ IsEnabled = $true } }
        if (-not $FilterXPath) {
            if ($Oldest) { return [pscustomobject]@{ RecordId = 1 } }
            if (-not $conversionMock.initialized) { $conversionMock.initialized = $true; return [pscustomobject]@{ RecordId = 1 } }
            return [pscustomobject]@{ RecordId = 4 }
        }
        if ($FilterXPath.Contains('4')) { return }
        $throws = [pscustomobject]@{ RecordId = 2; Id = 132; TimeCreated = [DateTime]::UtcNow }
        $throws | Add-Member ScriptMethod ToXml { throw 'synthetic-do-not-copy-exception' }
        $malformed = [pscustomobject]@{ RecordId = 3; Id = 132; TimeCreated = [DateTime]::UtcNow }
        $malformed | Add-Member ScriptMethod ToXml { '<Event><synthetic-do-not-copy' }
        $valid = [pscustomobject]@{ RecordId = 4; Id = 132; TimeCreated = [DateTime]::UtcNow }
        $valid | Add-Member ScriptMethod ToXml { '<Event xmlns="http://schemas.microsoft.com/win/2004/08/events/event"><EventData><Data Name="ChannelName">rdpsnd</Data></EventData></Event>' }
        return @($throws, $malformed, $valid)
    }
    $conversionPath = Join-Path $temp 'conversion-failure.jsonl'
    & (Join-Path $root 'scripts/collect-rdp-audio-diagnostics.ps1') -CaptureId $id -OutputPath $conversionPath -DurationSeconds 1
    $conversionRaw = [IO.File]::ReadAllText($conversionPath)
    $conversionRecords = @(Get-Content $conversionPath | ForEach-Object { $_ | ConvertFrom-Json })
    Assert (@($conversionRecords | Where-Object { $_.event -eq 'collector_gap' -and $_.reason -eq 'event_parse_failed' }).Count -eq 2) 'Both failed event conversions must leave explicit gaps'
    Assert (@($conversionRecords | Where-Object { $_.event -eq 'rdp_audio_channel' }).Count -eq 1) 'Later valid event must survive earlier conversion failures'
    Assert (-not $conversionRaw.Contains('synthetic-do-not-copy')) 'Conversion exception or malformed XML leaked'
    # Null and throwing timestamps must leave fixed gaps, advance the cursor, and
    # allow the later valid event through without persisting exception text.
    $timeGapMock = @{ initialized = $false; cursors = @() }
    function Get-WinEvent {
        [CmdletBinding()]param($ListLog, $LogName, $MaxEvents, $FilterXPath, [switch]$Oldest)
        if ($ListLog) { return [pscustomobject]@{ IsEnabled = $true } }
        if (-not $FilterXPath) {
            if ($Oldest) { return [pscustomobject]@{ RecordId = 1 } }
            if (-not $timeGapMock.initialized) { $timeGapMock.initialized = $true; return [pscustomobject]@{ RecordId = 1 } }
            return [pscustomobject]@{ RecordId = 4 }
        }
        $cursorMatch = [regex]::Match($FilterXPath, 'EventRecordID > (\d+)')
        if (-not $cursorMatch.Success) { throw 'synthetic_cursor_missing' }
        $cursor = [long]$cursorMatch.Groups[1].Value
        $timeGapMock.cursors += $cursor
        if ($cursor -lt 2) {
            $nullTime = [pscustomobject]@{ RecordId = 2; Id = 132; TimeCreated = $null }
            $nullTime | Add-Member ScriptMethod ToXml { throw 'synthetic-null-time-xml-secret' }
            return $nullTime
        }
        if ($cursor -lt 3) {
            $throwingTime = [pscustomobject]@{ RecordId = 3; Id = 132 }
            $throwingTime | Add-Member -MemberType ScriptProperty -Name TimeCreated -Value { throw 'synthetic-time-getter-secret' }
            $throwingTime | Add-Member ScriptMethod ToXml { throw 'synthetic-throwing-time-xml-secret' }
            return $throwingTime
        }
        if ($cursor -lt 4) {
            $valid = [pscustomobject]@{ RecordId = 4; Id = 132; TimeCreated = [DateTime]::UtcNow }
            $valid | Add-Member ScriptMethod ToXml { '<Event xmlns="http://schemas.microsoft.com/win/2004/08/events/event"><EventData><Data Name="ChannelName">rdpsnd</Data></EventData></Event>' }
            return $valid
        }
    }
    $timeGapPath = Join-Path $temp 'time-gap.jsonl'
    & (Join-Path $root 'scripts/collect-rdp-audio-diagnostics.ps1') -CaptureId $id -OutputPath $timeGapPath -DurationSeconds 4
    $timeGapRaw = [IO.File]::ReadAllText($timeGapPath)
    $timeGapRecords = @(Get-Content $timeGapPath | ForEach-Object { $_ | ConvertFrom-Json })
    $timeGaps = @($timeGapRecords | Where-Object { $_.event -eq 'collector_gap' -and $_.reason -eq 'event_time_unavailable' })
    Assert ($timeGaps.Count -eq 2) 'Null and throwing event timestamps must each leave a fixed gap'
    Assert (@($timeGapRecords | Where-Object { $_.event -eq 'rdp_audio_channel' -and $_.event_id -eq 132 }).Count -eq 1) 'Later valid event must survive null and throwing timestamps'
    Assert (($timeGapMock.cursors -contains 2) -and ($timeGapMock.cursors -contains 3) -and ($timeGapMock.cursors -contains 4)) 'Each timestamp gap must advance the cursor after recording'
    Assert (-not $timeGapRaw.Contains('synthetic-null-time-xml-secret') -and -not $timeGapRaw.Contains('synthetic-time-getter-secret') -and -not $timeGapRaw.Contains('synthetic-throwing-time-xml-secret')) 'Timestamp exception details leaked'
    # The English and Japanese tables share numeric indices, never guessed names.
    $syntheticLocale = 'en'
    function Get-ItemProperty {
        [CmdletBinding()]param($LiteralPath, $Name)
        if ($LiteralPath.EndsWith('\009') -or $syntheticLocale -eq 'en') { return [pscustomobject]@{ Counter = @('100', 'RemoteFX Network', '102', 'Current TCP RTT') } }
        return [pscustomobject]@{ Counter = @('100', '合成ネットワーク', '102', '合成往復時間') }
    }
    function Get-Counter {
        [CmdletBinding()]param($Counter, $MaxSamples, $ListSet)
        $objectName = 'RemoteFX Network'; $counterName = 'Current TCP RTT'
        if ($syntheticLocale -eq 'ja') { $objectName = '合成ネットワーク'; $counterName = '合成往復時間' }
        if ($ListSet) {
            if ($ListSet -cne $objectName) { throw 'unexpected_object' }
            return [pscustomobject]@{ CounterSetName = $objectName; Paths = @('\' + $objectName + '(*)\' + $counterName) }
        }
        if ($Counter -cne ('\' + $objectName + '(*)\' + $counterName)) { throw 'unexpected_counter' }
        return [pscustomobject]@{ CounterSamples = @([pscustomobject]@{ Status = 0; CookedValue = 12.5; InstanceName = 'synthetic-do-not-copy' }) }
    }
    foreach ($locale in @('en', 'ja')) {
        $syntheticLocale = $locale
        $localizedCapture = Join-Path $temp ('localized-' + $locale + '.jsonl')
        & (Join-Path $root 'scripts/collect-rdp-audio-diagnostics.ps1') -CaptureId $id -OutputPath $localizedCapture -DurationSeconds 1
        $localizedRaw = [IO.File]::ReadAllText($localizedCapture)
        $localizedRecords = @(Get-Content $localizedCapture | ForEach-Object { $_ | ConvertFrom-Json })
        $localizedSamples = @($localizedRecords | Where-Object { $_.event -eq 'counter_sample' -and $_.counter -eq 'tcp_rtt_ms' })
        Assert ($localizedSamples.Count -ge 1) "Localized counter should resolve for $locale; actual sample count=$($localizedSamples.Count)"
        Assert ($localizedSamples[0].mean -eq 12.5) 'Localized sample numeric value changed'
        Assert (-not $localizedRaw.Contains('合成') -and -not $localizedRaw.Contains('RemoteFX Network') -and -not $localizedRaw.Contains('synthetic-do-not-copy')) 'Localized paths or instances leaked'
    }
    Assert (@($records | Where-Object { $_.component -eq 'counter_localization' -and $_.state -eq 'unavailable' }).Count -eq 1) 'Missing localization must be explicit'
    Write-Output 'Synthetic diagnostics checks passed.'
} finally { [IO.Directory]::Delete($temp, $true) }
