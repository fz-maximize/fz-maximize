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
; We stop the running instance ourselves, wherever it runs from (see [Code]); the
; Restart Manager only sees copies inside the install folder.
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
// fz-maximize holds this mutex while it runs (one instance per sign-in session), wherever
// it was started from: this install, another folder, the portable zip or a dev build.
const
  InstanceMutex = 'Local\fz-maximize.single-instance';

function IsRunning(): Boolean;
begin
  Result := CheckForMutexes(InstanceMutex);
end;

// Ask the running copy to exit (`--quit` posts WM_CLOSE to its window) and wait for it
// to go. `Exe` is any fz-maximize.exe; it finds the running copy by its window class.
function StopRunningInstance(const Exe: String): Boolean;
var
  ResultCode, Waited: Integer;
begin
  if FileExists(Exe) then
    Exec(Exe, '--quit', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Waited := 0;
  while IsRunning() and (Waited < 10000) do
  begin
    Sleep(100);
    Waited := Waited + 100;
  end;
  Result := not IsRunning();
end;

// Use the exe being installed: there may be no installed copy yet (first install while
// a portable or dev copy runs).
function StopWithNewExe(): Boolean;
begin
  ExtractTemporaryFile('{#AppExe}');
  Result := StopRunningInstance(ExpandConstant('{tmp}\{#AppExe}'));
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if not IsRunning() then
    exit;
  // Silent installs (winget, upgrades) close it without asking; it is started again
  // when Setup finishes.
  if not WizardSilent() then
    if MsgBox('fz-maximize is running. Setup will close it now and start the new ' +
              'version when it finishes.' + #13#10#13#10 +
              'Click OK to continue, or Cancel to exit Setup.',
              mbConfirmation, MB_OKCANCEL) <> IDOK then
    begin
      Result := 'Setup was cancelled because fz-maximize is running.';
      exit;
    end;
  if not StopWithNewExe() then
    Result := 'fz-maximize could not be closed. Exit it from its tray icon, then run Setup again.';
end;

function InitializeUninstall(): Boolean;
begin
  Result := True;
  if IsRunning() then
    StopRunningInstance(ExpandConstant('{app}\{#AppExe}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  // The app's own "Start with Windows" toggle may have added this even if the
  // installer task was unticked; always clean it up.
  if CurUninstallStep = usPostUninstall then
    RegDeleteValue(HKEY_CURRENT_USER, 'Software\Microsoft\Windows\CurrentVersion\Run', '{#AppName}');
end;
