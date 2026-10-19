# Independent review instructions

Review the actual implementation diff from the initial task-instruction commit to the implementation SHA. Do not use a different base or infer behavior from the task text alone.

Prioritize these failure paths:

1. Diagnostics remain off after process start/restart; active sessions are explicit and bounded.
2. Capture IDs, file names, download paths, and purge paths cannot escape the diagnostics directory. Purge deletes only app-owned capture files; tests use isolated temporary directories.
3. JSONL records are parseable when a write is interrupted; timestamps and sequence order are reliable; limit reached / meter unavailable / process interruption can be distinguished.
4. A marker arrival time is not described as the exact YouTube click time. Host samples are not described as process-specific or as proof of audible output.
5. Tokens, headers, IPs, device IDs/names, user names and raw RDP event payloads cannot enter the durable log or download response.
6. New API routes remain protected by bearer auth and same-origin checks; state changes cannot be triggered cross-origin. Core Audio meter sampling and app operations remain serialized through `AudioGate`.
7. Default endpoint changes are represented by anonymous per-capture slots; meter errors do not flood the log or block audio operations.
8. Guest collector tolerates missing log channels/counters, avoids event `Message`/arbitrary `EventData`, and does not emit client addresses or session identity.
9. Analyzer handles truncated/malformed final JSONL lines and missing guest data without altering source files or silently dropping records.
10. Tests cover JSONL schema/order, bounded capture, interrupted session, path-safe purge, analyzer matching, and synthetic data only.

Report each finding with ID, severity, confirmed (reproduced) or suspected, file/line, synthetic repro (input, command, expected/actual), and minimal fix. Also state reviewed scope/counts, commands and exit codes, review SHA, and what could not be tested. Re-review any corrected SHA; an earlier pass does not carry forward.
