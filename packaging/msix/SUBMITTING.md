# Getting Remote Audio into the Microsoft Store

Only the **first** submission is done by hand. The Store submission API — and
therefore `.github/workflows/msstore-publish.yml` — requires that the app is
already published and live, so it cannot create the thing it needs.

MSIX is the route rather than EXE/MSI for one reason: Microsoft re-signs the
package after certification, so no code-signing certificate is needed. The
portable build stays unsigned and SmartScreen still warns about it; the Store
copy does not.

## What is in this folder

| File | Holds |
|---|---|
| `AppxManifest.xml` | package identity, logos, the startup task and the firewall rule. `{{VERSION}}` is substituted at build time |
| `build-msix.ps1` | stages the release exe + logos, packs with `MakeAppx`, optionally self-signs and sideloads |
| `run-wack.ps1` | Windows App Certification Kit, from an **elevated** prompt |
| `gen-logos.ps1` | re-bakes `assets/msix/*.png` from `assets/icon.svg`. Only needed when the icon changes |

The logos themselves live in `assets/msix/` and are committed, so a build needs
neither Chrome nor a network.

## Before the first build: the identity

Three values in `AppxManifest.xml` have to match Partner Center **exactly**,
including case and spacing:

| Manifest | Where it comes from |
|---|---|
| `Identity/Name` | Partner Center → the app → Product management → Product identity |
| `Identity/Publisher` (`CN=<GUID>`) | same page — **account-wide**, so it is already known from the first app published under this account |
| `Properties/PublisherDisplayName` | same page — also account-wide (`ishizakahiroshi`) |

Only `Identity/Name` is per-app, and it is fixed when the name is reserved. None
of these are secrets: they ship inside every public package. A mismatch is caught
loudly at upload, so a wrong guess cannot reach the Store — but it does cost a
round trip, and `build-msix.ps1` prints all three for exactly that comparison.

The current `Remote Audio` reservation is:

- `Identity/Name`: `ishizakahiroshi.RemoteAudio`
- `Identity/Publisher`: `CN=A454C7F3-0506-42C1-AB41-2BE056B76ABF`
- `PublisherDisplayName`: `ishizakahiroshi`
- Store ID: `9PC6L3B67FV9`
- PFN: `ishizakahiroshi.RemoteAudio_n1ntyf38k1qmm`

The first submission is still pending. Once published, the public Store link will
be <https://apps.microsoft.com/detail/9PC6L3B67FV9>.

Reserve the name at <https://partner.microsoft.com> → Apps and games → New
product → **MSIX/PWA** (not EXE/MSI). A reserved name is released again if it
goes unused for three months.

## Building and testing locally

```powershell
cargo build --release
pwsh -NoProfile -File packaging\msix\build-msix.ps1 -Install
```

`-Install` self-signs with a certificate whose subject matches `Identity/
Publisher` and sideloads the result. The first time, Windows will refuse it
because that certificate is not trusted yet. Trust it once, from an **elevated**
prompt:

```powershell
Import-Certificate -FilePath dist\msix\audioremote-local-test.cer `
  -CertStoreLocation Cert:\LocalMachine\TrustedPeople
```

This signature is for local testing only. The package uploaded to Partner Center
does not need it.

Then check, on a machine with real audio devices:

- launching from the Start menu shows **no console window** and puts an icon in
  the notification area
- Task Manager → Startup apps lists **Remote Audio**, and it can be switched off
- another machine on the LAN can open the share URL and switch the output device
- `%APPDATA%\audioremote\config.toml` is written where the portable build writes
  it, not into a package-private copy

Finally, WACK, from an elevated prompt:

```powershell
pwsh -NoProfile -File packaging\msix\run-wack.ps1
```

A `FAIL` marked `OPTIONAL=TRUE` is acceptable — the Rust standard library
references `CreateProcessW` and friends, and the same result passed
certification first time on the previous app from this account. A FAIL marked
`OPTIONAL=FALSE` is not.

## The first submission

Partner Center, six sections:

1. **Pricing and availability** — all markets, public, discoverable, release as
   soon as possible, free
2. **Properties** — category, **privacy policy URL (required, see below)**,
   support contact `ishizakahiroshi.dev@gmail.com`
3. **Age ratings** — App Type "All Other App Types", then No throughout
4. **Packages** — upload the `.msix`. The section stays *Incomplete* until
   Device family availability is filled in, even when the package itself says
   Validated
5. **Store listings** — `en-US` and `ja-JP`, because those are the two languages
   `<Resources>` declares
6. **Submission options** — the `runFullTrust` justification

Then **Supplemental info → Additional Testing Information → Description** for
the certification notes. This app is a LAN server, so the tester needs to be
told how to see it working: start it, open the tray icon, copy the share URL,
open that URL from a second machine.

### Four things that waste an afternoon

- **"Do you collect personal information?" cannot be saved as No.** Partner
  Center overwrites it from the declared capabilities, and `runFullTrust` is not
  optional for a Win32 MSIX. A published privacy policy URL is therefore
  mandatory. Fighting this is time spent losing.
- **The `runFullTrust` justification silently truncates at about 500
  characters.** No counter, no error. Write it short, save, and read it back.
- **Certification notes are not on the Submission options page.** They are under
  Supplemental info → Additional Testing Information.
- **Save immediately after changing a value.** Navigating away first discards
  it, which is very hard to tell apart from the overwrite above.

Before submitting, look at every publicly visible contact field and confirm the
`.dev` address is there rather than the private one.

## After it is published

`.github/workflows/msstore-publish.yml` can take over, but it needs four
repository secrets, and getting them requires linking an Entra ID tenant in
Partner Center and granting the registered application the **Manager** role:

`AZURE_AD_TENANT_ID`, `AZURE_AD_APPLICATION_CLIENT_ID`,
`AZURE_AD_APPLICATION_SECRET`, `SELLER_ID`.

WACK stays out of that workflow — it needs elevation — so keep running it here
before each submission.

## Things that will bite

- **The firewall rule is pinned to port 17650.** A manifest cannot read
  `config.toml`, so `windows.firewallRules` names one port. Anyone who moves the
  server with `RemoteAudio setup` needs an inbound rule of their own;
  `RemoteAudio.exe --install-autostart` detects this and prints the command.
- **`Executable` must not appear on the firewall `Extension` element.** It makes
  `EntryPoint` mandatory and `MakeAppx` fails with `80080204`. The executable is
  named on the inner `FirewallRules` element.
- **Store and portable share `%APPDATA%\audioremote\config.toml`,** tokens
  included. Uninstalling the Store version leaves it behind.
- **The version is four segments with a trailing zero** (`0.2.0` → `0.2.0.0`),
  derived from `Cargo.toml`. Partner Center rejects a version already submitted,
  so bumping `Cargo.toml` is part of every resubmission.
- **`Square310x310Logo` is deliberately absent.** Declaring it makes
  `Wide310x150Logo` mandatory (`MakeAppx` `80080204` again) for a tile size
  Windows 11 does not use.
