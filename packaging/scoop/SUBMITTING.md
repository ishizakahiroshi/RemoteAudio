# Scoop reference (not a v0.2 release task)

AudioRemote is **not publishing a Scoop bucket in v0.2**. The supported
distribution paths are Microsoft Store and winget. This folder is retained as
a learning/reference artifact for Scoop manifests and autoupdate workflows;
do not create `ishizakahiroshi/scoop-bucket` as part of the current release.

The instructions below are intentionally kept for a future revisit or a
standalone learning exercise.

Scoop has no central review queue. You publish a **bucket** — a git repository
holding manifests — and users add it by URL. So this is entirely under your own
control, and there is nothing to wait for.

## What is in this folder

| File | Holds |
|---|---|
| `audioremote.json` | the manifest itself: version, download URL, hash, and the update rules |
| `excavator.yml` | the workflow the **bucket** repository runs to keep that manifest current |

`audioremote.json` is the master copy. It is copied into the bucket repository;
the original stays here so the package can be rebuilt from the repository that
produces it.

## If Scoop is revisited later: creating the bucket

1. Create a public repository named `scoop-bucket` on the account that owns
   this project. The name is a convention, not a requirement, but every bucket
   uses it and users expect it.
2. Lay it out like this:

   ```
   bucket/audioremote.json          <- copy of packaging/scoop/audioremote.json
   .github/workflows/excavator.yml  <- copy of packaging/scoop/excavator.yml
   README.md
   ```

   The `bucket/` directory name is required — Scoop looks there.
3. In the bucket's README, tell people how to add it:

   ```powershell
   scoop bucket add ishizakahiroshi https://github.com/ishizakahiroshi/scoop-bucket
   scoop install ishizakahiroshi/audioremote
   ```

## How updates reach users

Nothing in this repository pushes to the bucket. Instead the bucket keeps
itself current: `excavator.yml` runs on a schedule, notices that the newest
GitHub release is ahead of the manifest, reads the checksum out of that
release's own `SHA256SUMS.txt`, and commits the result.

That means **Scoop lags a release by up to the excavator's interval** (a few
hours as configured). If you want it immediate, run the workflow by hand from
the bucket's Actions tab right after a release — it is `workflow_dispatch`-able
for exactly that reason.

The alternative would be for this repository to push into the bucket at release
time, which needs a token with write access to another repository. The lag is
cheaper than the credential.

## Verifying a change before you commit it

Both of these run against the files in this folder, no bucket required:

```powershell
$checkver = "$env:USERPROFILE\scoop\apps\scoop\current\bin\checkver.ps1"

# does checkver find the newest release?
& $checkver -App audioremote -Dir packaging\scoop

# does the manifest install, and does the shim work?
scoop install .\packaging\scoop\audioremote.json
audioremote --help
scoop uninstall audioremote
```

To rehearse the autoupdate path, copy the manifest somewhere, edit its
`version` to something older, and run `checkver` with `-Update` on the copy. It
should raise the version and fill in the hash it read from `SHA256SUMS.txt`.
Do this on a copy — with `-Update` it rewrites the file it is given.

## Things that will bite

- **`hash.url` points at `SHA256SUMS.txt` on purpose.** Autoupdate reads the
  checksum the build produced instead of computing its own. A bucket that
  pastes a hash by hand is a bucket claiming a digest nobody verified.
- **`bin` must match the file inside the zip.** The release archive holds
  `RemoteAudio.exe` at its root, with no directory around it.
- **The exe is unsigned.** Scoop does not care, but SmartScreen still warns on
  first run.
