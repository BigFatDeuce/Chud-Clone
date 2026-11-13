; Chud Clone — installer (Inno Setup 6), English and French.
; Built by scripts\package.ps1, which provides AppVersion and SourceDir:
;   ISCC.exe /DAppVersion=0.1.0 /DSourceDir=..\dist\stage packaging\installer.iss

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\dist\stage"
#endif

[Setup]
AppId={{6F1D2B84-93A7-4C55-B0E2-7A4D9C3E1F58}
AppName=Chud Clone
AppVersion={#AppVersion}
AppVerName=Chud Clone {#AppVersion}
AppPublisher=Chud Clone
AppPublisherURL=https://github.com/BigFatDeuce/Chud-Clone
AppSupportURL=https://github.com/BigFatDeuce/Chud-Clone/issues
DefaultDirName={autopf}\Chud Clone
DefaultGroupName=Chud Clone
DisableProgramGroupPage=yes
; Installed for all users in Program Files: administrator rights required.
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\dist
OutputBaseFilename=ChudClone-{#AppVersion}-setup
SetupIconFile=..\crates\app\icons\icon.ico
UninstallDisplayIcon={app}\chud-clone.exe
UninstallDisplayName=Chud Clone
LicenseFile=..\LICENSE
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
; The wizard follows the Windows display language (English otherwise).
ShowLanguageDialog=auto
LanguageDetectionMethod=uilanguage

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "fr"; MessagesFile: "compiler:Languages\French.isl"

[CustomMessages]
en.DesktopIcon=Create a desktop shortcut
fr.DesktopIcon=Créer une icône sur le Bureau
en.Shortcuts=Shortcuts:
fr.Shortcuts=Raccourcis :
en.ReadMe=Read me
fr.ReadMe=Lisez-moi
en.UninstallApp=Uninstall Chud Clone
fr.UninstallApp=Désinstaller Chud Clone
en.LaunchApp=Launch Chud Clone
fr.LaunchApp=Lancer Chud Clone
en.NoWebView2=Microsoft Edge WebView2 Runtime was not found.%n%nChud Clone needs it to display its interface. It ships with Windows 11 and up-to-date Windows 10; otherwise, install it from Microsoft's website.%n%nContinue the installation?
fr.NoWebView2=Le composant Microsoft Edge WebView2 Runtime n'a pas été trouvé.%n%nChud Clone en a besoin pour afficher son interface. Il est présent d'origine sur Windows 11 et sur les Windows 10 à jour ; sinon, installez-le depuis le site de Microsoft.%n%nContinuer l'installation ?
en.Uninstalled=Chud Clone has been uninstalled.%n%nYour clone reports were kept in:%n%1%nDelete this folder if you no longer need it.
fr.Uninstalled=Chud Clone a été désinstallé.%n%nVos rapports de clonage ont été conservés dans :%n%1%nSupprimez ce dossier si vous n'en avez plus besoin.

[Tasks]
Name: "desktopicon"; Description: "{cm:DesktopIcon}"; GroupDescription: "{cm:Shortcuts}"

[Files]
Source: "{#SourceDir}\chud-clone.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\chud-clone-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\README.txt"; DestDir: "{app}"; Flags: ignoreversion isreadme
Source: "{#SourceDir}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion recursesubdirs

[Icons]
Name: "{group}\Chud Clone"; Filename: "{app}\chud-clone.exe"
Name: "{group}\{cm:ReadMe}"; Filename: "{app}\README.txt"
Name: "{group}\{cm:UninstallApp}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Chud Clone"; Filename: "{app}\chud-clone.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\chud-clone.exe"; Description: "{cm:LaunchApp}"; Flags: nowait postinstall skipifsilent unchecked

[Code]
// The interface relies on Microsoft Edge WebView2. Nothing is downloaded (no network
// connection): if the runtime is missing, the user is warned and may continue.
function WebView2Present(): Boolean;
var
  V: String;
begin
  Result :=
    (RegQueryStringValue(HKLM, 'SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}', 'pv', V) and (V <> '') and (V <> '0.0.0.0')) or
    (RegQueryStringValue(HKLM, 'SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}', 'pv', V) and (V <> '') and (V <> '0.0.0.0')) or
    (RegQueryStringValue(HKCU, 'Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}', 'pv', V) and (V <> '') and (V <> '0.0.0.0'));
end;

function InitializeSetup(): Boolean;
begin
  Result := True;
  if not WebView2Present() then
    Result := MsgBox(CustomMessage('NoWebView2'), mbConfirmation, MB_YESNO) = IDYES;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    MsgBox(FmtMessage(CustomMessage('Uninstalled'), [ExpandConstant('{userappdata}\ChudClone')]), mbInformation, MB_OK);
end;
