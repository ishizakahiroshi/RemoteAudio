# Remote Audio

Switch your **Windows 11 host's default audio output device** from any browser on the LAN.
No more walking back to the host to change the output from Nest Hub Max to wired earphones
during a meeting — do it from a Hyper-V guest, WSL2, your phone, or another PC.

> Status: **v0.1.0 released** (2026-07-31) on npm, crates.io and GitHub Releases.
> The v0.2 resident UX is being prepared on `develop`; the Microsoft Store name
> **Remote Audio** is reserved (Product ID `9PC6L3B67FV9`) and its first submission
> is pending. Scoop is intentionally not a supported v0.2 channel.

---

## How it works

```
Browser / phone / other PC
        ↓ HTTP(S)
Remote Audio server (Rust, runs as your logon user on the Windows 11 host)
        ↓ windows crate (COM)
Windows Core Audio (IMMDeviceEnumerator / IPolicyConfig)
        ↓
Physical devices (Nest Hub Max / wired earphones / headphones / …)
```

- The server runs in **your physical console session** (not as a SYSTEM service, and not inside an RDP session — audio endpoints belong to the interactive user's physical session; from an RDP session you only see the virtual "Remote Audio" endpoint).
- Default bind is **`0.0.0.0:17650`** — exposed to the LAN out of the box, because controlling the host from another machine is the whole point. Non-loopback clients require a bearer token; loopback (the host itself) is bypassed. Lock it down to `127.0.0.1` with `RemoteAudio setup` if you don't need remote control.
- Console / Multimedia / Communications default endpoints are always **switched together** (otherwise meeting apps still route to the old device via the Communications default).
- The Web UI controls the **master volume and mute state** of the current default Multimedia output. Volume is per output device, so it follows the selected endpoint after a device switch.
- Switching is done by **device ID**, not display name (display names change on reconnect).

## Supported platforms

| Side | Support |
|---|---|
| Server (host) | **Windows 11 only.** Windows 10 is not officially supported. macOS / Linux are out of scope. |
| Client (browser) | Any modern browser on any OS. The built-in Web UI is served by the host binary. |

## Install

**npm** (primary — no Rust toolchain needed):

```powershell
npm i -g audioremote
# or run it once without installing:
npx audioremote
```

The platform-specific Rust binary ships via `optionalDependencies` (esbuild / Biome style), so
`npm i` pulls the right executable for your machine. Package-manager installs skip Mark of the Web,
so SmartScreen warnings are avoided.

**GitHub Releases** (Node-free): download `audioremote-win32-x64.zip` from the
[latest release](https://github.com/ishizakahiroshi/RemoteAudio/releases/latest), verify it against
the published `SHA256SUMS.txt`, and run the extracted `RemoteAudio.exe`.

The package and crate namespace remains `audioremote` for compatibility; the
native executable distributed by v0.2 and later is `RemoteAudio.exe`.

**winget** (v0.2; available after the first manifest submission):

```powershell
winget install ishizakahiroshi.AudioRemote
```

**Microsoft Store**: **Remote Audio** is reserved and the MSIX identity is now
`ishizakahiroshi.RemoteAudio`. The first submission is still pending. The Store
edition will be installed and updated by Microsoft, carries Microsoft's signature,
and targets Windows 11 (build 22000 or later); it is not a portable download.

**Scoop** is not a supported v0.2 install path. The manifest under
`packaging/scoop/` is retained as a learning/reference artifact; use the Store or
winget for supported package-manager installs.

**crates.io** (builds from source, needs Rust 1.85+):

```powershell
cargo install audioremote
```

npm, GitHub Releases and crates.io have been live since v0.1.0. The winget manifest is prepared
for v0.2, but its first external publication is still pending. Once available, the supported
package-manager path will unpack the same zip attached to the GitHub release and check it against
the same `SHA256SUMS.txt`, so there is no separate build to distrust. The binary is **unsigned**
— see Security posture for the warning and verification details.

## Getting started, end to end

Only the **host** installs anything. The guest opens a URL and that's it — a phone, a Linux box, or
another Windows machine all work the same way.

**On the host, once:**

1. Install it (see above).
2. Run `RemoteAudio.exe`. It stays in the notification area and shows a welcome window.
3. Press **Finish setting up** once. It registers autostart, asks once for the LAN firewall
   permission, and copies a share URL containing a connection token. The same actions are
   available from the tray later; `RemoteAudio share` prints all share URLs explicitly.

**On the guest, once:**

1. Paste that URL into a browser and open it. The token rides in the URL fragment (`#t=…`), which
   browsers never send to the server; the page reads it and keeps it in `localStorage`.
2. Bookmark the page. From then on the bare URL is enough — there is no token to type in.

**Every day:** open the bookmark and press the device you want. Console, Multimedia, and
Communications switch together, so nothing is left routed to the old endpoint.

v0.2 folds host steps 3 and 4 into a single click from the tray, drops the console window
entirely, and keeps the server alive behind the tray icon — see Roadmap.

## Build from source (developer)

```powershell
# On Windows 11 with Rust 1.85 or newer (see `rust-version` in Cargo.toml)
cargo build            # dev build
cargo run              # starts the resident supervisor
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
npm test               # npm launcher (bin/audioremote.js)
```

`cargo build --release` produces `target/release/RemoteAudio.exe`.

## Command line

```text
RemoteAudio                     run the resident supervisor (default)
RemoteAudio serve               run one server directly and open the browser
RemoteAudio serve --no-open     run one server directly without opening a browser
RemoteAudio setup               interactive config wizard (bind / token / sort / port / recovery)
RemoteAudio list                list playback endpoints + current defaults
RemoteAudio set <id>            switch the default output device
RemoteAudio share               print the LAN URLs with the token in full
RemoteAudio token list          list tokens (masked)
RemoteAudio token list --show   list tokens in full
RemoteAudio token add <name>    issue a new named token
RemoteAudio token revoke <name|token>
```

`token add` and `token revoke` take effect on a **running** server within a
second — no restart. Everything else in `config.toml` (bind, port,
`allowed_networks`, `device_sort`, and resident crash recovery) is read once at
startup. Resident mode retries an unexpectedly stopped server with increasing
waits, then stops after repeated failures; run `RemoteAudio setup` to turn that
recovery off.

## Volume and mute

Open the built-in Web UI from the host or a LAN client. The master-volume panel
uses the current default **Multimedia** render endpoint and provides a 0–100%
slider plus mute/unmute. Changes made in Windows are picked up by the Web UI's
three-second refresh, and switching the output device refreshes the panel for
the new endpoint.

The same state is available through the authenticated HTTP API:

```text
GET  /api/volume
POST /api/volume   {"level": 0.5}
POST /api/volume   {"muted": true}
```

`level` is a finite scalar from `0.0` to `1.0`; invalid requests return HTTP
400. Only the fields present in the body are applied, so a mute-only request
cannot clobber a level someone changed in Windows a moment earlier. The
bearer-token, Host-header, allowlist and same-origin checks all apply.

## Start at logon

Register the current executable in the per-user HKCU Run key:

```powershell
.\target\release\RemoteAudio.exe --install-autostart
```

Remove only Remote Audio's own Run value with:

```powershell
.\target\release\RemoteAudio.exe --uninstall-autostart
```

The registered command is the quoted absolute exe path followed by `supervise`,
so signing in puts an icon in the notification area and keeps the server alive
behind it. No console window appears, at logon or otherwise.

Installing also offers to add an inbound firewall rule for the configured port
on private and domain networks, which needs **one** UAC prompt. Declining costs
you the rule, not the logon entry — the command reports what to run later from
an elevated prompt. Uninstalling removes both, and asks for elevation again for
the firewall half.

If the exe is moved, run the install command again.

## Roadmap

- **v0.1** — Host server + HTTP API + built-in Web UI, including device switching,
  master volume/mute, and minimal per-user autostart (released 2026-07-31).
- **v0.2** — Host-side resident UX: tray icon, no console window, full autostart
  (firewall rule, restart and crash recovery), remote restart, plus winget /
  Microsoft Store distribution. **Nothing new to install on the guest.**
- **v0.3+** — Automatic HTTPS provisioning, and a PWA entry point on top of it
  (a PWA needs a secure context, which plain `http://` on a LAN address is not).
  Per-app volume (the Windows volume mixer, from the guest) also sits here: it
  only earns its keep when two apps play at once, which this host does not do.

Guests stay on the browser. Open the share URL once and the token is kept in
`localStorage`, so a bookmark is all you need afterwards. Shipping a native guest
app is a non-goal (see below). The core architecture (host-resident server + HTTP
API) does not change between versions.

## Configuration

Config lives at `%APPDATA%\audioremote\config.toml` (created automatically on first run). See the UX mockup for the concrete layout; the shape is roughly:

```toml
[server]
bind = "0.0.0.0"     # LAN-exposed by default; "127.0.0.1" to lock to this host
port = 17650
allowed_networks = []   # optional allowlist: ["203.0.113.0/24", "198.51.100.5"]; empty = any

[auth]
require_token = true

# One or more named bearer tokens (first run generates a "default").
# Manage with `RemoteAudio token add|revoke|list`.
[[auth.tokens]]
name = "default"
token = "ar_live_..."   # auto-generated on first run
revoked = false

[audio]
device_sort = "state"   # "state" | "name" | "recent"

[tray]
auto_restart = true       # resident mode retries a crashed server; setup can turn it off
```

Notes on hand-editing:

- `bind` accepts an IPv4/IPv6 literal or `localhost`; host names are not
  resolved and `port = 0` is refused. An unusable value is reported at startup
  with the reason instead of failing obscurely.
- `allowed_networks` accepts CIDR (`"203.0.113.0/24"`) or a bare address
  (`"203.0.113.20"`, treated as a single host). Entries that parse as neither
  match nothing — the startup banner names them so a typo does not read as "the
  server ignores my LAN".
- Console / Multimedia / Communications are **always** switched together; there
  is no setting for it (see Non-goals).
- `tray.auto_restart` applies only to the resident supervisor. Direct
  `RemoteAudio serve` runs one server and never starts a replacement child.

Device usage history (for `device_sort = "recent"`) is stored separately in `%APPDATA%\audioremote\history.toml` so editing config by hand does not clobber it.

## Security posture

- **Exposed to the LAN by default** (`bind = "0.0.0.0"`). The Windows Firewall prompt on first run is the outer gate; the bearer token is the inner gate. Lock down with `RemoteAudio setup` (bind `127.0.0.1`) if you don't want remote control.
- Bearer token authentication required for every **non-loopback** client on all API endpoints; loopback (the host itself) is bypassed. Tokens are named and individually revocable — `RemoteAudio token add|revoke|list`.
- **Revocation is immediate.** The running server re-reads the token set when `config.toml` changes (checked at most once a second), so `token revoke` stops a leaked token without a restart. Writes are atomic, so the server never reads a half-saved file.
- **DNS-rebinding guard**: a request is accepted only when its `Host` header matches loopback or a current LAN IP, so a malicious page whose DNS re-resolves to `127.0.0.1` cannot reach the API. Applies to the Web UI assets as well, not just the API.
- Optional **allowlist** (`allowed_networks`) refuses non-loopback source IPs outside the listed networks before token checking.
- **Cross-origin writes are refused.** Because loopback skips the token, any web page could otherwise `fetch()` a device switch at `127.0.0.1` while you browse. State-changing requests must carry `Sec-Fetch-Site: same-origin`/`none` and, when an `Origin` is present, an authority matching the request's `Host`. Non-browser clients (curl, scripts) send neither header and are unaffected. No CORS handler is installed, so cross-origin **reads** stay blocked by the browser.
- **Framing is refused** (`Content-Security-Policy: frame-ancestors 'none'` + `X-Frame-Options: DENY`), so the token-free loopback UI cannot be used for clickjacking. Every response also carries `X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer`.
- **Tokens are masked in console output.** Resident mode does not print share URLs. Direct `serve` masks existing tokens on startup; only a newly generated first-run token is shown there, while the explicit `RemoteAudio share` command prints full URLs on demand. `token list` masks by default (`--show` to reveal). This keeps live credentials out of scrollback, screen shares and redirected logs.
- The resident welcome screen does **not** copy a live share URL silently at launch. It explains that the URL contains a connection token and copies it only after the user presses the setup button or the tray copy action; the completion dialog says what was copied.
- The tray can also create an Internet Shortcut (`.url`) on the Desktop for a selected LAN address. This is an explicit, confirmed action because the file stores the live connection token; treat the shortcut like a password.
- When bound to LAN, the guest UI shows a **"LAN exposed"** badge as a reminder.
- No unsigned exe direct-download flow is recommended for end users; use the npm channel to avoid SmartScreen prompts.

### Plain HTTP, and what that costs

v0.1 speaks **HTTP, not HTTPS**. On a LAN segment you control that is a
considered trade, not an oversight — but be clear about it: the bearer token
travels in a header in the clear, so anyone able to sniff the segment (an
untrusted Wi-Fi AP, an ARP-spoofing device) can capture and replay it until you
revoke it. Accordingly:

- Run it on a **trusted private LAN** only. Choose "Private networks" at the
  Windows Firewall prompt, never "Public".
- Narrow the reachable set with `allowed_networks`, and issue **one token per
  device** so a single leak can be revoked without disturbing the others.
- If you need transport encryption, put a TLS reverse proxy in front and take the
  server off the LAN entirely: `RemoteAudio setup` → bind `127.0.0.1`, then have
  the proxy (Caddy, nginx, IIS ARR) terminate TLS on the same machine and forward
  to `http://127.0.0.1:17650`. Send the **upstream** authority as `Host` —
  `proxy_set_header Host 127.0.0.1:17650;` in nginx, `header_up Host {upstream_hostport}`
  in Caddy — because the rebinding guard only accepts loopback and this host's own
  LAN IPs, not your proxy's hostname. Writes still work: modern browsers send
  `Sec-Fetch-Site: same-origin`, which is checked instead of comparing `Origin` to
  `Host`. A browser old enough to omit that header would need `Origin` rewritten to
  match as well; a first-class "trusted hostname" setting remains deferred and is
  not scheduled for a specific milestone.
- Automatic HTTPS provisioning (mkcert, self-signed helpers) stays out of scope
  see Non-goals.

## Non-goals and deferred work

- Global hotkeys. **Dropped for good** — this product is operated from the guest,
  so a host-side hotkey contradicts the premise.
- A native guest client (Tauri / iOS / Android). **Dropped for good** — guests
  already work with nothing but a browser, so shipping an executable would only
  add an install step and give up phone / Linux / any-device reach.
- Per-role (Console / Multimedia / Communications) individual switching UI.
- Per-app volume control. Deferred to v0.3+ because it only earns its keep when
  two applications are playing at once; v0.2 keeps the master-volume control.
- macOS / Linux server implementations.
- Windows 10 official support.
- Automatic HTTPS provisioning, mkcert integration, and self-signed helpers.
- Code signing for direct-download executables. The Microsoft Store re-signs its
  package; direct downloads remain unsigned and are accompanied by SHA256 checksums.

## Project layout

```
audioremote/
├── src/                        Rust sources (binary crate)
├── web/                        embedded Web UI (vanilla JS, no build step)
├── bin/audioremote.js          npm launcher (resolves + runs the native binary)
├── npm/platforms/              per-platform npm packages carrying the .exe
├── test/                       node:test suite for the npm launcher
├── Cargo.toml                  package definition
├── CLAUDE.md / AGENTS.md       AI entry points
├── LICENSE                     MIT
├── scripts/                    secrets-scan + hook installer
├── .githooks/                  layer 2 pre-commit
├── .github/workflows/          CI (validate.yml) + release + secrets-scan
└── docs/
    └── local/                  plan / recap / bugfix / mockup (gitignored — local-only)
```

## License

MIT — see [LICENSE](LICENSE). Copyright (c) 2026 Hiroshi Ishizaka (ishizakahiroshi).
