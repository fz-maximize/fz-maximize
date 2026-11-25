# Releasing fz-maximize

## What a release contains

`task package:all` builds everything into `dist\`:

| File | What it is |
|---|---|
| `fz-maximize-<ver>-x64-setup.exe` | Inno Setup installer (per-user, no admin). The main artifact for GitHub Releases, winget and the Microsoft Store. |
| `fz-maximize-<ver>-x64-setup.exe.sha256` | Its SHA-256 (winget manifests need it). |
| `fz-maximize-<ver>-x64-portable.zip` | The bare exe + docs, for people who don't want an installer. |
| `fz-maximize-<ver>-arm64-setup.exe` (+ `.sha256`), `-arm64-portable.zip` | The same for Windows on ARM: a native ARM64 build, not x64 under emulation. |

`task package` builds just the x64 files, and `task package:arm64` just the ARM64 ones.
ARM64 needs the Visual Studio "MSVC ARM64 build tools" component; the Release workflow's
runner has it. Without it you can still type-check the ARM64 build with `task lint:arm64`
(CI does that on every PR).

The version comes from `version` in `Cargo.toml`.

## Build prerequisites (build machine only)

* Rust: installed automatically by rustup from `rust-toolchain.toml`.
* Visual Studio Build Tools ("Desktop development with C++"): MSVC linker + Windows SDK.
* Inno Setup 6: `task setup:installer` (per-user install via winget).

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
* Stops a running fz-maximize (`--quit`) before upgrading or uninstalling, and starts it
  again afterwards, including after silent installs, so `winget upgrade` leaves it running.
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

1. Bump `version` in `Cargo.toml` (and `cargo build` so `Cargo.lock` updates) on a
   branch, open a PR and merge it (`main` only accepts PRs).
2. `task ci` locally: checks, tests, release build and installer.
3. `task e2e` on a desktop with FancyZones (live behavior; CI can't run this).
4. `task upstream:check` against the PowerToys version you're shipping for.
5. Tag the merged commit and push the tag:
   `git pull` then `git tag -a v<ver> -m "fz-maximize <ver>"` then `git push origin v<ver>`.
   Release tags are protected: only maintainers can create them, and once pushed they
   can't be moved or deleted. A published version is final, so a fix means a new version.
   The **Release** workflow (`.github/workflows/release.yml`) then:
   * checks that the tag matches `Cargo.toml`;
   * runs `task check` and `task package:all` (x64 + ARM64) on a clean Windows runner;
   * runs the ARM64 exe on a real Windows on ARM runner (native ARM64 binary, right version);
   * (once SignPath is set up) signs the exe and installer;
   * writes `SHA256SUMS.txt` and a build-provenance attestation;
   * publishes the GitHub Release (as `github-actions`) with the installer, portable
     zip and checksums. Tags with a `-` (e.g. `v0.2.0-beta.1`) become pre-releases.
6. Update the winget manifest (and the Store listing, once it exists).

To test the pipeline without publishing: Actions → **Release** → **Run workflow**. It
builds everything and attaches the files to the run as an artifact, and creates no
release.

**CI** (`.github/workflows/ci.yml`) runs fmt, clippy and the unit tests on every push to
`main` and every pull request.
