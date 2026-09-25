#define AppName "VocalCode"
#define AppExe "VocalCode.exe"
#define AppVersion GetStringFileInfo("..\..\dist-community\windows\" + AppExe, "FileVersion")
; VocalCode installs with exactly the identity the paid releases (up to 1.2.1)
; installed with, so a paid installation's own in-app updater can run this
; installer and replace that app in place:
; - Their AppName was "VocalCode" and they set no AppId, and Inno Setup uses
;   AppName when AppId is absent. The same AppId gives the same uninstall key,
;   HKCU\...\Uninstall\VocalCode_is1, and the same uninstall log in {app}.
; - {autopf} with PrivilegesRequired=lowest is %LOCALAPPDATA%\Programs, so a
;   fresh install lands where theirs did; an upgrade reuses the recorded folder
;   (UsePreviousAppDir).
; - The executable is VocalCode.exe and owns Local\VocalCode.Desktop, which is
;   what their updater relaunches and what this AppMutex waits for.
; - Their updater accepts only an installer whose version resource says
;   ProductName "VocalCode" and OriginalFilename "VocalCodeSetup.exe".
[Setup]
AppId=VocalCode
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Daming Wu
AppPublisherURL=https://github.com/wudaming00/vocalcode-community
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
VersionInfoOriginalFileName=VocalCodeSetup.exe
DefaultDirName={autopf}\VocalCode
DefaultGroupName=VocalCode
UninstallDisplayIcon={app}\{#AppExe}
SetupIconFile=..\..\vocalcode-app\vocalcode.ico
LicenseFile=..\..\LICENSE
WizardStyle=modern dark windows11
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
AppMutex=Local\VocalCode.Desktop
; A paid installation's uninstall log also lists files its installers put in
; the data folder, such as the first default vocalcode.toml. Appending to that
; log would make a later uninstall of VocalCode delete the person's settings,
; although this uninstaller otherwise keeps their data. Overwriting it records
; only what this installer put down; the uninstaller then also runs only this
; script's [Code], never the paid one's data purge. Files an older release put
; in {app} that this one does not install are removed in [InstallDelete], so
; the new log is complete. Keep that list current whenever a file is dropped.
UninstallLogMode=overwrite
OutputDir=..\..\dist-community\artifacts
OutputBaseFilename=VocalCodeSetup
Compression=lzma2/max
SolidCompression=yes
CloseApplications=no
#ifdef SIGNED_CACHE
SignedUninstaller=yes
SignedUninstallerDir={#SIGNED_CACHE}
#endif

[InstallDelete]
; Program files of the paid releases that this build does not ship. Paid 1.0
; to 1.1 bundled cargs.dll, which nothing imports now; a DLL left beside
; VocalCode.exe is the kind of file Windows could load by mistake.
Type: files; Name: "{app}\cargs.dll"
; Their licence notices describe their own runtime (1.0 to 1.1 also had
; Cargs-LICENSE.txt, which paid 1.2 left behind); this installer brings a
; complete set for what it installs.
Type: filesandordirs; Name: "{app}\THIRD-PARTY-LICENSES"

[Dirs]
; Removed on uninstall when empty, even when a paid release created it.
Name: "{app}"; Flags: uninsalwaysuninstall

[Files]
Source: "..\..\dist-community\windows\*"; DestDir: "{app}"; Excludes: "prerequisites\*"; Flags: recursesubdirs ignoreversion
Source: "..\..\dist-community\windows\prerequisites\MicrosoftEdgeWebview2Setup.exe"; Flags: dontcopy

[Icons]
Name: "{group}\VocalCode"; Filename: "{app}\{#AppExe}"
Name: "{group}\Uninstall VocalCode"; Filename: "{uninstallexe}"

[Run]
; skipifsilent: the paid app's updater runs this installer with /VERYSILENT and
; starts {app}\VocalCode.exe itself afterwards. A normal install offers it here.
Filename: "{app}\{#AppExe}"; Description: "Launch VocalCode"; Flags: nowait postinstall skipifsilent

[Code]
const
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
  { VocalCode Community 1.3.1 and 1.4.0, the early free builds, installed as
    their own app. This installer replaces them: once VocalCode is installed,
    their uninstaller runs, which removes their program files, shortcuts and
    login item and keeps their data folder, so VocalCode can import it. }
  EarlyUninstallKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\VocalCode.Community_is1';
  EarlyMutex = 'Local\VocalCode.Community.Desktop';
  EarlyExe = 'VocalCodeCommunity.exe';
  EarlyUninstaller = 'unins000.exe';
  { A silent install is usually the paid app's updater, which must not hang:
    a running early build is waited for briefly, then left installed. }
  EarlySilentWaitMs = 10000;
  { Their uninstaller finishes from a copy in %TEMP% after its first process
    has returned. }
  EarlySettleMs = 60000;

function HasWebView2(): Boolean;
var V: String; K: String;
begin
  K := 'Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}';
  Result := (RegQueryStringValue(HKCU, K, 'pv', V) and (V <> '') and (V <> '0.0.0.0')) or
            (RegQueryStringValue(HKLM32, K, 'pv', V) and (V <> '') and (V <> '0.0.0.0')) or
            (RegQueryStringValue(HKLM64, K, 'pv', V) and (V <> '') and (V <> '0.0.0.0'));
end;

function GetLongPathName(ShortPath: String; LongPath: String; Size: Cardinal): Cardinal;
  external 'GetLongPathNameW@kernel32.dll stdcall';

{ The full, long spelling of a path, so that two spellings of one existing
  file compare equal. A path that does not exist is compared as written. }
function LongPathOf(Path: String): String;
var
  Buffer: String;
  Count: Cardinal;
begin
  Result := Trim(RemoveQuotes(Trim(Path)));
  if Copy(Result, 1, 4) = '\\?\' then
    Result := Copy(Result, 5, Length(Result));
  Result := ExpandFileName(Result);
  SetLength(Buffer, 1024);
  Count := GetLongPathName(Result, Buffer, 1024);
  if (Count > 0) and (Count < 1024) then
    Result := Copy(Buffer, 1, Count);
end;

{ The paid releases' in-app updater, and this app's own, runs this installer
  silently with VC_UPDATE_EXE set to the running VocalCode.exe and restarts
  exactly that file afterwards. When that copy is not VocalCode.exe in the
  folder this installer installs into - a Scoop installation unpacks the
  installer and never registers itself, so that folder is then a new one -
  installing would leave a second VocalCode there while the updater restarts
  the old one, which offers the same update again. Refuse before anything is
  changed: a silent run exits with code 7, and the updater restarts the old
  app with --update-failed 7. A normal install, without VC_UPDATE_EXE, is
  unaffected. }
function UpdaterRestartsAnotherCopy(): String;
var
  Restarts, Installs: String;
begin
  Result := '';
  Restarts := Trim(GetEnv('VC_UPDATE_EXE'));
  if Restarts = '' then
    exit;
  Installs := AddBackslash(ExpandConstant('{app}')) + '{#AppExe}';
  if CompareText(LongPathOf(Restarts), LongPathOf(Installs)) = 0 then
    exit;
  Log('The updater would restart ' + Restarts + ', not ' + Installs + '; nothing is installed.');
  Result := 'This copy of VocalCode was not installed by VocalCodeSetup.exe (for example, it came from Scoop), so it cannot update itself. ' +
            'Remove it (for Scoop: scoop uninstall vocalcode), then install VocalCode from https://github.com/wudaming00/vocalcode-community/releases.';
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var ExitCode: Integer;
begin
  Result := UpdaterRestartsAnotherCopy();
  if Result <> '' then
    exit;
  if not HasWebView2() then begin
    ExtractTemporaryFile('MicrosoftEdgeWebview2Setup.exe');
    if not Exec(ExpandConstant('{tmp}\MicrosoftEdgeWebview2Setup.exe'), '/silent /install', '', SW_HIDE, ewWaitUntilTerminated, ExitCode) then
      Result := 'Microsoft WebView2 could not be started. Install the WebView2 runtime and retry.'
    else if not HasWebView2() then
      Result := 'Microsoft WebView2 is required. Check your internet connection, install the runtime, and retry.';
  end;
end;

{ The early free build's own per-user installation, or '' when there is none
  this installer may act on. Every field is checked; anything else is left
  alone, including a machine-wide installation. }
function EarlyInstallDir(): String;
var
  Location, Uninstaller, Name: String;
begin
  Result := '';
  if not RegQueryStringValue(HKCU, EarlyUninstallKey, 'InstallLocation', Location) then
    exit;
  Location := Trim(Location);
  if (Length(Location) < 4) or (Copy(Location, 2, 2) <> ':\') then begin
    Log('The registered VocalCode Community location is not a local folder: ' + Location);
    exit;
  end;
  Location := AddBackslash(Location);
  if not RegQueryStringValue(HKCU, EarlyUninstallKey, 'DisplayName', Name) or
     (Pos('VocalCode Community', Name) <> 1) then begin
    Log('The registered VocalCode Community has an unexpected name: ' + Name);
    exit;
  end;
  if not RegQueryStringValue(HKCU, EarlyUninstallKey, 'UninstallString', Uninstaller) or
     (CompareText(RemoveQuotes(Trim(Uninstaller)), Location + EarlyUninstaller) <> 0) then begin
    Log('The registered VocalCode Community uninstaller is not in its own folder: ' + Uninstaller);
    exit;
  end;
  if not FileExists(Location + EarlyUninstaller) then begin
    Log('The registered VocalCode Community folder has no uninstaller: ' + Location);
    exit;
  end;
  if CompareText(Location, AddBackslash(ExpandConstant('{app}'))) = 0 then begin
    Log('VocalCode was installed into VocalCode Community''s folder; nothing is uninstalled.');
    exit;
  end;
  Result := Location;
end;

{ False when the early build is still running and is to be left installed. }
function EarlyStopped(): Boolean;
var
  Waited: Integer;
begin
  Result := True;
  Waited := 0;
  while CheckForMutexes(EarlyMutex) do begin
    if WizardSilent() then begin
      if Waited >= EarlySilentWaitMs then begin
        Log('VocalCode Community is running, so it stays installed for now. VocalCode shows that it is running and can turn off its login item.');
        Result := False;
        exit;
      end;
      Sleep(500);
      Waited := Waited + 500;
    end else if MsgBox('VocalCode Community, the earlier free build, is still running. VocalCode replaces it and can import its data.' + #13#10#13#10 +
                       'Quit VocalCode Community (right-click its icon in the notification area, then choose Quit), then click Retry.' + #13#10 +
                       'Cancel keeps it installed for now; you can remove it later in Settings > Apps.',
                       mbConfirmation, MB_RETRYCANCEL) <> IDRETRY then begin
      Log('VocalCode Community stays installed at the user''s choice.');
      Result := False;
      exit;
    end;
  end;
end;

procedure RetireEarlyEdition();
var
  Dir: String;
  ResultCode, Waited: Integer;
begin
  Dir := EarlyInstallDir();
  if Dir = '' then
    exit;
  Log('Found VocalCode Community in ' + Dir);
  if not EarlyStopped() then
    exit;
  if not Exec(Dir + EarlyUninstaller, '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART', '', SW_HIDE,
    ewWaitUntilTerminated, ResultCode) then begin
    Log('VocalCode Community''s uninstaller could not start: ' + SysErrorMessage(ResultCode));
    exit;
  end;
  Log(Format('VocalCode Community''s uninstaller exited with %d', [ResultCode]));
  Waited := 0;
  if ResultCode = 0 then
    while (RegKeyExists(HKCU, EarlyUninstallKey) or FileExists(Dir + EarlyExe)) and (Waited < EarlySettleMs) do begin
      Sleep(250);
      Waited := Waited + 250;
    end;
  if RegKeyExists(HKCU, EarlyUninstallKey) then
    Log('VocalCode Community is still installed; remove it in Settings > Apps.')
  else
    Log('VocalCode Community was uninstalled; its data folder is kept for import in VocalCode.');
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep <> ssPostInstall then
    exit;
  { Never makes this installation fail: VocalCode is already in place. }
  try
    RetireEarlyEdition();
  except
    Log('Replacing VocalCode Community stopped: ' + GetExceptionMessage);
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Command: String;
begin
  if CurUninstallStep <> usUninstall then
    exit;
  { The login item, when it starts this copy. The shortcut is the one the
    paid 0.4 installers created, which VocalCode itself also removes. }
  if RegQueryStringValue(HKCU, RunKey, 'VocalCode', Command) and
     (Pos(Lowercase(ExpandConstant('{app}\{#AppExe}')), Lowercase(Command)) > 0) then
    RegDeleteValue(HKCU, RunKey, 'VocalCode');
  DeleteFile(ExpandConstant('{userstartup}\VocalCode.lnk'));
  { Models, settings, dictionary, meetings and kept History are intentionally
    kept, as are any files an older paid release left in the data folder. }
end;
