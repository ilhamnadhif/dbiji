; DBiji's Windows installer (Inno Setup 6), built by scripts/bundle-windows.ps1:
;
;   ISCC.exe /DAppVersion=1.2.3 /DSourceExe=...\dbiji.exe /DOutputDir=...\dist ^
;            /DOutputBaseFilename=DBiji-1.2.3-windows-x64-setup packaging\windows\dbiji.iss
;
; Installs for the current user (no administrator rights) unless "all users"
; is picked, adds a Start menu shortcut (a desktop one when asked) and an
; uninstaller. Settings, connections and scripts (in %APPDATA%) stay when
; DBiji is uninstalled. Installing a new version over the old one updates it.

#ifndef AppVersion
  #error Define AppVersion (/DAppVersion=1.2.3)
#endif
#ifndef SourceExe
  #error Define SourceExe (/DSourceExe=path\to\dbiji.exe)
#endif
#ifndef OutputDir
  #define OutputDir "..\..\dist"
#endif
#ifndef OutputBaseFilename
  #define OutputBaseFilename "DBiji-" + AppVersion + "-windows-x64-setup"
#endif

[Setup]
AppId={{03F93606-1AE0-4EFD-A66F-1CC88238C476}
AppName=DBiji
AppVersion={#AppVersion}
AppVerName=DBiji {#AppVersion}
VersionInfoVersion={#AppVersion}
AppPublisher=DBiji
AppPublisherURL=https://github.com/ilhamnadhif/dbiji
AppSupportURL=https://github.com/ilhamnadhif/dbiji/issues
AppUpdatesURL=https://github.com/ilhamnadhif/dbiji/releases
DefaultDirName={autopf}\DBiji
DefaultGroupName=DBiji
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#OutputDir}
OutputBaseFilename={#OutputBaseFilename}
SetupIconFile=dbiji.ico
UninstallDisplayIcon={app}\dbiji.exe
UninstallDisplayName=DBiji
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; A running DBiji is closed before its files are replaced.
CloseApplications=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "dbiji.exe"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\DBiji"; Filename: "{app}\dbiji.exe"
Name: "{autodesktop}\DBiji"; Filename: "{app}\dbiji.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\dbiji.exe"; Description: "{cm:LaunchProgram,DBiji}"; Flags: nowait postinstall skipifsilent
