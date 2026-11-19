; Inno Setup script for wim. Build from the repo root with:
;   iscc /DMyVersion=0.1.0 packaging\windows\wim.iss
; Expects a staging dir `dist\` containing wim.exe, shellswim.exe, mpv.exe
; and its DLLs. wim is a TUI and must run inside a terminal.

#ifndef MyVersion
  #error Pass the version with /DMyVersion=x.y.z
#endif

[Setup]
AppName=wim
AppVersion={#MyVersion}
AppPublisher=wim
DefaultDirName={autopf}\wim
DefaultGroupName=wim
OutputDir=..\..\dist-setup
OutputBaseFilename=wim-{#MyVersion}-windows-x64-setup
LicenseFile=..\..\LICENSE
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
Compression=lzma2
SolidCompression=yes
PrivilegesRequiredOverridesAllowed=dialog
ChangesEnvironment=yes
UninstallDisplayName=wim

[Tasks]
Name: "addtopath"; Description: "Add wim to the user PATH"; GroupDescription: "Options:"; Flags: unchecked

[Files]
Source: "..\..\dist\wim.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\dist\shellswim.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\dist\mpv.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\dist\*.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
; Windows Terminal when installed, otherwise cmd.exe.
Name: "{group}\wim"; Filename: "{code:TerminalExe}"; Parameters: "{code:TerminalParams|wim.exe}"; WorkingDir: "{userdocs}"; Check: UseWindowsTerminal
Name: "{group}\wim"; Filename: "{cmd}"; Parameters: "/k ""{app}\wim.exe"""; WorkingDir: "{userdocs}"; Check: not UseWindowsTerminal
Name: "{group}\shellswim"; Filename: "{code:TerminalExe}"; Parameters: "{code:TerminalParams|shellswim.exe}"; WorkingDir: "{userdocs}"; Check: UseWindowsTerminal
Name: "{group}\shellswim"; Filename: "{cmd}"; Parameters: "/k ""{app}\shellswim.exe"""; WorkingDir: "{userdocs}"; Check: not UseWindowsTerminal
Name: "{group}\Uninstall wim"; Filename: "{uninstallexe}"

[Code]
const
  EnvKey = 'Environment';

function WtPath(): String;
begin
  Result := ExpandConstant('{localappdata}\Microsoft\WindowsApps\wt.exe');
end;

function UseWindowsTerminal(): Boolean;
begin
  Result := FileExists(WtPath());
end;

function TerminalExe(Param: String): String;
begin
  Result := WtPath();
end;

function TerminalParams(Param: String): String;
begin
  Result := '-d "' + ExpandConstant('{userdocs}') + '" "' + ExpandConstant('{app}') + '\' + Param + '"';
end;

function PathContains(Dir: String): Boolean;
var
  Paths: String;
begin
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', Paths) then
    Paths := '';
  Result := Pos(';' + Lowercase(Dir) + ';', ';' + Lowercase(Paths) + ';') > 0;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Paths: String;
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
  begin
    if not PathContains(ExpandConstant('{app}')) then
    begin
      if not RegQueryStringValue(HKCU, EnvKey, 'Path', Paths) then
        Paths := '';
      if (Paths <> '') and (Copy(Paths, Length(Paths), 1) <> ';') then
        Paths := Paths + ';';
      RegWriteExpandStringValue(HKCU, EnvKey, 'Path', Paths + ExpandConstant('{app}'));
    end;
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Paths, Dir: String;
  P: Integer;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    Dir := ExpandConstant('{app}');
    if RegQueryStringValue(HKCU, EnvKey, 'Path', Paths) then
    begin
      Paths := ';' + Paths + ';';
      P := Pos(';' + Lowercase(Dir) + ';', Lowercase(Paths));
      if P > 0 then
      begin
        Delete(Paths, P, Length(Dir) + 1);
        Paths := Copy(Paths, 2, Length(Paths) - 2);
        RegWriteExpandStringValue(HKCU, EnvKey, 'Path', Paths);
      end;
    end;
  end;
end;
