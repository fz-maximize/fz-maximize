; Inno Setup script for fz-maximize. Build with `task package:installer`, which passes:
;   /DAppVersion=0.1.0   (from Cargo.toml)
;   /DArch=x64|arm64
;   /DExePath=<path to the release fz-maximize.exe>
;   /DSign               (optional) sign the exe, installer and uninstaller with the
;                        "fzmsign" tool passed as /Sfzmsign=<command>; see docs/RELEASING.md
;
; Per-user install by default (no admin prompt): %LOCALAPPDATA%\Programs\fz-maximize.
; `/ALLUSERS` on the command line installs for all users instead (needs admin).

#ifndef AppVersion
  #error AppVersion must be defined (/DAppVersion=x.y.z)
#endif
#ifndef Arch
  #define Arch "x64"
#endif
#ifndef ExePath
  #error ExePath must be defined (/DExePath=...)
#endif
#ifndef Publisher
  #define Publisher "fz-maximize"
#endif

#define AppName "fz-maximize"
#define AppExe "fz-maximize.exe"

[Setup]
; Permanent identity of the product: NEVER change it, or upgrades/uninstall break.
AppId={{23A7EC64-695C-4A8E-A06E-FEBBD469AA50}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#Publisher}
AppComments=Maximize windows into their PowerToys FancyZones zone
VersionInfoVersion={#AppVersion}.0
VersionInfoDescription={#AppName} setup
DefaultDirName={autopf}\{#AppName}
DisableDirPage=auto
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=commandline
#if Arch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif
MinVersion=10.0.17763
OutputDir=..\dist
OutputBaseFilename={#AppName}-{#AppVersion}-{#Arch}-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
; We stop the running instance ourselves (see [Code]); no Restart Manager prompts.
CloseApplications=no
#ifdef Sign
SignTool=fzmsign
SignedUninstaller=yes
#endif

[Tasks]
Name: autostart; Description: "Start {#AppName} when I sign in"

[Files]
#ifdef Sign
Source: "{#ExePath}"; DestDir: "{app}"; DestName: "{#AppExe}"; Flags: ignoreversion signonce
#else
Source: "{#ExePath}"; DestDir: "{app}"; DestName: "{#AppExe}"; Flags: ignoreversion
#endif
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
; MIT requires the copyright notices to ship with copies of the software.
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "..\THIRD_PARTY_NOTICES.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"; Comment: "Maximize windows into their FancyZones zone"

[Registry]
; Same value the app's own "Start with Windows" toggle writes (HKA = HKCU for per-user).
Root: HKA; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExe}"""; Flags: uninsdeletevalue; Tasks: autostart

[Run]
; Runs after interactive installs (checkbox) and after silent ones (winget upgrades),
; so an upgrade leaves the app running again.
Filename: "{app}\{#AppExe}"; Description: "Start {#AppName} now"; Flags: nowait postinstall

[Code]
// Ask a running fz-maximize to exit (it posts WM_CLOSE to its own window) so the exe
// can be replaced or removed. The running copy may live elsewhere (a dev build), so
// the installed exe's --quit is tried first, then the one being installed.
procedure StopRunningInstance(const Exe: String);
var
  ResultCode: Integer;
begin
  if FileExists(Exe) then
  begin
    Exec(Exe, '--quit', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    // Exit code 0 means an instance was found and asked to exit.
    if ResultCode = 0 then
      Sleep(1500);
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopRunningInstance(ExpandConstant('{app}\{#AppExe}'));
  Result := '';
end;

function InitializeUninstall(): Boolean;
begin
  StopRunningInstance(ExpandConstant('{app}\{#AppExe}'));
  Result := True;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  // The app's own "Start with Windows" toggle may have added this even if the
  // installer task was unticked; always clean it up.
  if CurUninstallStep = usPostUninstall then
    RegDeleteValue(HKEY_CURRENT_USER, 'Software\Microsoft\Windows\CurrentVersion\Run', '{#AppName}');
end;
