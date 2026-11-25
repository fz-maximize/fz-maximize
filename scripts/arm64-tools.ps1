<#
.SYNOPSIS
  Check for (or install) the Visual Studio component that builds ARM64 binaries.

.DESCRIPTION
  Building fz-maximize for ARM64 needs the "MSVC ARM64 build tools" component of Visual
  Studio / the Build Tools (Microsoft.VisualStudio.Component.VC.Tools.ARM64).

  Without -Install: exit 0 if it's installed, 1 if not (used as a Task precondition).
  With -Install: add it to the newest Visual Studio installation (asks for admin rights).
#>
param([switch]$Install)

$component = "Microsoft.VisualStudio.Component.VC.Tools.ARM64"
$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) {
    Write-Host "Visual Studio (or the Build Tools) isn't installed: see the README's prerequisites."
    exit 1
}

$found = & $vswhere -latest -products * -requires $component -property installationPath
if ($found) {
    if ($Install) { Write-Host "ARM64 build tools are already installed in $found" }
    exit 0
}
if (-not $Install) { exit 1 }

$path = & $vswhere -latest -products * -property installationPath
if (-not $path) {
    Write-Host "No Visual Studio installation found to add the ARM64 build tools to."
    exit 1
}
$setup = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\setup.exe"
Write-Host "Adding the ARM64 build tools to $path (Windows will ask for admin rights)..."
$p = Start-Process -FilePath $setup -Verb RunAs -Wait -PassThru -ArgumentList @(
    "modify", "--installPath", "`"$path`"", "--add", $component, "--passive", "--norestart"
)
if ($p.ExitCode -ne 0) {
    Write-Host "The Visual Studio installer exited with code $($p.ExitCode)."
    exit $p.ExitCode
}
Write-Host "Done. 'task package' now builds ARM64 too."
