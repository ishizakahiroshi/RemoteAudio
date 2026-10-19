#requires -Version 5.1
<#
Opt-in, local-only guest diagnostics. No recording or configuration changes.
Only allowlisted channel/counter enums and numeric data enter the JSONL file.
Run in the Windows RDP guest; the host capture ID must already be known.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidatePattern('^[0-9a-f]{32}$')][string]$CaptureId,
    [Parameter(Mandatory = $true)][string]$OutputPath,
    [ValidateRange(1, 1800)][int]$DurationSeconds = 300,
    [ValidateRange(1, 10)][int]$IntervalSeconds = 1,
    [ValidateRange(4096, 16777216)][long]$MaxBytes = 4194304
)
$ErrorActionPreference = 'Stop'
$encoding = New-Object System.Text.UTF8Encoding($false)
# CreateNew deliberately refuses to overwrite previous evidence.
$stream = New-Object System.IO.FileStream([IO.Path]::GetFullPath($OutputPath), [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
$clock = [Diagnostics.Stopwatch]::StartNew()
$script:sequence = 0L
$script:limitReached = $false
$script:states = @{}
$reason = 'user'
function Write-Record([string]$Event, [hashtable]$Fields, [switch]$Final) {
    $row = [ordered]@{ schema_version = 1; source = 'guest'; capture_id = $CaptureId; sequence = $script:sequence; utc_unix_ms = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds(); elapsed_ms = $clock.ElapsedMilliseconds; event = $Event }
    foreach ($key in $Fields.Keys) { $row[$key] = $Fields[$key] }
    $bytes = $encoding.GetBytes(($row | ConvertTo-Json -Depth 5 -Compress) + "`n")
    # Reserve room for a terminal event. Each write is flushed for interrupted runs.
    if (-not $Final -and ($stream.Length + $bytes.Length + 1024 -gt $MaxBytes)) { $script:limitReached = $true; return }
    if ($stream.Length + $bytes.Length -gt $MaxBytes) { return }
    $stream.Write($bytes, 0, $bytes.Length)
    $stream.Flush()
    $script:sequence++
}
function Set-State([string]$Component, [string]$State) {
    if ($script:states[$Component] -ne $State) {
        Write-Record 'collector_state' @{ component = $Component; state = $State }
        $script:states[$Component] = $State
    }
}
# These stable output keys never contain a counter instance/session/computer name.
$metrics = @(
    @{ key = 'tcp_rtt_ms'; path = '\RemoteFX Network(*)\Current TCP RTT' },
    @{ key = 'udp_rtt_ms'; path = '\RemoteFX Network(*)\Current UDP RTT' },
    @{ key = 'tcp_bandwidth'; path = '\RemoteFX Network(*)\Current TCP Bandwidth' },
    @{ key = 'udp_bandwidth'; path = '\RemoteFX Network(*)\Current UDP Bandwidth' },
    @{ key = 'input_frames_per_second'; path = '\RemoteFX Graphics(*)\Input Frames/Second' },
    @{ key = 'output_frames_per_second'; path = '\RemoteFX Graphics(*)\Output Frames/Second' },
    @{ key = 'average_encoding_time'; path = '\RemoteFX Graphics(*)\Average Encoding Time' },
    @{ key = 'frames_skipped_network'; path = '\RemoteFX Graphics(*)\Frames Skipped/Second - Insufficient Network Resources' },
    @{ key = 'frames_skipped_client'; path = '\RemoteFX Graphics(*)\Frames Skipped/Second - Insufficient Client Resources' },
    @{ key = 'frames_skipped_server'; path = '\RemoteFX Graphics(*)\Frames Skipped/Second - Insufficient Server Resources' }
)
# Resolve the fixed English allowlist through Windows' numeric Perflib indices.
# Localized names/paths stay transient; no guessed translations or broad searches.
function Resolve-CounterPaths {
    try {
        $english = @((Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Perflib\009' -Name Counter -ErrorAction Stop).Counter)
        $localized = @((Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Perflib\CurrentLanguage' -Name Counter -ErrorAction Stop).Counter)
        if ($english.Count % 2 -ne 0 -or $localized.Count % 2 -ne 0 -or $english.Count -eq 0 -or $localized.Count -eq 0) { throw 'invalid_table' }
        $englishIndex = @{}
        $localName = @{}
        for ($i = 0; $i -lt $english.Count; $i += 2) {
            if ([string]$english[$i] -notmatch '^[0-9]+$') { throw 'invalid_index' }
            $englishIndex[[string]$english[$i + 1]] = [string]$english[$i]
        }
        for ($i = 0; $i -lt $localized.Count; $i += 2) {
            if ([string]$localized[$i] -notmatch '^[0-9]+$') { throw 'invalid_index' }
            $localName[[string]$localized[$i]] = [string]$localized[$i + 1]
        }
        Set-State 'counter_localization' 'available'
        foreach ($metric in $metrics) {
            $parts = $metric.path.Split('\')
            $englishObject = $parts[1].Substring(0, $parts[1].Length - 3)
            $englishCounter = $parts[2]
            if (-not $englishIndex.ContainsKey($englishObject) -or -not $englishIndex.ContainsKey($englishCounter)) { continue }
            $objectIndex = $englishIndex[$englishObject]
            $counterIndex = $englishIndex[$englishCounter]
            if (-not $localName.ContainsKey($objectIndex) -or -not $localName.ContainsKey($counterIndex)) { continue }
            $objectName = $localName[$objectIndex]
            $counterName = $localName[$counterIndex]
            try {
                $sets = @(Get-Counter -ListSet $objectName -ErrorAction Stop)
                $verified = $false
                foreach ($set in $sets) {
                    if ($set.CounterSetName -cne $objectName) { continue }
                    foreach ($path in $set.Paths) {
                        if ([string]$path.Substring($path.LastIndexOf('\') + 1) -ceq $counterName) { $verified = $true; break }
                    }
                }
                if ($verified) { $metric.resolvedPath = '\' + $objectName + '(*)\' + $counterName }
            } catch { } # This fixed metric remains explicitly unavailable.
        }
    } catch { Set-State 'counter_localization' 'unavailable' }
}
$channels = @{ 'rdpsnd' = 'rdpsnd'; 'audio_playback_dvc' = 'audio_playback_dvc'; 'audio_playback_lossy_dvc' = 'audio_playback_lossy_dvc' }
$logName = 'Microsoft-Windows-RemoteDesktopServices-RdpCoreTS/Operational'
$lastRecord = 0L
$startedUtc = [DateTime]::UtcNow
try {
    Write-Record 'capture_started' @{}
    Resolve-CounterPaths
    # Establish a cursor, without recording old events from earlier sessions.
    try {
        $log = Get-WinEvent -ListLog $logName -ErrorAction Stop
        if (-not $log.IsEnabled) { throw 'disabled' }
        $latest = @(Get-WinEvent -LogName $logName -MaxEvents 1 -ErrorAction SilentlyContinue)
        if ($latest.Count -gt 0) { $lastRecord = [long]$latest[0].RecordId }
        Set-State 'rdp_events' 'available'
    } catch { Set-State 'rdp_events' 'unavailable' }
    while ($clock.Elapsed.TotalSeconds -lt $DurationSeconds -and -not $script:limitReached) {
        try {
            $log = Get-WinEvent -ListLog $logName -ErrorAction Stop
            if (-not $log.IsEnabled) { throw 'disabled' }
            # Normal rollover can discard unread records while newest IDs continue
            # increasing. Compare the oldest retained header with the next cursor.
            $oldest = @(Get-WinEvent -LogName $logName -Oldest -MaxEvents 1 -ErrorAction SilentlyContinue)
            if ($oldest.Count -gt 0 -and $lastRecord -gt 0 -and [long]$oldest[0].RecordId -gt $lastRecord + 1) {
                Write-Record 'collector_gap' @{ component = 'rdp_events'; reason = 'log_rollover'; missing_record_count = [long]$oldest[0].RecordId - $lastRecord - 1 }
                $lastRecord = [long]$oldest[0].RecordId - 1
            }
            # Oldest-first and bounded batches prevent silent loss under normal load.
            # Do not read Message or persist arbitrary EventData, even on failures.
            $xpath = "*[System[EventRecordID > $lastRecord]]"
            $readErrors = @()
            $events = @(Get-WinEvent -LogName $logName -FilterXPath $xpath -Oldest -MaxEvents 256 -ErrorAction SilentlyContinue -ErrorVariable readErrors)
            if ($readErrors.Count -gt 0 -and @($readErrors | Where-Object { $_.FullyQualifiedErrorId -notlike 'NoMatchingEventsFound*' }).Count -gt 0) { throw 'event_read_failed' }
            Set-State 'rdp_events' 'available'
            foreach ($entry in $events) {
                $eventTimeUtc = $null
                try {
                    $eventTime = $entry.TimeCreated
                    if ($null -eq $eventTime) { throw 'event_time_unavailable' }
                    $eventTimeUtc = $eventTime.ToUniversalTime()
                } catch {
                    # The timestamp is nullable and can fail independently of XML.
                    # Record only a fixed gap, then advance past the bad record.
                    Write-Record 'collector_gap' @{ component = 'rdp_events'; reason = 'event_time_unavailable' }
                    $lastRecord = [long]$entry.RecordId
                    if ($script:limitReached) { break }
                    continue
                }
                if ($eventTimeUtc -lt $startedUtc) { $lastRecord = [long]$entry.RecordId; continue }
                try { $xml = [xml]$entry.ToXml() } catch {
                    # Expose lost conversion evidence before advancing past this
                    # record; never persist the XML or exception description.
                    Write-Record 'collector_gap' @{ component = 'rdp_events'; reason = 'event_parse_failed' }
                    $lastRecord = [long]$entry.RecordId
                    if ($script:limitReached) { break }
                    continue
                }
                $matched = @{}
                foreach ($data in $xml.SelectNodes("/*[local-name()='Event']/*[local-name()='EventData']/*[local-name()='Data']")) {
                    # A matching arbitrary value (for example UserName=rdpsnd)
                    # is not evidence of an audio channel. Only the known field.
                    if ($data.GetAttribute('Name') -cne 'ChannelName') { continue }
                    $value = ([string]$data.InnerText).ToLowerInvariant()
                    if ($channels.ContainsKey($value)) { $matched[$channels[$value]] = $true }
                }
                foreach ($channel in $matched.Keys) {
                    Write-Record 'rdp_audio_channel' @{ channel = $channel; event_id = [int]$entry.Id; event_utc_unix_ms = ([DateTimeOffset]$eventTimeUtc).ToUnixTimeMilliseconds() }
                }
                $lastRecord = [long]$entry.RecordId
                if ($script:limitReached) { break }
            }
            if ($events.Count -eq 256) { Set-State 'rdp_event_backlog' 'pending' } else { Set-State 'rdp_event_backlog' 'clear' }
            # A cleared log restarts record numbering; expose the gap and reset cursor.
            $latest = @(Get-WinEvent -LogName $logName -MaxEvents 1 -ErrorAction SilentlyContinue)
            if ($latest.Count -gt 0 -and [long]$latest[0].RecordId -lt $lastRecord) {
                Write-Record 'collector_gap' @{ component = 'rdp_events'; reason = 'log_reset' }
                $lastRecord = 0L
            }
        } catch { Set-State 'rdp_events' 'unavailable' }
        foreach ($metric in $metrics) {
            if ($script:limitReached -or $clock.Elapsed.TotalSeconds -ge $DurationSeconds) { break }
            try {
                if (-not $metric.ContainsKey('resolvedPath')) { throw 'counter_not_resolved' }
                $result = Get-Counter -Counter $metric.resolvedPath -MaxSamples 1 -ErrorAction Stop
                $values = @($result.CounterSamples | Where-Object { $_.Status -eq 0 -and -not [double]::IsNaN($_.CookedValue) -and -not [double]::IsInfinity($_.CookedValue) } | ForEach-Object { [double]$_.CookedValue })
                if ($values.Count -eq 0) { throw 'no_valid_samples' }
                Set-State $metric.key 'available'
                $summary = $values | Measure-Object -Minimum -Maximum -Average
                Write-Record 'counter_sample' @{ counter = $metric.key; units = 'unknown_unit'; instance_count = $values.Count; minimum = $summary.Minimum; maximum = $summary.Maximum; mean = $summary.Average }
            } catch { Set-State $metric.key 'unavailable' }
        }
        $remaining = [Math]::Max(0, ($DurationSeconds * 1000) - $clock.ElapsedMilliseconds)
        if (-not $script:limitReached -and $remaining -gt 0) { Start-Sleep -Milliseconds ([int][Math]::Min($IntervalSeconds * 1000, $remaining)) }
    }
    if ($script:limitReached) { $reason = 'size_limit' } else { $reason = 'duration_limit' }
} catch {
    $reason = 'collector_error'
    # Never serialize exception messages, paths, machine names, or native payloads.
    Write-Warning 'Collector stopped after an error. Inspect collector state records; no exception payload was saved.'
} finally {
    try { Write-Record 'capture_stopped' @{ reason = $reason } -Final } finally { $stream.Dispose(); $clock.Stop() }
}
