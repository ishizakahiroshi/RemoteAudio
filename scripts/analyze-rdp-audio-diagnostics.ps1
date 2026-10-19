#requires -Version 5.1
<# Read-only analysis. The report retains every raw line, including invalid input.
Treat the report with the same privacy care as the input files. Output is CreateNew.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string[]]$InputPath,
    [Parameter(Mandatory = $true)][ValidatePattern('^[0-9a-f]{32}$')][string]$CaptureId,
    [Parameter(Mandatory = $true)][string]$OutputPath,
    [ValidateRange(0.000001, 1.0)][double]$Threshold = 0.01,
    [ValidateRange(1, 33554432)][long]$MaxInputBytes = 33554432,
    [ValidateRange(1, 134217728)][long]$MaxReportBytes = 134217728
)
$ErrorActionPreference = 'Stop'
# Hard bounds suit one host plus one guest capture. Callers can lower byte limits.
if ($InputPath.Count -lt 1 -or $InputPath.Count -gt 2) { throw 'Analysis accepts one or two input files only.' }
$totalInputBytes = 0L
foreach ($path in $InputPath) {
    $file = New-Object System.IO.FileInfo([IO.Path]::GetFullPath($path))
    $totalInputBytes += $file.Length
    if ($totalInputBytes -gt $MaxInputBytes) { throw 'Aggregate input byte limit exceeded; no report was created.' }
}
$readInputBytes = 0L
$rows = New-Object 'System.Collections.Generic.List[object]'
$warnings = New-Object 'System.Collections.Generic.List[string]'
$matched = New-Object 'System.Collections.Generic.List[object]'
$last = @{}
$inputBytes = New-Object 'System.Collections.Generic.List[object]'
$badHostOrder = $false
function Is-Integer($Value) {
    return ($null -ne $Value -and ($Value -is [long] -or $Value -is [int] -or $Value -is [decimal] -or $Value -is [double]) -and -not [double]::IsNaN([double]$Value) -and -not [double]::IsInfinity([double]$Value) -and [double]$Value -ge 0 -and [double]$Value -eq [Math]::Floor([double]$Value))
}
$inputIndex = 0
foreach ($path in $InputPath) {
    $inputIndex++
    # Preserve exact input bytes as well as readable lines, including broken UTF-8
    # from a partially written final character. Source files are untouched.
    # Recheck through an open stream and allocate only a bounded buffer. Refuse
    # files that grow/shrink while being read rather than producing partial evidence.
    $inputStream = [IO.File]::OpenRead([IO.Path]::GetFullPath($path))
    try {
        $length = $inputStream.Length
        if ($length -gt $MaxInputBytes - $readInputBytes) { throw 'Aggregate input byte limit exceeded; no report was created.' }
        $bytes = [byte[]]::new([int]$length)
        $offset = 0
        while ($offset -lt $bytes.Length) {
            $read = $inputStream.Read($bytes, $offset, $bytes.Length - $offset)
            if ($read -eq 0) { throw 'Input changed during analysis; no report was created.' }
            $offset += $read
        }
        if ($inputStream.ReadByte() -ne -1) { throw 'Input changed during analysis; no report was created.' }
        $readInputBytes += $length
    } finally { $inputStream.Dispose() }
    $inputBytes.Add([pscustomobject]@{ input_index = $inputIndex; base64 = [Convert]::ToBase64String($bytes) })
    $strictEncoding = New-Object System.Text.UTF8Encoding($false, $true)
    try { $content = $strictEncoding.GetString($bytes) } catch {
        $warnings.Add("invalid_utf8:$inputIndex")
        $content = [Text.Encoding]::UTF8.GetString($bytes)
    }
    # Culture-sensitive StartsWith can treat U+FEFF as ignorable and match a
    # BOM-less document, stripping its opening '{'. Compare the actual code unit.
    if ($content.Length -gt 0 -and $content[0] -eq [char]0xFEFF) { $content = $content.Substring(1) }
    $lines = [regex]::Split($content, "`n")
    $count = $lines.Length
    if ($content.EndsWith("`n")) { $count-- }
    if ($content.Length -eq 0) { $count = 0; $warnings.Add("empty_input:$inputIndex") }
    $lineNumber = 0
    foreach ($raw in $lines | Select-Object -First $count) {
        $lineNumber++
        $row = [ordered]@{ input_index = $inputIndex; line_number = $lineNumber; raw = $raw; status = 'invalid'; issue = $null; terminated = ($lineNumber -lt $count -or $content.EndsWith("`n")) }
        $record = $null
        try { $record = ConvertFrom-Json -InputObject $raw -ErrorAction Stop } catch { $row.issue = 'malformed_json' }
        if ($null -ne $record) {
            if ($record -is [Array] -or -not (Is-Integer $record.schema_version) -or $record.schema_version -ne 1 -or $record.source -notin @('host', 'guest') -or $record.capture_id -cnotmatch '^[0-9a-f]{32}$' -or -not (Is-Integer $record.sequence) -or -not (Is-Integer $record.utc_unix_ms) -or -not (Is-Integer $record.elapsed_ms) -or $record.event -isnot [string]) {
                $row.issue = 'invalid_envelope'
            } elseif ($record.capture_id -cne $CaptureId) { $row.status = 'unmatched'; $row.issue = 'capture_id_mismatch' }
            else {
                $row.status = 'matched'
                $key = "$inputIndex/$($record.source)"
                if ($last.ContainsKey($key) -and ($record.sequence -le $last[$key].sequence -or $record.elapsed_ms -lt $last[$key].elapsed_ms)) { $warnings.Add("nonmonotonic_input:${inputIndex}:$lineNumber"); if ($record.source -eq 'host') { $badHostOrder = $true } }
                $last[$key] = $record
                $matched.Add([pscustomobject]@{ input_index = $inputIndex; line_number = $lineNumber; record = $record })
            }
        } elseif ($null -eq $row.issue) { $row.issue = 'invalid_envelope' }
        if (-not $row.terminated) { $warnings.Add("unterminated_final_line:${inputIndex}:$lineNumber") }
        if ($row.status -eq 'invalid') { $warnings.Add("invalid_line:${inputIndex}:$lineNumber") }
        $rows.Add([pscustomobject]$row)
    }
}
$hostRows = @($matched | Where-Object { $_.record.source -eq 'host' } | Sort-Object { $_.record.sequence })
$guestRows = @($matched | Where-Object { $_.record.source -eq 'guest' })
if ($hostRows.Count -eq 0) { $warnings.Add('host_missing') }
if ($guestRows.Count -eq 0) { $warnings.Add('guest_missing') }
foreach ($source in @('host', 'guest')) {
    $sourceRows = @($matched | Where-Object { $_.record.source -eq $source })
    if ($sourceRows.Count -eq 0) { continue }
    if (@($sourceRows | Where-Object { $_.record.event -eq 'capture_stopped' }).Count -eq 0) { $warnings.Add("capture_interrupted_or_incomplete:$source") }
    if (@($sourceRows | Where-Object { $_.record.event -eq 'capture_started' }).Count -eq 0) { $warnings.Add("capture_start_missing:$source") }
    $orderedRows = @($sourceRows | Sort-Object { $_.record.sequence })
    $expectedSequence = 0L
    if ($source -eq 'host') { $expectedSequence = 1L }
    foreach ($orderedRow in $orderedRows) {
        if ($orderedRow.record.sequence -gt $expectedSequence) { $warnings.Add("sequence_gap:${source}:$expectedSequence") }
        $expectedSequence = [Math]::Max($expectedSequence, $orderedRow.record.sequence + 1)
    }
    if (@($sourceRows | Group-Object { $_.record.sequence } | Where-Object { $_.Count -gt 1 }).Count -gt 0) { $warnings.Add("duplicate_sequence:$source") }
}
# Canonical envelope times describe append order. Explicit event times describe
# arrival/acquisition, which can precede another record despite a later sequence.
function Event-Time($Item, [string]$Prefix, [bool]$Required) {
    $record = $Item.record
    $elapsedName = $Prefix + '_elapsed_ms'
    $utcName = $Prefix + '_utc_unix_ms'
    $hasElapsed = $null -ne $record.PSObject.Properties[$elapsedName]
    $hasUtc = $null -ne $record.PSObject.Properties[$utcName]
    if (-not $hasElapsed) {
        if ($Required -or $hasUtc) {
            $warnings.Add("missing_${Prefix}_time:$($Item.input_index):$($Item.line_number)")
            return $null
        }
        $warnings.Add("legacy_marker_append_time:$($Item.input_index):$($Item.line_number)")
        return [pscustomobject]@{ elapsed = $record.elapsed_ms; utc = $record.utc_unix_ms }
    }
    if (-not (Is-Integer $record.$elapsedName) -or $record.$elapsedName -gt $record.elapsed_ms -or ($hasUtc -and -not (Is-Integer $record.$utcName))) {
        $warnings.Add("invalid_${Prefix}_time:$($Item.input_index):$($Item.line_number)")
        return $null
    }
    # Wall clocks can jump, so UTC is validated as numeric, not <= append UTC.
    $utc = $null
    if ($hasUtc) { $utc = $record.$utcName }
    return [pscustomobject]@{ elapsed = $record.$elapsedName; utc = $utc }
}
$markers = @{}
$samples = @{}
foreach ($item in $hostRows) {
    $key = "$($item.input_index)/$($item.line_number)"
    if ($item.record.event -eq 'marker') { $markers[$key] = Event-Time $item 'received' $false }
    if ($item.record.event -eq 'meter_sample') { $samples[$key] = Event-Time $item 'acquired' $true }
}
$latencies = New-Object 'System.Collections.Generic.List[object]'
foreach ($stop in $hostRows | Where-Object { $_.record.event -eq 'marker' -and $_.record.marker -eq 'stop_pressed' }) {
    $stopTime = $markers["$($stop.input_index)/$($stop.line_number)"]
    $measurement = [ordered]@{ stop_sequence = $stop.record.sequence; stop_elapsed_ms = $null; stop_utc_unix_ms = $null; threshold = $Threshold; status = 'invalid_stop_time'; first_below_sequence = $null; delta_ms = $null }
    if ($null -eq $stopTime) { $latencies.Add([pscustomobject]$measurement); continue }
    $measurement.stop_elapsed_ms = $stopTime.elapsed
    $measurement.stop_utc_unix_ms = $stopTime.utc
    $measurement.status = 'no_following_below_threshold_sample'
    # Arrival order is authoritative. Sequence only resolves equal-time markers.
    $restartElapsed = $null
    $invalidRestart = $false
    foreach ($start in $hostRows | Where-Object { $_.record.event -eq 'marker' -and $_.record.marker -eq 'playback_started' }) {
        $startTime = $markers["$($start.input_index)/$($start.line_number)"]
        if ($null -eq $startTime) { $invalidRestart = $true; continue }
        if ($startTime.elapsed -gt $stopTime.elapsed -or ($startTime.elapsed -eq $stopTime.elapsed -and $start.record.sequence -gt $stop.record.sequence)) {
            if ($null -eq $restartElapsed -or $startTime.elapsed -lt $restartElapsed) { $restartElapsed = $startTime.elapsed }
        }
    }
    if ($null -ne $restartElapsed) { $measurement.status = 'playback_restarted_before_threshold' }
    if (@($hostRows | Where-Object { $sampleTime = $samples["$($_.input_index)/$($_.line_number)"]; $null -ne $sampleTime -and $sampleTime.elapsed -eq $stopTime.elapsed }).Count -gt 0) {
        $warnings.Add("same_millisecond_sample_excluded:$($stop.record.sequence)")
    }
    $following = @($hostRows | Where-Object {
        $time = $samples["$($_.input_index)/$($_.line_number)"]
        $_.record.event -eq 'meter_sample' -and $null -ne $time -and $time.elapsed -gt $stopTime.elapsed -and ($null -eq $restartElapsed -or $time.elapsed -lt $restartElapsed)
    } | Sort-Object { $samples["$($_.input_index)/$($_.line_number)"].elapsed }, { $_.record.sequence })
    foreach ($item in $following) {
        $sample = $item.record
        if ($sample.peak -isnot [ValueType] -or $sample.peak -is [bool] -or [double]::IsNaN([double]$sample.peak) -or [double]::IsInfinity([double]$sample.peak) -or $sample.peak -lt 0 -or $sample.peak -gt 1) { $warnings.Add('invalid_peak_sample'); continue }
        if ($sample.peak -lt $Threshold) {
            $measurement.status = 'observed'
            $measurement.first_below_sequence = $sample.sequence
            $measurement.delta_ms = $samples["$($item.input_index)/$($item.line_number)"].elapsed - $stopTime.elapsed
            break
        }
    }
    if ($invalidRestart) { $measurement.status = 'invalid_playback_boundary'; $measurement.delta_ms = $null }
    if (@($warnings | Where-Object { $_ -like 'sequence_gap:host:*' }).Count -gt 0) { $measurement.status = 'incomplete_host_sequence'; $measurement.delta_ms = $null }
    if ($badHostOrder -or $warnings.Contains('duplicate_sequence:host')) {
        $measurement.status = 'ambiguous_host_order'; $measurement.delta_ms = $null
    }
    $latencies.Add([pscustomobject]$measurement)
}
$timeline = @($matched | Sort-Object { $_.record.utc_unix_ms }, { $_.record.source }, { $_.record.sequence })
$report = [ordered]@{ schema_version = 1; capture_id = $CaptureId; threshold = $Threshold; warnings = @($warnings | Select-Object -Unique); limitations = @('UTC timeline requires synchronized host and guest clocks; no automatic offset correction.', 'Markers are host arrival times, not exact playback-button click times.', 'Peak is aggregate host output, not recorded audio, per-process attribution, or proof of audible sound.', 'The first sample below threshold is not a sustained-silence measurement; sampling gaps and endpoint changes may affect it.', 'Raw lines are retained. Review input and report before sharing.'); stop_observations = @($latencies.ToArray()); timeline = $timeline; raw_lines = @($rows.ToArray()); raw_inputs = @($inputBytes.ToArray()) }
$json = $report | ConvertTo-Json -Depth 40
$outputEncoding = New-Object System.Text.UTF8Encoding($false)
if ($outputEncoding.GetByteCount($json) + 1 -gt $MaxReportBytes) { throw 'Report byte limit exceeded; no report was created.' }
$bytes = $outputEncoding.GetBytes($json + "`n")
$output = New-Object System.IO.FileStream([IO.Path]::GetFullPath($OutputPath), [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
try { $output.Write($bytes, 0, $bytes.Length) } finally { $output.Dispose() }
