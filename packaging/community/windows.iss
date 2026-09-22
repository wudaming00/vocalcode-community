#define AppName "VocalCode Community"
#define AppExe "VocalCodeCommunity.exe"
#define AppVersion GetStringFileInfo("..\..\dist-community\windows\" + AppExe, "FileVersion")
[Setup]
AppId=VocalCode.Community
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Daming Wu
AppPublisherURL=https://github.com/wudaming00/vocalcode-community
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
VersionInfoOriginalFileName=VocalCodeCommunitySetup.exe
DefaultDirName={localappdata}\Programs\VocalCode Community
DefaultGroupName=VocalCode Community
UninstallDisplayIcon={app}\{#AppExe}
SetupIconFile=..\..\vocalcode-app\vocalcode.ico
LicenseFile=..\..\LICENSE
WizardStyle=modern dark windows11
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
AppMutex=Local\VocalCode.Community.Desktop
OutputDir=..\..\dist-community\artifacts
OutputBaseFilename=VocalCodeCommunitySetup
Compression=lzma2/max
SolidCompression=yes
CloseApplications=no
#ifdef SIGNED_CACHE
SignedUninstaller=yes
SignedUninstallerDir={#SIGNED_CACHE}
#endif

[Files]
Source: "..\..\dist-community\windows\*"; DestDir: "{app}"; Excludes: "prerequisites\*"; Flags: recursesubdirs ignoreversion
Source: "..\..\dist-community\windows\prerequisites\MicrosoftEdgeWebview2Setup.exe"; Flags: dontcopy

[Icons]
Name: "{group}\VocalCode Community"; Filename: "{app}\{#AppExe}"
Name: "{group}\Uninstall VocalCode Community"; Filename: "{uninstallexe}"

[Run]
Filename: "{app}\{#AppExe}"; Description: "Launch VocalCode Community"; Flags: nowait postinstall skipifsilent

[Code]
function HasWebView2(): Boolean;
var V: String; K: String;
begin
  K := 'Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}';
  Result := (RegQueryStringValue(HKCU, K, 'pv', V) and (V <> '') and (V <> '0.0.0.0')) or
            (RegQueryStringValue(HKLM32, K, 'pv', V) and (V <> '') and (V <> '0.0.0.0')) or
            (RegQueryStringValue(HKLM64, K, 'pv', V) and (V <> '') and (V <> '0.0.0.0'));
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var ExitCode: Integer;
begin
  Result := '';
  if not HasWebView2() then begin
    ExtractTemporaryFile('MicrosoftEdgeWebview2Setup.exe');
    if not Exec(ExpandConstant('{tmp}\MicrosoftEdgeWebview2Setup.exe'), '/silent /install', '', SW_HIDE, ewWaitUntilTerminated, ExitCode) then
      Result := 'Microsoft WebView2 could not be started. Install the WebView2 runtime and retry.'
    else if not HasWebView2() then
      Result := 'Microsoft WebView2 is required. Check your internet connection, install the runtime, and retry.';
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'VocalCodeCommunity');
  { User models, settings, vocabulary and meeting data are intentionally kept.
    The legacy edition's files, startup entry and installation are never touched. }
end;
