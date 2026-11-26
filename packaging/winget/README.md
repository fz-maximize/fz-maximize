# winget manifests

Copies of the manifests submitted to [microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs),
in the same folder layout (`manifests/f/fz-maximize/fz-maximize/<version>/`).

For a new release:

1. Copy the previous version's folder to the new version number.
2. Update `PackageVersion`, the `InstallerUrl`s, the `InstallerSha256`s (from the release's
   `SHA256SUMS.txt`), `ReleaseDate` and `ReleaseNotesUrl`.
3. `winget validate --manifest <folder>`
4. Optionally test it: `winget settings --enable LocalManifestFiles` (admin, once), then
   `winget install --manifest <folder>`. A local manifest always triggers SmartScreen
   for unsigned installers; installs from the winget source don't.
5. Submit the folder to winget-pkgs from the fzmaximize account (PR title:
   `New version: fz-maximize.fz-maximize version <version>`).
