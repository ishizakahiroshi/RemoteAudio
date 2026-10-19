# RDP audio delay diagnostics

This opt-in capture helps compare a stop marker, host output level, RemoteAudio operations, and optional guest RDP observations. It does not establish the cause of a delay.

## Capture on the host

1. Run RemoteAudio in the Windows host's physical console session, as usual. Open its browser UI and the diagnostics section. Capture is off at startup; restarting does not resume it.
2. Choose **Start capture** and copy its capture ID. A capture is written locally as UTF-8 JSONL under `%APPDATA%\audioremote\diagnostics\`. No recording or upload occurs.
3. Optionally start the guest collector below with that same ID.
4. Press the fixed **Playback started** marker when starting playback. At the time you stop playback, press **Stop button pressed**. When you actually hear silence, press **Sound stopped**. Repeat if useful.
5. Choose **Stop capture**, then download the capture from the history list. Host captures are limited to 30 minutes and 16 MiB and can stop automatically; its terminal event gives the reason. Retain the guest file separately.
6. After saving anything needed, choose **Delete all captures** and check the displayed count before confirming. This removes only app-managed host capture JSONL files, not downloaded copies or guest files. History is not automatically deleted.

Markers record when the request reaches the host, not the exact media-player click or moment your ears detect silence. Browser interaction and network latency add uncertainty. There is no automatic YouTube integration.

Capture startup writes and syncs a complete header to a unique `.pending` staging file, then publishes the JSONL without replacing existing files. The filesystem must support hard links; otherwise startup fails safely. A process crash during startup can leave `.pending` files. These unverified staging files are retained and excluded from capture history/download/purge; inspect them manually before deciding whether to remove them. A successfully published header remains visible as an interrupted capture if startup fails afterward.


## What the host measures

About every 100 ms, the current default render endpoint's aggregate peak, master volume, and mute state are sampled. Endpoints are anonymous capture-local slots such as `endpoint_1`. RemoteAudio device, volume, mute, and test-sound operations are also logged during capture. A meter failure is a state transition, with a numeric HRESULT for a COM failure; repeated identical failures are not emitted each tick.

Peak values are not audio recordings, per-process measurements, or proof of sound reaching speakers. Another application can keep the peak high. A measured zero peak can occur during mute, endpoint changes, or a quiet passage. Missing samples or an unavailable meter mean unknown output, never an assumed zero. Logs do not contain tokens, request headers, IP addresses, machine/user names, raw endpoint IDs/names, arbitrary marker text, or event message bodies. Review files before sharing; externally modified input files may contain other information.

## Optional Windows guest collector

The scripts are in the source checkout's `scripts` directory. They are not installed as a guest application. Use Windows PowerShell 5.1 or PowerShell 7 with Windows event/counter cmdlets. No dependencies, administrator setup, RDP configuration changes, or event-channel enabling are performed. Respect your local execution policy; do not bypass it just to run diagnostics.

Run this in the Windows RDP guest, replacing the synthetic ID with the host's active capture ID and choosing an existing local folder:

```powershell
.\scripts\collect-rdp-audio-diagnostics.ps1 `
  -CaptureId '0123456789abcdef0123456789abcdef' `
  -OutputPath '.\guest-capture.jsonl' `
  -DurationSeconds 300 -IntervalSeconds 1 -MaxBytes 4194304
```

The output must not already exist. Collection runs up to the duration or size limit (maximum accepted values: 1,800 seconds and 16 MiB). Size budgeting reserves room for `capture_stopped`. Cmdlet calls can add scheduling delay to the time limit. Ctrl+C normally runs cleanup, but forced process termination, disk failure, or power loss can leave a truncated last line or no terminal event. Keep that file for analysis.

