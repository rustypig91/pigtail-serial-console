; Inno Setup script for Rusty's Pigtail - Serial Terminal.
;
; This is the friendly, click-through installer. The MSI next to it
; (../../wix/main.wxs) covers scripted and managed deployment instead.
;
; Built in CI (see .github/workflows/release.yml), and locally with:
;   ISCC /DAppVersion=0.2.0 /DSourceBinDir=<dir holding pigtail.exe> pigtail.iss

#ifndef AppVersion
  #error AppVersion must be passed to ISCC, e.g. /DAppVersion=0.2.0
#endif
#ifndef SourceBinDir
  #error SourceBinDir must be passed to ISCC, e.g. /DSourceBinDir=target\release
#endif

#define AppName "Rusty's Pigtail - Serial Terminal"
#define AppPublisher "Christoffer Zakrisson"
#define AppURL "https://github.com/rustypig91/pigtail-serial-console"
#define AppExeName "pigtail.exe"

[Setup]
; Deliberately not the MSI's UpgradeCode: Windows must treat the two installer
; formats as separate products rather than upgrades of one another.
AppId={{374D0E66-90B2-4055-A852-0AF51237DA44}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}/issues
AppUpdatesURL={#AppURL}/releases
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
LicenseFile={#SourcePath}License.rtf
SetupIconFile={#SourcePath}pigtail.ico
UninstallDisplayIcon={app}\{#AppExeName}
OutputBaseFilename=pigtail-v{#AppVersion}-x86_64-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Recommend AppData, including when Setup was started elevated. The user can
; still explicitly select an all-users installation in Program Files.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
; Keep the choice available even if another installation scope already exists.
; The updater supplies /CURRENTUSER or /ALLUSERS for its existing destination.
UsePreviousPrivileges=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceBinDir}\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourcePath}..\..\..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourcePath}..\..\..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[InstallDelete]
; Remove the temporary apostrophe-free shortcut name if it was installed.
Type: files; Name: "{group}\Rustys Pigtail - Serial Terminal.lnk"

[Icons]
; Keep the display name and include the apostrophe-free spelling in metadata.
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Comment: "Rustys Pigtail - Serial Terminal"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(AppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent
