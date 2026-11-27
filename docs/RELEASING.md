# Releasing fz-maximize

## What a release contains

`task package` builds everything a release ships (both architectures) into `dist\`:

| File | What it is |
|---|---|
| `fz-maximize-<ver>-x64-setup.exe` | Inno Setup installer (per-user, no admin). The main artifact for GitHub Releases, winget and the Microsoft Store. |
| `fz-maximize-<ver>-x64-setup.exe.sha256` | Its SHA-256 (winget manifests need it). |
| `fz-maximize-<ver>-x64-portable.zip` | The bare exe + docs, for people who don't want an installer. |
| `fz-maximize-<ver>-arm64-setup.exe` (+ `.sha256`), `-arm64-portable.zip` | The same for Windows on ARM: a native ARM64 build, not x64 under emulation. |

`task package:x64` or `task package:arm64` builds just one architecture. The individual
steps take an `ARCH` parameter, e.g. `task package:installer ARCH=arm64`.

ARM64 needs Visual Studio's "MSVC ARM64 build tools" component: `task setup:arm64:tools`
adds it (once, asks for admin rights). Without it, `task package` stops after x64 with that
hint; `task lint:arm64` still type-checks the ARM64 build (CI does that on every PR). The
Release workflow's runner has the tools.

The version comes from `version` in `Cargo.toml`.

## Build prerequisites (build machine only)

* Rust: installed automatically by rustup from `rust-toolchain.toml`.
* Visual Studio Build Tools ("Desktop development with C++"): MSVC linker + Windows SDK.
* Inno Setup 6: `task setup:installer` (per-user install via winget).
* For ARM64: `task setup:arm64:tools` (the Visual Studio "MSVC ARM64 build tools").


None of these are needed on users' machines. The exe uses a **hybrid CRT**
(`.cargo/config.toml`), the same approach PowerToys uses: the Visual C++ runtime is linked in
statically, and the Universal CRT comes from Windows. Its only dependencies are DLLs that
ship with Windows 10/11, so there's no Visual C++ Redistributable to install.

## What the installer does

`installer/fz-maximize.iss`:

* Installs per-user to `%LOCALAPPDATA%\Programs\fz-maximize` (no UAC prompt);
  `/ALLUSERS` installs machine-wide instead.
* Start Menu shortcut, an Apps & features entry (with uninstaller) and an optional
  "Start when I sign in" task (the same `HKCU\...\Run` value the app's own toggle uses).
* Stops a running fz-maximize (`--quit`) before installing or uninstalling, wherever it
  runs from (this install, another folder, the portable zip, a dev build), and starts the
  new one afterwards, including after silent installs, so `winget upgrade` leaves it
  running. Interactive setup asks first; silent setup (winget) closes it without asking.
* Uninstall removes the program, shortcut and autostart value. It keeps the user's
  settings (`%APPDATA%\fz-maximize`) and logs (`%LOCALAPPDATA%\fz-maximize`), as is
  conventional.
* `AppId` `{23A7EC64-695C-4A8E-A06E-FEBBD469AA50}` is the product's permanent identity:
  **never change it**, or upgrades and uninstalls stop recognizing existing installs.

Silent install (what winget runs): `fz-maximize-<ver>-x64-setup.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART`.
Add `/TASKS=autostart` to enable start-with-Windows, or `/TASKS=""` to disable it.

## Code signing

Unsigned installers work, but Windows SmartScreen shows "Windows protected your PC" for
downloaded copies, some corporate environments block them, and the Microsoft Store's
desktop-app (EXE/MSI) route **requires** an installer signed with a CA-trusted certificate.
The Store only signs MSIX packages for you, not EXE/MSI installers.

Options (prices as of late 2026; check the providers before buying):

| Option | Cost | Who can use it | Notes |
|---|---|---|---|
| **Azure Artifact Signing** (formerly Trusted Signing) | ~$9.99/month Basic (5,000 signatures), ~$120/year | Individuals in the **US or Canada**; organizations in more regions | Microsoft's recommended service. Needs a pay-as-you-go Azure subscription; identity validation required. No hardware token; signs from CI with `signtool` + the Artifact Signing dlib. |
| **OV code-signing certificate** from a CA (Sectigo, SSL.com, Certum, DigiCert...) | ~$129–$320/year (SSL.com is among the cheapest at list price; resellers discount multi-year terms) | Anyone | Private key must live on a hardware token or a cloud HSM (extra cost, often ~$100–$380, or a cloud-signing subscription). Builds SmartScreen reputation over time. |
| **Certum Open Source** certificate | from ~€49 (cloud) | Individuals, for non-commercial open-source projects | Issued in your name. Good fit if the repo is public and free. |
| **SignPath Foundation** | Free | Open-source projects (OSI license, public repo, automated builds, MFA for the team) | The certificate is issued to SignPath Foundation, so it shows as the publisher. Releases must credit SignPath. |
| **Unsigned** | Free | — | Fine for personal use and testing. SmartScreen warnings, and no Store EXE/MSI listing. |

For a free personal project: **Artifact Signing** if you're in the US or Canada (cheapest
paid option, CI-friendly, Microsoft-backed). **SignPath Foundation** is the free route if
the repo becomes public open source.

### Wiring signing into the build

`scripts/build-installer.ps1` signs when `FZM_SIGN_COMMAND` (or `-SignCommand`) is set.
Inno Setup then signs the exe, the installer and the uninstaller. The command uses `$f`
for the file, e.g. with Artifact Signing:

```powershell
$env:FZM_SIGN_COMMAND = 'signtool sign /v /fd SHA256 /tr http://timestamp.acs.microsoft.com /td SHA256 /dlib "C:\path\Azure.CodeSigning.Dlib.dll" /dmdf "C:\path\metadata.json" $f'
task package
```

## Distribution channels

1. **GitHub Releases:** attach the installer, its `.sha256` and the portable zip to a
   tagged release (`v<ver>`). The installer URL must never change after publishing,
   because winget and the Store point at it.
2. **winget:** submit a manifest to `microsoft/winget-pkgs` (`InstallerType: inno`,
   `Scope: user`, the GitHub URL and SHA-256). `wingetcreate new <installer-url>` generates
   it, and `wingetcreate update` makes the PR for each later release.
3. **Microsoft Store:** register as a developer in Partner Center (free for individuals),
   create a desktop-app (EXE) listing, and give it the **signed** installer URL plus the
   silent switches above. The Store installs it, but updates come from winget or a
   future in-app update check, not the Store.

## Release checklist

1. `task ci` locally: checks, tests, release build and installer.
2. `task e2e` on a desktop with FancyZones (live behavior; CI can't run this).
3. `task upstream:check` against the PowerToys version you're shipping for.
4. Bump `version` in `Cargo.toml` (and `cargo build` so `Cargo.lock` updates) on a
   branch, open a PR and merge it (`main` only accepts PRs). Merging the bump is the
   release: **Tag release** (`.github/workflows/tag-release.yml`) sees a version on `main`
   without a tag and creates `v<ver>` ("fz-maximize <ver>"). Release tags are protected:
   only maintainers and the release automation can create them, and once pushed they
   can't be moved or deleted. A published version is final, so a fix means a new version.
5. The **Release** workflow (`.github/workflows/release.yml`) then:
   * checks that the tag matches `Cargo.toml`;
   * runs `task check` and `task package` (x64 + ARM64) on a clean Windows runner;
   * runs the ARM64 exe on a real Windows on ARM runner (native ARM64 binary, right version);
   * (once SignPath is set up) signs the exe and installer;
   * writes `SHA256SUMS.txt` and a build-provenance attestation;
   * publishes the GitHub Release (as `github-actions`) with the installer, portable
     zip and checksums. Tags with a `-` (e.g. `v0.2.0-beta.1`) become pre-releases.
6. Update the winget manifest (and the Store listing, once it exists).

## Security releases (automatic, approved by a person)

`.github/workflows/security-release.yml` runs on every push to `main` and daily. When the
latest release ships a crate with a known vulnerability (from [OSV](https://osv.dev):
RustSec and GitHub advisories; informational notices such as "unmaintained" don't count)
and `main` no longer does, usually because a Dependabot security update merged, it opens
a **Release X.Y.Z (security)** PR. The PR bumps the patch version, lists the advisories,
and requests a review from the release approver (repository variable `RELEASE_REVIEWER`).

* **Approve the PR to release.** It merges itself once CI passes, Tag release tags it, and
  Release builds and publishes it.
* **Close it to skip that release.** A closed release PR isn't proposed again; the next
  security fix proposes the following version.
* While it's open, it's rebuilt on top of `main` whenever `main` moves, so it stays
  mergeable. Website and Actions updates never trigger a release; neither do routine
  (non-security) crate updates, which ship with the next manual release.

Both workflows act as the **fz-maximize-automerge** GitHub App (installed on this repo
only; Contents and Pull requests write), so the PR, the merge and the tag are the app's.
Its credentials are secrets of the `release-automation` environment, which only `main`
can use. The same app approves and merges low-risk Dependabot PRs
(`.github/workflows/dependabot-automerge.yml`, with its own copy of the credentials as
Dependabot secrets).

To check the PR path end to end: Actions → **Security release** → **Run workflow** with
**test** ticked. It pretends the latest release is vulnerable and opens a draft
`[Test]` PR from `release/test` with no review request or auto-merge. Close it and
delete the branch.

To test the pipeline without publishing: Actions → **Release** → **Run workflow**. It
builds everything and attaches the files to the run as an artifact, and creates no
release.

**CI** (`.github/workflows/ci.yml`) runs fmt, clippy and the unit tests on every push to
`main` and every pull request.
