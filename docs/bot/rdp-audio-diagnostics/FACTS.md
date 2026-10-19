# Facts and source evidence

- User-reported symptom (2026-10-19 conversation): while browser playback runs in the RDP guest, pressing Stop still leaves audio audible for about two seconds. This is a report, not a measured result or a root-cause finding.
- Repo: `ishizakahiroshi/RemoteAudio`, public. Working branch base is `develop` at `1f74eaa2d17dd521e2472a3842267bb1d87fbccf`.
- Current app is a Windows host service with an authenticated HTTP API and embedded browser UI. `CLAUDE.md` identifies `src/server.rs` as the HTTP layer and requires Core Audio calls to pass through `AudioGate`.
- `src/audio/mod.rs` currently exposes device enumeration/switch, master volume and mute, and a short test sound. It does not record audio content or include a diagnostic capture API.
- Existing `/api` routes are wrapped by the same-origin and bearer-token middleware in `src/server.rs`; new diagnostics routes must remain inside that boundary.
- Existing frontend translations are in `web/lang/en.json` and `web/lang/ja.json`; web UI is implemented in `web/app.js` and `web/style.css`.
- User wants an implementation they can run on the physical host. No host playback result is available yet. Do not claim a cause or mark host acceptance complete.
- Private/local observation notes and machine-specific event data are intentionally not copied into this public task. Use only synthetic fixtures in tests.

## Data minimization

Capture is opt-in. Do not persist audio, request headers, tokens, IP addresses, host names, usernames, event message bodies, arbitrary marker text, friendly device names, or raw Core Audio device IDs. Use a capture-local endpoint slot where an endpoint transition needs correlation. Keep only bounded diagnostics event data needed for timeline correlation.
