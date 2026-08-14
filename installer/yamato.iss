; SPDX-License-Identifier: MIT
;
; Yamato - fan control software for ThinkPads
; Copyright (c) 2026 David Brustein
;
; Build with:  build.cmd installer
; Requires dist\ to have been staged first, which build.cmd does.

#define AppName     "Yamato"
#define AppVersion  "1.0.10"
#define AppExe      "yamato.exe"
#define AppPublisher "David Brustein"

[Setup]
AppId={{A24D8D93-7F01-4483-AC38-3C63EF96779D}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppComments=Not affiliated with, endorsed by, or supported by Lenovo. Comes with no warranty.
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
OutputDir=..\dist
OutputBaseFilename=Yamato-{#AppVersion}-setup
SetupIconFile=..\assets\yamato.ico
UninstallDisplayIcon={app}\{#AppExe}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Reaching the port driver and installing a service both need it.
PrivilegesRequired=admin
; The program is x64; there is no 32 bit build.
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
LicenseFile=..\LICENSE

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
; The service is not really optional, and the wording says so rather than
; leaving someone to work it out from a gray icon. It is the only part that
; talks to the controller: the tray reads what the service publishes and sends
; it requests, and never touches the fan itself. Without it Yamato displays
; nothing and controls nothing.
Name: "service"; Description: "Control the fan (installs the Yamato service - required, nothing works without it)"; GroupDescription: "Startup"
Name: "trayicon"; Description: "Show the tray icon when I log in"; GroupDescription: "Startup"

[Dirs]
; The settings directory, created here rather than left to whoever saves
; first.
;
; Two processes write it: the service, running as SYSTEM, which records which
; controller layout the probe found, and the window, running as whoever logged
; in. On a fresh install the service reaches it first, at the boot before
; anybody has opened the window, and a directory SYSTEM created on NTFS can
; leave files the window is then never able to replace. The symptom would be a
; settings window that saves without complaint and changes nothing.
;
; Made once by the installer, which is elevated and runs before either of
; them, with users granted modify so both sides can write for the life of the
; install. The program keeps its own fallback for anyone running the service
; without having installed it.
Name: "{commonappdata}\{#AppName}"; Permissions: users-modify

[Files]
Source: "..\dist\{#AppExe}";           DestDir: "{app}"; Flags: ignoreversion
; The PawnIO modules are looked for next to the executable, not in the working
; directory, because a service and a run-key launch both start elsewhere.
; Both are installed on every machine: LpcACPIEC serves the standard EC ports
; and LpcIO the 0x1600 window some ThinkPads use instead. The engine probes
; for which one this machine needs at its first start and remembers the
; answer in its settings.
Source: "..\dist\LpcACPIEC.bin";       DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\LpcIO.bin";           DestDir: "{app}"; Flags: ignoreversion
; The modules' sources, which the LGPL wants shipped with the objects rather
; than left at a link somewhere else.
Source: "..\dist\LpcACPIEC.p";         DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\LpcIO.p";             DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\LICENSE";             DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\LICENSE.LGPL-2.1.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\NOTICE.md";           DestDir: "{app}"; Flags: ignoreversion
; The MIT notices, which have to travel with the binary they are in.
Source: "..\dist\THIRD-PARTY-LICENSES.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"

[Registry]
; Machine-wide only. The per-user run entry is deliberately NOT written here:
; this installer runs elevated, so HKCU would be the administrator's hive
; rather than the person actually logging in, and the tray icon would silently
; never appear for them. It is set below by the program itself, running as the
; original user.
;
; Anything left over from a previous build that asked for administrator would
; override the manifest and stop the run entry starting at all.
Root: HKLM; Subkey: "Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers"; \
    ValueName: "{app}\{#AppExe}"; Flags: deletevalue

[Run]
Filename: "{app}\{#AppExe}"; Parameters: "--install"; \
    StatusMsg: "Installing the Yamato service..."; Flags: runhidden waituntilterminated; \
    Tasks: service

; runasoriginaluser matters: this has to touch the logging-on user's registry,
; not the elevated installer's.
Filename: "{app}\{#AppExe}"; Parameters: "--enable-startup"; \
    StatusMsg: "Setting up the tray icon..."; \
    Flags: runhidden waituntilterminated runasoriginaluser; Tasks: trayicon

Filename: "{app}\{#AppExe}"; Description: "Start Yamato now"; \
    Flags: postinstall nowait skipifsilent runasoriginaluser

; The service is removed from CurUninstallStepChanged below rather than from an
; [UninstallRun] entry. That entry cannot look at an exit code, and this one
; matters: Yamato refuses to delete a service it could not stop, because a
; service marked for deletion while still running goes on driving the fan while
; every interface reports it gone. An uninstaller that ignored the refusal
; would delete the files regardless and leave a registration pointing at an
; executable that is no longer there.

[Code]
// The version floor, split the way VS_FIXEDFILEINFO carries it. PawnIO
// before 2.2.0 can blue-screen Windows 10 1809, 19H1 and 19H2 on port
// access, and 1809 is a version Yamato supports.
const
  MinPawnIoMS = $00020002; // 2.2, major in the high word
  MinPawnIoLS = $00000000; // .0.0

// PawnIO is deliberately not bundled. It is GPL-2.0, so redistributing the
// driver would oblige us to ship its source; pointing at the download does
// not. It is also not ours to sign for.
function PawnIoInstalled(): Boolean;
var
  Ignored: Cardinal;
begin
  Result := RegQueryDWordValue(HKLM, 'SYSTEM\CurrentControlSet\Services\PawnIO', 'Type', Ignored);
end;

// Where the service registration says the driver binary is. The value
// usually reads '\SystemRoot\...', the kernel's spelling of the Windows
// directory, which nothing user-mode can open until it is translated.
function PawnIoSysPath(): String;
var
  Raw: String;
begin
  Result := '';
  if not RegQueryStringValue(HKLM, 'SYSTEM\CurrentControlSet\Services\PawnIO',
                             'ImagePath', Raw) then
    Exit;

  if Pos('\systemroot\', Lowercase(Raw)) = 1 then
    Result := ExpandConstant('{win}') + '\' + Copy(Raw, Length('\SystemRoot\') + 1, Length(Raw))
  else if Pos('\??\', Raw) = 1 then
    Result := Copy(Raw, Length('\??\') + 1, Length(Raw))
  else if Pos('system32\', Lowercase(Raw)) = 1 then
    Result := ExpandConstant('{win}') + '\' + Raw
  else
    Result := Raw;
end;

// The installed PawnIO's file version. Read from the driver binary itself,
// the file the old crash lives in; failing that, from PawnIOLib.dll in
// PawnIO's install directory, which carries the same release version.
// PawnIO's own version query is deliberately not used: it tracks the API,
// not the release, and still answers 2.0.0 on a current 2.2.0 install.
function PawnIoVersion(var MS, LS: Cardinal): Boolean;
var
  Path: String;
begin
  Result := False;

  Path := PawnIoSysPath();
  if (Path <> '') and FileExists(Path) then
    Result := GetVersionNumbers(Path, MS, LS);

  if not Result then
  begin
    if not RegQueryStringValue(HKLM,
        'SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO',
        'InstallLocation', Path) then
      Path := ExpandConstant('{commonpf}\PawnIO');
    Path := AddBackslash(Path) + 'PawnIOLib.dll';

    if FileExists(Path) then
      Result := GetVersionNumbers(Path, MS, LS);
  end;
end;

// Readable and below the floor. An unreadable version is not evidence of an
// old one, so it never refuses; the engine applies the same rule when it
// starts, and logs when the check could not be made.
function PawnIoTooOld(var Found: String): Boolean;
var
  MS, LS: Cardinal;
begin
  Result := False;
  if PawnIoVersion(MS, LS) then
  begin
    Found := Format('%d.%d.%d', [MS shr 16, MS and $FFFF, LS shr 16]);
    Result := (MS < MinPawnIoMS) or ((MS = MinPawnIoMS) and (LS < MinPawnIoLS));
  end;
end;

// Stops a running service before any file is replaced.
//
// Upgrading over a running Yamato otherwise finds yamato.exe locked by the
// service that is using it: Windows will not replace a file in use, so the
// install either fails or quietly defers the replacement to the next reboot,
// leaving the old engine driving the fan while the installer reports success.
//
// Stopping is also what hands the fan back to the firmware, so this leaves the
// machine under firmware control for the few seconds the files are being
// replaced -- which is the right state to be in while the program that manages
// the fan is being overwritten.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
  Exe: String;
begin
  Result := '';
  Exe := ExpandConstant('{app}\{#AppExe}');

  // Only on an upgrade. A first install has nothing to stop, and a failure
  // here would be reporting the absence of something that was never there.
  if FileExists(Exe) then
  begin
    Exec(Exe, '--stop-service', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Response: Integer;
  ErrorCode: Integer;
  Found: String;
begin
  if CurStep = ssPostInstall then
  begin
    if not PawnIoInstalled() then
    begin
      Response := MsgBox(
        'Yamato needs PawnIO to reach the embedded controller.' + #13#13 +
        'PawnIO is a small signed driver, made by someone else, that Yamato ' +
        'talks to. It is not bundled here, so it is a separate download.' + #13#13 +
        'Open the PawnIO download page now?',
        mbConfirmation, MB_YESNO);

      if Response = IDYES then
        ShellExec('open', 'https://pawnio.eu', '', '', SW_SHOW, ewNoWait, ErrorCode);
    end
    else if PawnIoTooOld(Found) then
    begin
      Response := MsgBox(
        'Yamato needs PawnIO 2.2.0 or later, and this machine has ' + Found + '.' + #13#13 +
        'PawnIO versions before 2.2.0 can crash some Windows 10 machines ' +
        'while reaching the embedded controller, so Yamato will not run ' +
        'with this one.' + #13#13 +
        'Open the PawnIO download page now?',
        mbConfirmation, MB_YESNO);

      if Response = IDYES then
        ShellExec('open', 'https://pawnio.eu', '', '', SW_SHOW, ewNoWait, ErrorCode);
    end;
  end;
end;

// Stops and removes the service before any file is deleted.
//
// Stopping it is what hands the fan back to the firmware, so this has to
// succeed before the program goes. Yamato returns a failure exit code if it
// could not stop the service, and that is worth stopping the uninstall over:
// carrying on would take away the only executable that knows how to release
// the fan, while a manual level might still be set.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ResultCode: Integer;
  Response: Integer;
begin
  if CurUninstallStep = usUninstall then
  begin
    if not Exec(ExpandConstant('{app}\{#AppExe}'), '--uninstall', '',
                SW_HIDE, ewWaitUntilTerminated, ResultCode) then
      ResultCode := -1;

    if ResultCode <> 0 then
    begin
      Response := MsgBox(
        'Yamato could not stop and remove its service.' + #13#13 +
        'The service may still be running, and it may still be holding the ' +
        'fan at a fixed level with the firmware''s own control switched off. ' +
        'Removing the files now would take away the only program that can ' +
        'hand the fan back.' + #13#13 +
        'Stop here so you can try again? Choosing No removes Yamato anyway, ' +
        'and you should then restart the machine to be certain the fan is ' +
        'back under firmware control.',
        mbError, MB_YESNO);

      if Response = IDYES then
        Abort();
    end;
  end;
end;
