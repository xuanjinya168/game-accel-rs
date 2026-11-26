#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\dist\stage"
#endif
#ifndef OutputDir
  #define OutputDir "..\..\dist"
#endif
#ifndef OutputBaseName
  #define OutputBaseName "game-accel-v" + AppVersion + "-windows-x64-setup"
#endif

[Setup]
AppId={{8F6A2D51-4C93-4F2B-9E1A-3B7D5C8E4A10}
AppName=GameAccel
AppVersion={#AppVersion}
AppPublisher=game-accel-rs
DefaultDirName={autopf}\GameAccel
DefaultGroupName=GameAccel
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
OutputDir={#OutputDir}
OutputBaseFilename={#OutputBaseName}
SetupIconFile=..\..\crates\gui\assets\app.ico
UninstallDisplayIcon={app}\game-accel.exe

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "{#SourceDir}\game-accel.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\wintun.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\client.exe"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#SourceDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist

[Icons]
Name: "{group}\GameAccel"; Filename: "{app}\game-accel.exe"
Name: "{group}\Uninstall GameAccel"; Filename: "{uninstallexe}"
Name: "{autodesktop}\GameAccel"; Filename: "{app}\game-accel.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\game-accel.exe"; Description: "{cm:LaunchProgram,GameAccel}"; Flags: nowait postinstall skipifsilent shellexec
; In-app auto-update runs this installer with /SILENT: relaunch the new version afterwards.
Filename: "{app}\game-accel.exe"; Flags: nowait shellexec; Check: WizardSilent
