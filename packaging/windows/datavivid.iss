; Inno Setup script for the DataVivid Windows installer.
;
; Built by .github/workflows/release.yml:
;   ISCC.exe /DAppVersion=0.1.2 /DSourceDir=<repo> [/DIconFile=<ico>] packaging\windows\datavivid.iss
; Output: dist\DataVivid-<version>-windows-x64-setup.exe
;
; - Per-user install, no admin rights, into %LOCALAPPDATA%\Programs\DataVivid.
;   That is the folder scripts\install.ps1 uses, so the setup replaces an
;   install made from the zip, and it removes that install's own Settings > Apps
;   entry and uninstall.ps1.
; - AppId below must never change: it is how a newer setup finds and replaces
;   an older one (same folder, one entry in Settings > Apps).
; - DataVivid is closed if it is running, and `datavivid.exe --register` adds
;   it to Explorer's "Open with" and to Settings > Default apps. The uninstaller
;   runs `--unregister` first.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\.."
#endif

[Setup]
AppId={{EEAF0EC4-489A-414E-89DA-9EADC0DD4AF8}
AppName=DataVivid
AppVersion={#AppVersion}
AppVerName=DataVivid {#AppVersion}
AppPublisher=Prevail Team, FH Steyr
AppPublisherURL=https://github.com/FH-Prevail/DataVivid
AppSupportURL=https://github.com/FH-Prevail/DataVivid/issues
AppUpdatesURL=https://github.com/FH-Prevail/DataVivid/releases
AppCopyright=(c) 2026 Prevail Team, FH Steyr. MIT License
VersionInfoVersion={#AppVersion}
VersionInfoDescription=DataVivid Setup
; {autopf} is %LOCALAPPDATA%\Programs for a per-user install.
DefaultDirName={autopf}\DataVivid
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#SourceDir}\dist
OutputBaseFilename=DataVivid-{#AppVersion}-windows-x64-setup
#ifdef IconFile
SetupIconFile={#IconFile}
#endif
UninstallDisplayIcon={app}\datavivid.exe
UninstallDisplayName=DataVivid
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
CloseApplications=yes
RestartApplications=no
ChangesAssociations=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "german"; MessagesFile: "compiler:Languages\German.isl"

[CustomMessages]
english.ChooseDefaults=Choose DataVivid as the default app for data files
german.ChooseDefaults=DataVivid als Standard-App für Datendateien auswählen
english.Registering=Registering DataVivid for data files...
german.Registering=DataVivid wird für Datendateien registriert...

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[InstallDelete]
; Left over from an install made with scripts\install.ps1.
Type: files; Name: "{app}\uninstall.ps1"

[Files]
Source: "{#SourceDir}\target\release\datavivid.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\DataVivid"; Filename: "{app}\datavivid.exe"; WorkingDir: "{app}"; Comment: "View, clean and convert tabular data"
Name: "{autodesktop}\DataVivid"; Filename: "{app}\datavivid.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\datavivid.exe"; Parameters: "--register"; StatusMsg: "{cm:Registering}"; Flags: runhidden
Filename: "ms-settings:defaultapps?registeredAppUser=DataVivid"; Description: "{cm:ChooseDefaults}"; Flags: postinstall shellexec nowait skipifsilent unchecked
Filename: "{app}\datavivid.exe"; Description: "{cm:LaunchProgram,DataVivid}"; Flags: postinstall nowait skipifsilent

[UninstallRun]
Filename: "{app}\datavivid.exe"; Parameters: "--unregister"; Flags: runhidden; RunOnceId: "UnregisterDataVivid"

[Code]
procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
    // An install made with scripts\install.ps1 has its own Settings > Apps
    // entry; this setup registers its own, so drop the old one.
    RegDeleteKeyIncludingSubkeys(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\DataVivid');
end;
