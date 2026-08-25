; GLMeter Inno Setup script
; Build (from repo root, after copying glmeter.exe here):
;   iscc /DVERSION=x.y.z glmeter.iss

#ifndef VERSION
  #define VERSION "0.0.0"
#endif

[Setup]
AppId={{4C7A9B2F-8D3E-4A16-9B5C-2E7F1D8A0C93}
AppName=GLMeter
AppVersion={#VERSION}
AppPublisher=crazykun
AppPublisherURL=https://github.com/crazykun/GLMeter
AppSupportURL=https://github.com/crazykun/GLMeter/issues
DefaultDirName={localappdata}\GLMeter
DefaultGroupName=GLMeter
PrivilegesRequired=lowest
OutputBaseFilename=glmeter-{#VERSION}-setup
OutputDir=.
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesInstallIn64BitMode=x64compatible
SetupIconFile=icon.ico
UninstallDisplayIcon={app}\glmeter.exe

[Files]
Source: "glmeter.exe"; DestDir: "{app}"; Flags: ignoreversion

[Tasks]
Name: "startup"; Description: "Start GLMeter when Windows starts (开机自启)"; Flags: unchecked

[Icons]
Name: "{group}\GLMeter"; Filename: "{app}\glmeter.exe"
Name: "{userstartup}\GLMeter"; Filename: "{app}\glmeter.exe"; Tasks: startup

[Run]
Filename: "{app}\glmeter.exe"; Description: "Launch GLMeter now"; Flags: nowait postinstall skipifsilent
