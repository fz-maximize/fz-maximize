<#
.SYNOPSIS
  Build the fz-maximize installer (Inno Setup) into dist\.

.PARAMETER Version
  Product version (from Cargo.toml).

.PARAMETER Arch
  x64 or arm64.

.PARAMETER ExePath
  Path to the release fz-maximize.exe to package.

.PARAMETER SignCommand
  Optional signing command line for Inno Setup's SignTool, with $f where the file goes,
  e.g. 'signtool sign /fd SHA256 /tr http://timestamp.acs.microsoft.com /td SHA256 /dlib <Azure.CodeSigning.Dlib.dll> /dmdf <metadata.json> $f'.
  The FZM_SIGN_COMMAND environment variable is used when this is empty. When neither is
  set, the installer is built unsigned.
#>
param(
    [Parameter(Mandatory)][string]$Version,
    [ValidateSet("x64", "arm64")][string]$Arch = "x64",
    [Parameter(Mandatory)][string]$ExePath,
    [string]$SignCommand = $env:FZM_SIGN_COMMAND
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

$iscc = @(
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $iscc) {
    $cmd = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($cmd) { $iscc = $cmd.Source }
}
if (-not $iscc) {
    Write-Host "Inno Setup not found. Install it with: task setup:installer" -ForegroundColor Red
    exit 2
}

$exe = Resolve-Path (Join-Path $root $ExePath)
$args = @(
    "/Qp",
    "/DAppVersion=$Version",
    "/DArch=$Arch",
    "/DExePath=$exe"
)
if ($SignCommand) {
    $args += "/DSign"
    $args += "/Sfzmsign=$SignCommand"
    Write-Host "Signing enabled."
} else {
    Write-Host "Building UNSIGNED installer (set FZM_SIGN_COMMAND to sign; see docs/RELEASING.md)." -ForegroundColor Yellow
}
$args += (Join-Path $root "installer\fz-maximize.iss")

& $iscc @args
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$out = Join-Path $root "dist\fz-maximize-$Version-$Arch-setup.exe"
$hash = (Get-FileHash -Algorithm SHA256 $out).Hash
"$hash  $(Split-Path -Leaf $out)" | Set-Content -Encoding ascii "$out.sha256"
Write-Host "Built $out"
Write-Host "SHA256 $hash"