The collector reads only new RdpCoreTS Operational events. It inspects XML transiently for exact audio channel names `RDPSND`, `AUDIO_PLAYBACK_DVC`, and `AUDIO_PLAYBACK_LOSSY_DVC`, saving only a fixed canonical channel label, numeric event ID, and original UTC event time. It never saves the event message or arbitrary EventData. Event IDs are preserved as observations rather than translated into a claimed connection state; their meaning depends on the Windows provider/version. No matching event is not proof of a healthy or absent audio channel. These channel names are defined in [Microsoft's audio transport specification](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpea/40527932-258d-4664-bf5a-e569222e23ed).

Available RemoteFX Network/Graphics counters are selected from a fixed English-name allowlist. The collector maps those names through read-only Perflib `009` and `CurrentLanguage` numeric-index tables, then verifies the runtime-localized names with `Get-Counter -ListSet`. It does not guess translations. Missing localization tables or counters, disabled/inaccessible event logs, or unavailable cmdlets produce `unavailable` state records and collection continues. Localized names, paths, and instance labels are never saved. Instances are aggregated into count/minimum/maximum/mean without saving session or instance names. Counter values can reflect other sessions; values retain Windows counter units and are explicitly labeled `unknown_unit` because unit interpretation is not verified by this collector. This is not packet capture or an audio-specific transport latency measurement. Event backlogs, detected log resets, and rollover that moves the oldest retained record beyond the unread cursor are flagged. This does not recover missing events; events with no recognized `ChannelName` field can still leave observation gaps.

## Analyze without modifying evidence

Copy the downloaded host file and optional guest file into a local folder. Run:

```powershell
.\scripts\analyze-rdp-audio-diagnostics.ps1 `
  -InputPath '.\host-capture.jsonl', '.\guest-capture.jsonl' `
  -CaptureId '0123456789abcdef0123456789abcdef' `
  -OutputPath '.\capture-report.json' -Threshold 0.01
```

For host-only analysis, supply only the host path. The analyzer accepts at most two files, with at most 32 MiB of aggregate input, and caps the report at 128 MiB. `-MaxInputBytes` and `-MaxReportBytes` may lower those caps. Excess input or report size is refused before creating an output file. Inputs changing length during reading are refused. The report path must be new. Source files are never overwritten. Keep both original files and the report; reports can be larger because they preserve exact input bytes as base64 as well as readable raw lines. A report inherits all privacy risks of its inputs.

- `timeline`: matching, valid envelope records ordered by UTC; includes source file index and line number. Guest channel records also carry the event's original `event_utc_unix_ms`; their envelope timestamp is collection time.
- `stop_observations`: for each host `stop_pressed`, the difference from marker arrival to the first valid peak acquired after that arrival and strictly below the threshold. The window ends at the next playback-start arrival, even if its append sequence precedes the stop marker. Equality does not count. Missing data yields an explicit status, never a fabricated zero.
- `warnings`: malformed/truncated data, missing guest/host, incomplete capture, and duplicate/out-of-order records. Ambiguous host ordering or any missing host sequence suppresses reported differences conservatively; raw evidence remains available. Guest sequence gaps warn without changing host-only measurements.
- `raw_lines` / `raw_inputs`: every input line and its status, plus exact bytes, including malformed, truncated, or nonmatching capture IDs. These are retained even when excluded from the timeline.

Elapsed times have millisecond resolution. A sample acquired in the same millisecond as stop arrival is excluded as ambiguous, regardless of append sequence; only a strictly later acquisition can count. A single below-threshold sample is not sustained silence. Check the surrounding samples, endpoint slots, meter state, and operations. Host/guest UTC clocks must be synchronized for cross-machine comparison; the analyzer does not guess a clock offset. Envelope `elapsed_ms` / `utc_unix_ms` describe append time. Marker `received_elapsed_ms` / `received_utc_unix_ms` describe handler arrival, and meter `acquired_elapsed_ms` / `acquired_utc_unix_ms` describe acquisition immediately after reading peak. Latency uses acquisition time, never a delayed sample append time. Missing acquisition time excludes that sample with a warning. Older markers missing arrival fields explicitly warn and fall back to envelope time. Present invalid timing fields (including elapsed arrival/acquisition after append) are rejected for measurement, with raw evidence retained. UTC clocks may jump, so only monotonic timing is compared for consistency. Host-only duration uses monotonic time to avoid wall-clock jumps. An absent terminal event means interruption or incomplete evidence, not a clean stop.

## Checks still required on Windows

Run `node --test test/rdp-audio-diagnostics.test.mjs` or `powershell -NoProfile -File test/rdp-audio-diagnostics.synthetic.ps1` for synthetic tests. Node tests explicitly skip PowerShell execution if unavailable; fixture checks alone do not validate the PowerShell runtime.

On the physical host, verify opt-in startup, real meter levels, fixed markers, device/volume/mute/test-sound operations, unavailable-device recovery, duration/size limits, download, authentication and same-origin rejection, and count-confirmed deletion. In the guest verify supported/missing counters, permitted event-log access, and matching IDs. Reproduce playback and compare observations without attributing a root cause in advance. These real-device checks are not completed by synthetic tests.

Operation events are buffered in a bounded 256-record queue so diagnostics storage cannot hold the audio gate. If storage cannot keep up, capture stops with `queue_overflow`; some operation events were lost and that capture is incomplete evidence. Sample acquisition, marker arrival, and operation occurrence have separate timestamps from JSONL append order.
