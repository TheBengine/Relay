; Local per-user installation. Publisher signing is a separate release gate.
#ifndef Stage
  #error Build this script through scripts/build-windows-installer.py
#endif
#ifndef RelayVersion
  #error Release version must be supplied by scripts/build-windows-installer.py
#endif
#ifndef RelayRegisterProtocol
  #define RelayRegisterProtocol 0
#endif

[Setup]
AppId={#RelayAppId}
AppName={#RelayAppName}
AppPublisher=TheBengine
AppVersion={#RelayVersion}
AppVerName={#RelayAppName} {#RelayVersion}
DefaultDirName={localappdata}\Programs\Relay
DefaultGroupName={#RelayAppName}
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
SetupArchitecture=x64
MinVersion=10.0
OutputDir={#Output}
OutputBaseFilename=Relay-{#RelayVersion}-{#ReleaseSequence}-Setup
SetupIconFile={#RelayIcon}
UninstallDisplayIcon={app}\{#RelayDisplayIcon}
WizardStyle=modern dynamic
DisableProgramGroupPage=yes
AllowNoIcons=yes
DisableReadyPage=no
DisableDirPage=auto
AlwaysShowDirOnReadyPage=yes
InfoBeforeFile={#Stage}\before.txt
CloseApplications=no
RestartApplications=no
RestartIfNeededByRun=no
UninstallLogMode=append
Compression=lzma2
SolidCompression=yes
#if Str(RelayRegisterProtocol) == "1"
ChangesAssociations=yes
#else
ChangesAssociations=no
#endif
ChangesEnvironment=no
UsePreviousAppDir=yes
UsePreviousTasks=yes
; Manual installs retain a choice; the native handoff supplies /LANG=english.
ShowLanguageDialog=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "danish"; MessagesFile: "compiler:Languages\Danish.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; Flags: unchecked

[Files]
#include Stage + "\generated-files.iss"

[Icons]
Name: "{userprograms}\{#RelayAppName}\Relay"; Filename: "{app}\relay.exe"; WorkingDir: "{app}"; AppUserModelID: "Relay.App"; Check: WantIcons
Name: "{userdesktop}\Relay"; Filename: "{app}\relay.exe"; WorkingDir: "{app}"; Tasks: desktopicon; AppUserModelID: "Relay.App"; Check: WantIcons

[Run]
Filename: "{app}\relay.exe"; Parameters: "--wait-for-installer={code:InstallerIdentity}"; WorkingDir: "{app}"; Description: "Open Relay"; Flags: nowait postinstall skipifsilent; Check: OfferOpenRelay

#if Str(RelayRegisterProtocol) == "1"
[UninstallRun]
; Invoke dist\relay.exe directly: the visible launcher exits before its child.
; The client checks exact command+owner and preserves another installation.
Filename: "{app}\{#RelayClientExecutable}"; Parameters: "--unregister-invitation-protocol"; WorkingDir: "{app}"; Flags: runhidden waituntilterminated skipifdoesntexist; RunOnceId: "RelayInvitationProtocolV1"
#endif

[Code]
const
  InvalidAttributes = $FFFFFFFF;
  ReparsePoint = $400;
  GenericWrite = $40000000;
  OpenExisting = 3;
  ShareAll = 7;

function FileAttributes(Path: String): LongWord;
  external 'GetFileAttributesW@kernel32.dll stdcall';
function DriveType(Root: String): LongWord;
  external 'GetDriveTypeW@kernel32.dll stdcall';
function OpenForWrite(Path: String; Access, Sharing: LongWord;
  Security: LongWord; Creation, Flags: LongWord; Template: THandle): THandle;
  external 'CreateFileW@kernel32.dll stdcall';
function CloseFile(Handle: THandle): Boolean;
  external 'CloseHandle@kernel32.dll stdcall';

type
  TUpdateFileTime = record
    Low: LongWord;
    High: LongWord;
  end;
  TUpdateWaitHandles = array[0..1] of THandle;

function OpenUpdateProcess(Access: LongWord; Inherit: Integer; Pid: LongWord): THandle;
  external 'OpenProcess@kernel32.dll stdcall';
function UpdateProcessTimes(Process: THandle; var Created, Exited, KernelTime, UserTime: TUpdateFileTime): Integer;
  external 'GetProcessTimes@kernel32.dll stdcall';
function UpdateProcessImage(Process: THandle; Flags: LongWord; Image: String; var Size: LongWord): Integer;
  external 'QueryFullProcessImageNameW@kernel32.dll stdcall';
function OpenUpdateEvent(Access: LongWord; Inherit: Integer; Name: String): THandle;
  external 'OpenEventW@kernel32.dll stdcall';
function SignalUpdateEvent(Event: THandle): Integer;
  external 'SetEvent@kernel32.dll stdcall';
function WaitUpdateObjects(Count: LongWord; var Handles: TUpdateWaitHandles; WaitAll: Integer; Milliseconds: LongWord): LongWord;
  external 'WaitForMultipleObjects@kernel32.dll stdcall';

function UpdateNumber(Value: String): Boolean;
var I: Integer; Number: Int64;
begin
  Result := False;
  if (Length(Value) = 0) or (Length(Value) > 10) then exit;
  for I := 1 to Length(Value) do
    if (Value[I] < '0') or (Value[I] > '9') then exit;
  Number := StrToInt64Def(Value, -1);
  Result := (Number >= 0) and (Number <= 4294967295);
end;


{ App handoffs use only the window of the PID+FILETIME-validated parent.
  These are presentation hints; they never approve installation. }
type
  TUpdateRect = record
    Left, Top, Right, Bottom: Integer;
  end;
  TUpdateMonitorInfo = record
    Size: LongWord;
    Monitor, Work: TUpdateRect;
    Flags: LongWord;
  end;

const
  UpdateReleaseFirstShow = $8000 + $527;
  UpdateSubclassId = $52454C59;
  UpdateWindowPosChanged = $0047;
  UpdateNcDestroy = $0082;
  UpdateCbtActivate = 5;

var
  UpdateParentPid: LongWord;
  UpdateParentWindow, UpdateParentMonitor, UpdateWizardWindow: THandle;
  UpdateParentWork: TUpdateRect;
  UpdateParentWindows: Integer;
  UpdateParentWorkPhysical: Boolean;
  UpdateEnumCallback, UpdateCbtCallback, UpdateSubclassCallback: NativeInt;
  UpdateShowHook: THandle;
  UpdateParentProcess, UpdateReadyEvent, UpdateCommitEvent, UpdateCancelEvent: THandle;
  UpdateHandoffPending, UpdateFirstShow, UpdateReleaseQueued, UpdateSubclassed: Boolean;
  UpdateRestartRequested, UpdateRestartApproved, UpdateRestartStarted: Boolean;
  UpdateAutomatic: Boolean;
  UpdateInstallDirectory, UpdateBaseVersion, UpdateBaseSequence, UpdateBaseHash: String;
  StopServerChoice: TNewCheckBox;
  StopServerHint: TNewStaticText;

function UpdateWindowPid(Window: THandle; var Pid: LongWord): LongWord;
  external 'GetWindowThreadProcessId@user32.dll stdcall';
function UpdateWindowVisible(Window: THandle): Integer;
  external 'IsWindowVisible@user32.dll stdcall';
function UpdateWindowExists(Window: THandle): Integer;
  external 'IsWindow@user32.dll stdcall';
function UpdateWindowOwner(Window: THandle; Command: LongWord): THandle;
  external 'GetWindow@user32.dll stdcall';
function UpdateEnumWindows(Callback: NativeInt; Parameter: NativeInt): Integer;
  external 'EnumWindows@user32.dll stdcall';
function UpdateMonitorForWindow(Window: THandle; Flags: LongWord): THandle;
  external 'MonitorFromWindow@user32.dll stdcall';
function UpdateMonitorForRect(var Rect: TUpdateRect; Flags: LongWord): THandle;
  external 'MonitorFromRect@user32.dll stdcall';
function UpdateMonitorInfo(Monitor: THandle; var Info: TUpdateMonitorInfo): Integer;
  external 'GetMonitorInfoW@user32.dll stdcall';
function UpdateWindowRect(Window: THandle; var Rect: TUpdateRect): Integer;
  external 'GetWindowRect@user32.dll stdcall';
function UpdatePositionWindow(Window, AfterWindow: THandle; X, Y, Width, Height: Integer; Flags: LongWord): Integer;
  external 'SetWindowPos@user32.dll stdcall';
function UpdateProcessWait(Process: THandle; Milliseconds: LongWord): LongWord;
  external 'WaitForSingleObject@kernel32.dll stdcall';
function UpdateCurrentThread: LongWord;
  external 'GetCurrentThreadId@kernel32.dll stdcall';
function UpdateInstallHook(Kind: Integer; Callback: NativeInt; Module: THandle; Thread: LongWord): THandle;
  external 'SetWindowsHookExW@user32.dll stdcall';
function UpdateNextHook(Hook: THandle; Code: Integer; WParam: NativeUInt; LParam: NativeInt): NativeInt;
  external 'CallNextHookEx@user32.dll stdcall';
function UpdateRemoveHook(Hook: THandle): Integer;
  external 'UnhookWindowsHookEx@user32.dll stdcall';
function UpdateInstallSubclass(Window: THandle; Callback: NativeInt; Id, Data: NativeUInt): Integer;
  external 'SetWindowSubclass@comctl32.dll stdcall';
function UpdateRemoveSubclass(Window: THandle; Callback: NativeInt; Id: NativeUInt): Integer;
  external 'RemoveWindowSubclass@comctl32.dll stdcall';
function UpdateDefaultSubclass(Window: THandle; Message: LongWord; WParam: NativeUInt; LParam: NativeInt): NativeInt;
  external 'DefSubclassProc@comctl32.dll stdcall';
function UpdatePostMessage(Window: THandle; Message: LongWord; WParam: NativeUInt; LParam: NativeInt): Integer;
  external 'PostMessageW@user32.dll stdcall';


function UpdateThreadDpiContext(Context: NativeInt): NativeInt;
  external 'SetThreadDpiAwarenessContext@user32.dll stdcall delayload';

function BeginUpdateGeometry: NativeInt;
begin
  Result := 0;
  try
    { Per-monitor-aware physical geometry is available from Windows 10 1607.
      This changes only this thread temporarily, never the existing HWND's awareness. }
    Result := UpdateThreadDpiContext(-3);
  except
    Log('Relay geometry is using system DPI because the thread DPI API is unavailable.');
  end;
  if Result = 0 then
    Log('Relay geometry could not enter per-monitor coordinates; mixed-DPI placement needs verification.');
end;

function ReadUpdateMonitorWork(Monitor: THandle; var Work: TUpdateRect; var Physical: Boolean): Boolean;
var Info: TUpdateMonitorInfo; PreviousDpi: NativeInt;
begin
  PreviousDpi := BeginUpdateGeometry;
  try
    Info.Size := 40;
    Result := UpdateMonitorInfo(Monitor, Info) <> 0;
    if Result then begin
      Work := Info.Work;
      Physical := PreviousDpi <> 0;
    end;
  finally
    if PreviousDpi <> 0 then UpdateThreadDpiContext(PreviousDpi);
  end;
end;

procedure EndUpdateFirstShow;
begin
  { Disable the veto even if an OS cleanup call fails. Later activation is normal. }
  UpdateFirstShow := False;
  if UpdateShowHook <> 0 then begin
    if UpdateRemoveHook(UpdateShowHook) <> 0 then UpdateShowHook := 0
    else Log('Relay first-show hook cleanup failed; activation veto is disabled.');
  end;
  if UpdateSubclassed then begin
    if (UpdateWindowExists(UpdateWizardWindow) = 0) or
       (UpdateRemoveSubclass(UpdateWizardWindow, UpdateSubclassCallback, UpdateSubclassId) <> 0) then
      UpdateSubclassed := False
    else Log('Relay first-show subclass cleanup will be retried at shutdown.');
  end;
end;

procedure CloseUpdateHandoff;
begin
  UpdateHandoffPending := False;
  if UpdateCancelEvent <> 0 then begin
    CloseFile(UpdateCancelEvent); UpdateCancelEvent := 0;
  end;
  if UpdateCommitEvent <> 0 then begin
    CloseFile(UpdateCommitEvent); UpdateCommitEvent := 0;
  end;
  if UpdateReadyEvent <> 0 then begin
    CloseFile(UpdateReadyEvent); UpdateReadyEvent := 0;
  end;
  if UpdateParentProcess <> 0 then begin
    CloseFile(UpdateParentProcess); UpdateParentProcess := 0;
  end;
end;

function UpdateParentWindowCandidate(Window: THandle; Parameter: NativeInt): Integer;
var Pid: LongWord;
begin
  Result := 1;
  Pid := 0;
  if (UpdateWindowPid(Window, Pid) <> 0) and (Pid = UpdateParentPid) and
     (UpdateWindowVisible(Window) <> 0) and (UpdateWindowOwner(Window, 4) = 0) then begin
    Inc(UpdateParentWindows);
    UpdateParentWindow := Window;
  end;
end;

function EligibleUpdateParentWindow(Process, Window: THandle): Boolean;
var Pid: LongWord;
begin
  Pid := 0;
  Result := (UpdateProcessWait(Process, 0) = 258) and
    (UpdateWindowPid(Window, Pid) <> 0) and (Pid = UpdateParentPid) and
    (UpdateWindowVisible(Window) <> 0) and (UpdateWindowOwner(Window, 4) = 0);
end;

function SnapshotUpdateParentMonitor(Process, Window: THandle): Boolean;
var Monitor: THandle; Work: TUpdateRect; Physical: Boolean;
begin
  Result := False;
  if not EligibleUpdateParentWindow(Process, Window) then exit;
  Monitor := UpdateMonitorForWindow(Window, 0);
  if (Monitor = 0) or not ReadUpdateMonitorWork(Monitor, Work, Physical) then exit;
  { Revalidate after the query too: a stale/recycled HWND is never a monitor hint. }
  if not EligibleUpdateParentWindow(Process, Window) or
     (UpdateMonitorForWindow(Window, 0) <> Monitor) then exit;
  UpdateParentMonitor := Monitor;
  UpdateParentWork := Work;
  UpdateParentWorkPhysical := Physical;
  Result := True;
end;

function CaptureUpdateParentMonitor(Process: THandle; Pid: LongWord): Boolean;
begin
  Result := False;
  UpdateParentPid := Pid;
  if UpdateProcessWait(Process, 0) <> 258 then exit;
  { Explicit owned silent handoffs deliberately have no desktop window. }
  if WizardSilent then begin Result := True; exit; end;
  UpdateParentWindow := 0;
  UpdateParentWindows := 0;
  if UpdateEnumCallback = 0 then UpdateEnumCallback := CreateCallback(@UpdateParentWindowCandidate);
  if UpdateEnumWindows(UpdateEnumCallback, 0) = 0 then exit;
  if UpdateParentWindows <> 1 then begin
    Log('Relay update needs one visible main window of the exact parent. Relay was left open.');
    exit;
  end;
  Result := SnapshotUpdateParentMonitor(Process, UpdateParentWindow);
end;

function UpdateFirstShowCbt(Code: Integer; WParam: NativeUInt; LParam: NativeInt): NativeInt;
begin
  if (Code = UpdateCbtActivate) and UpdateFirstShow and
     (UpdateWizardWindow <> 0) and (WParam = NativeUInt(UpdateWizardWindow)) then
    Result := 1
  else
    Result := UpdateNextHook(UpdateShowHook, Code, WParam, LParam);
end;

function UpdateFirstShowSubclass(Window: THandle; Message: LongWord;
  WParam: NativeUInt; LParam: NativeInt; Id, Data: NativeUInt): NativeInt;
begin
  if Message = UpdateReleaseFirstShow then begin
    EndUpdateFirstShow;
    Result := 0;
    exit;
  end;
  if Message = UpdateNcDestroy then EndUpdateFirstShow;
  Result := UpdateDefaultSubclass(Window, Message, WParam, LParam);
  { The posted release runs after the complete synchronous first-show operation.
    Forward the original message first so VCL still receives normal move/size events. }
  if (Window = UpdateWizardWindow) and (Message = UpdateWindowPosChanged) and
     UpdateFirstShow and not UpdateReleaseQueued and
     (UpdateWindowVisible(Window) <> 0) then begin
    UpdateReleaseQueued := UpdatePostMessage(Window, UpdateReleaseFirstShow, 0, 0) <> 0;
    if not UpdateReleaseQueued then begin
      { Do not leave a permanently non-activatable wizard after a queue failure. }
      Log('Relay could not queue first-show cleanup; activation veto is disabled.');
      EndUpdateFirstShow;
    end;
  end;
end;

function BeginUpdateFirstShow: Boolean;
begin
  UpdateFirstShow := True;
  UpdateReleaseQueued := False;
  if UpdateCbtCallback = 0 then UpdateCbtCallback := CreateCallback(@UpdateFirstShowCbt);
  { Current installer UI thread only; no global/input hook, injection or other app changes. }
  UpdateShowHook := UpdateInstallHook(5, UpdateCbtCallback, 0, UpdateCurrentThread);
  Result := UpdateShowHook <> 0;
  if not Result then UpdateFirstShow := False;
end;

procedure PlaceUpdateWizard;
var Info: TUpdateMonitorInfo; Rect: TUpdateRect;
  Monitor: THandle; Width, Height, WorkWidth, WorkHeight, X, Y: Integer;
  PreviousDpi: NativeInt;
begin
  WizardForm.CenterOnShow := False;
  WizardForm.Position := poDesigned;
  { Create the HWND with Inno's normal system-DPI context before the override. }
  UpdateWizardWindow := WizardForm.Handle;
  PreviousDpi := BeginUpdateGeometry;
  try
    if UpdateParentWorkPhysical <> (PreviousDpi <> 0) then
      RaiseException('The Relay installation screen could not be measured consistently.');
    Info.Size := 40;
    Monitor := UpdateParentMonitor;
    if (Monitor = 0) or (UpdateMonitorInfo(Monitor, Info) = 0) then begin
      { A removed display falls back from Relay's captured workspace, never the mouse. }
      Monitor := UpdateMonitorForRect(UpdateParentWork, 2);
      if (Monitor = 0) or (UpdateMonitorInfo(Monitor, Info) = 0) then
        RaiseException('The Relay installation screen is no longer available.');
      Log('Relay installation screen changed; using the nearest remaining workspace.');
    end;
    WorkWidth := Info.Work.Right - Info.Work.Left;
    WorkHeight := Info.Work.Bottom - Info.Work.Top;
    if (WorkWidth <= 0) or (WorkHeight <= 0) then
      RaiseException('The Relay installation screen has no available workspace.');
    if UpdateWindowRect(UpdateWizardWindow, Rect) = 0 then
      RaiseException('The Relay installation window could not be measured.');
    Width := Rect.Right - Rect.Left;
    Height := Rect.Bottom - Rect.Top;
    X := Info.Work.Left + (WorkWidth - Width) div 2;
    Y := Info.Work.Top + (WorkHeight - Height) div 2;
    if X < Info.Work.Left then X := Info.Work.Left;
    if Y < Info.Work.Top then Y := Info.Work.Top;
    { Move the existing hidden window first. Windows may bitmap-scale a system-DPI
      window on the target monitor, so measure its target rectangle afterwards.
      Never create HWNDs or apply a second DPI multiplier inside this override. }
    if UpdatePositionWindow(UpdateWizardWindow, 0, X, Y, 0, 0, $0001 or $0004 or $0010) = 0 then
      RaiseException('The Relay installation window could not be moved to its screen.');
    if UpdateWindowRect(UpdateWizardWindow, Rect) = 0 then
      RaiseException('The Relay installation window could not be measured on its screen.');
    Width := Rect.Right - Rect.Left;
    Height := Rect.Bottom - Rect.Top;
    if (Width <= 0) or (Height <= 0) or (WorkWidth < Width) or (WorkHeight < Height) then
      RaiseException('The Relay installation window does not fit on this screen. Increase its available resolution or reduce display scaling, then start the installer again.');
    X := Info.Work.Left + (WorkWidth - Width) div 2;
    Y := Info.Work.Top + (WorkHeight - Height) div 2;
    if X < Info.Work.Left then X := Info.Work.Left;
    if Y < Info.Work.Top then Y := Info.Work.Top;
    if X + Width > Info.Work.Right then X := Info.Work.Right - Width;
    if Y + Height > Info.Work.Bottom then Y := Info.Work.Bottom - Height;
    if UpdatePositionWindow(UpdateWizardWindow, 0, X, Y, 0, 0, $0001 or $0004 or $0010) = 0 then
      RaiseException('The Relay installation window could not be placed on its screen.');
    if UpdateWindowRect(UpdateWizardWindow, Rect) = 0 then
      RaiseException('The Relay installation window position could not be checked.');
    if (Rect.Left < Info.Work.Left) or (Rect.Top < Info.Work.Top) or
       (Rect.Right > Info.Work.Right) or (Rect.Bottom > Info.Work.Bottom) or
       (UpdateMonitorForWindow(UpdateWizardWindow, 0) <> Monitor) then
      RaiseException('The Relay installation window would be outside its screen.');
    if (Abs(Rect.Left - (Info.Work.Left + (WorkWidth - (Rect.Right - Rect.Left)) div 2)) > 1) or
       (Abs(Rect.Top - (Info.Work.Top + (WorkHeight - (Rect.Bottom - Rect.Top)) div 2)) > 1) then
      RaiseException('The Relay installation window could not be centered on its screen.');
    UpdateParentMonitor := Monitor;
    UpdateParentWork := Info.Work;
  finally
    if PreviousDpi <> 0 then UpdateThreadDpiContext(PreviousDpi);
  end;
end;

function CompleteUpdateHandoff: Boolean;
var Handles: TUpdateWaitHandles;
begin
  Result := False;
  if (UpdateParentProcess = 0) or (UpdateReadyEvent = 0) or
     (UpdateCommitEvent = 0) or (UpdateCancelEvent = 0) then exit;
  if UpdateProcessWait(UpdateParentProcess, 0) <> 258 then exit;
  if UpdateProcessWait(UpdateCancelEvent, 0) = 0 then begin
    Log('Relay update handoff was cancelled or was not approved.'); exit;
  end;
  if not WizardSilent then begin
    if not EligibleUpdateParentWindow(UpdateParentProcess, UpdateParentWindow) or
       (UpdateMonitorForWindow(UpdateParentWindow, 0) <> UpdateParentMonitor) then begin
      Log('Relay main window changed before installation readiness. Relay was left open.'); exit;
    end;
  end;
  { Ready now means the actual hidden wizard has passed presentation preparation.
    The same frozen monitor/workspace stays approved throughout this handoff. }
  if SignalUpdateEvent(UpdateReadyEvent) = 0 then exit;
  Handles[0] := UpdateCancelEvent; Handles[1] := UpdateCommitEvent;
  if WaitUpdateObjects(2, Handles, 0, 30000) <> 1 then begin
    Log('Relay update handoff was cancelled or was not approved.'); exit;
  end;
  Handles[1] := UpdateParentProcess;
  if WaitUpdateObjects(2, Handles, 0, 60000) <> 1 then begin
    Log('Relay update handoff did not observe the exact parent exit.'); exit;
  end;
  Log('Relay update handoff observed approval and the exact parent exit.');
  UpdateRestartApproved := UpdateRestartRequested;
  Result := True;
end;

procedure InitializeServerStopChoice; forward;

procedure InitializeWizard;
var Completed: Boolean;
begin
  InitializeServerStopChoice;
  if not UpdateHandoffPending then exit; { Manual installers retain normal Inno behavior. }
  if UpdateAutomatic and
     (CompareText(RemoveBackslashUnlessRoot(WizardDirValue), UpdateInstallDirectory) <> 0) then begin
    Log('The installer selected a different update directory. Relay was left open.');
    Abort;
  end;
  Completed := False;
  try
    try
      if not WizardSilent then begin
        if not SnapshotUpdateParentMonitor(UpdateParentProcess, UpdateParentWindow) then begin
          Log('Relay main window was unavailable before installation readiness. Relay was left open.');
          Abort;
        end;
        PlaceUpdateWizard;
        if UpdateSubclassCallback = 0 then
          UpdateSubclassCallback := CreateCallback(@UpdateFirstShowSubclass);
        UpdateSubclassed := UpdateInstallSubclass(UpdateWizardWindow, UpdateSubclassCallback, UpdateSubclassId, 0) <> 0;
        if not UpdateSubclassed then
          RaiseException('The Relay installation window could not preserve background startup.');
        if not BeginUpdateFirstShow then
          RaiseException('The Relay installation window could not preserve first-show focus.');
      end;
      { Silent installers call InitializeWizard too, but never need desktop hints/hooks. }
      if not CompleteUpdateHandoff then Abort;
      Completed := True;
    except
      Log('Relay installation preparation stopped: ' + GetExceptionMessage);
      Abort; { Documented silent setup exit from InitializeWizard; no error dialog. }
    end;
  finally
    CloseUpdateHandoff;
    if not Completed then EndUpdateFirstShow;
  end;
end;

function InitializeAutomaticUpdate: Boolean; forward;

function InitializeUpdateHandoff: Boolean;
var
  Identity, Name, Restart, Automatic, Pid, Low, High, Tail: String;
  Split, I: Integer;
  Created, Exited, KernelTime, UserTime: TUpdateFileTime;
begin
  Identity := ExpandConstant('{param:RELAYPARENT|}');
  Name := ExpandConstant('{param:RELAYHANDOFF|}');
  Restart := ExpandConstant('{param:RELAYRESTART|}');
  Automatic := ExpandConstant('{param:RELAYAUTOMATIC|}');
  Result := (Identity = '') and (Name = '') and (Restart = '') and (Automatic = '');
  if Result then exit; { Normal manually started installers are unchanged. }
  Result := False;
  if (Restart <> '') and (Restart <> '1') then exit;
  if (Automatic <> '') and (Automatic <> 'in-place-v2') then exit;
  UpdateAutomatic := Automatic = 'in-place-v2';
  if UpdateAutomatic and ((Restart <> '1') or not WizardSilent) then exit;
  if Length(Identity) > 32 then exit;
  Split := Pos('-', Identity); if Split = 0 then exit;
  Pid := Copy(Identity, 1, Split - 1);
  Tail := Copy(Identity, Split + 1, Length(Identity));
  Split := Pos('-', Tail); if Split = 0 then exit;
  Low := Copy(Tail, 1, Split - 1); High := Copy(Tail, Split + 1, Length(Tail));
  if not UpdateNumber(Pid) or not UpdateNumber(Low) or not UpdateNumber(High) then exit;
  if StrToInt64Def(Pid, 0) = 0 then exit;
  if (Length(Name) <> 82) or (Copy(Name, 1, 18) <> 'Local\RelayUpdate-') then exit;
  for I := 19 to 82 do
    if not (((Name[I] >= '0') and (Name[I] <= '9')) or ((Name[I] >= 'a') and (Name[I] <= 'f'))) then exit;
  { Pascal globals start at zero; ownership now spans InitializeSetup/InitializeWizard. }
  UpdateParentProcess := OpenUpdateProcess($00100000 or $1000, 0, StrToInt64Def(Pid, 0));
  if UpdateParentProcess = 0 then exit;
  try
    if UpdateProcessTimes(UpdateParentProcess, Created, Exited, KernelTime, UserTime) = 0 then exit;
    if (Created.Low <> StrToInt64Def(Low, -1)) or (Created.High <> StrToInt64Def(High, -1)) then exit;
    UpdateReadyEvent := OpenUpdateEvent($0002, 0, Name + '-ready');
    UpdateCommitEvent := OpenUpdateEvent($00100000, 0, Name + '-commit');
    UpdateCancelEvent := OpenUpdateEvent($00100000, 0, Name + '-cancel');
    if (UpdateReadyEvent = 0) or (UpdateCommitEvent = 0) or (UpdateCancelEvent = 0) then exit;
    if UpdateAutomatic and not InitializeAutomaticUpdate then exit;
    if not CaptureUpdateParentMonitor(UpdateParentProcess, StrToInt64Def(Pid, 0)) then exit;
    { Do not signal ready or wait here: the real hidden wizard must be prepared first. }
    UpdateRestartRequested := Restart = '1';
    UpdateHandoffPending := True;
    Result := True;
  finally
    if not Result then CloseUpdateHandoff;
  end;
end;

function OfferOpenRelay: Boolean;
begin
  { A legacy/manual installer keeps its normal optional Open Relay choice. }
  Result := not UpdateRestartApproved;
end;

function WantIcons: Boolean;
begin
  Result := not WizardForm.NoIconsCheck.Checked;
end;

function SamePath(A, B: String): Boolean;
begin
  Result := CompareText(RemoveBackslashUnlessRoot(A), RemoveBackslashUnlessRoot(B)) = 0;
end;

function UnderOrEqual(Path, Parent: String): Boolean;
begin
  Path := Lowercase(RemoveBackslashUnlessRoot(Path));
  Parent := Lowercase(RemoveBackslashUnlessRoot(Parent));
  Result := (Path = Parent) or (Pos(AddBackslash(Parent), AddBackslash(Path)) = 1);
end;

function DirectLocalPath(Path: String; var Error: String): Boolean;
var
  Current, Parent, Part: String;
  Attributes, LastError: LongWord;
  I, Start: Integer;
begin
  Result := False;
  if (Length(Path) < 3) or (Path[2] <> ':') or (Path[3] <> '\') or
    (Pos(':', Copy(Path, 3, Length(Path))) <> 0) then begin
    Error := 'Choose a direct folder on a local drive.'; exit;
  end;
  if DriveType(Copy(Path, 1, 3)) <> 3 then begin
    Error := 'Choose a folder on a fixed local drive.'; exit;
  end;
  Start := 4;
  for I := 4 to Length(Path) + 1 do begin
    if (I > Length(Path)) or (Path[I] = '\') then begin
      Part := Copy(Path, Start, I - Start);
      if (Part = '.') or (Part = '..') or ((Part = '') and (I <= Length(Path))) or
        ((Length(Part) > 0) and ((Part[Length(Part)] = '.') or (Part[Length(Part)] = ' '))) then begin
        Error := 'Choose a direct folder without relative path components.'; exit;
      end;
      Start := I + 1;
    end;
  end;
  Current := RemoveBackslashUnlessRoot(Path);
  while Current <> '' do begin
    Attributes := FileAttributes(Current);
    if Attributes = InvalidAttributes then begin
      LastError := DLLGetLastError;
      if (LastError <> 2) and (LastError <> 3) then begin
        Error := 'The installation folder could not be checked. Existing files were left in place.'; exit;
      end;
    end else if (Attributes and ReparsePoint) <> 0 then begin
      Error := 'Choose a direct folder without junctions or symbolic links. Existing files were left in place.'; exit;
    end;
    Parent := ExtractFileDir(Current);
    if SamePath(Parent, Current) then break;
    Current := Parent;
  end;
  Result := True;
end;

function InstallationRecords(Path: String; RequireComplete: Boolean; var HasRecord: Boolean; var Error: String): Boolean; forward;
function InstallationRegularPath(Path: String; AllowMissing: Boolean; var Error: String): Boolean; forward;
function InstallationFolderEmpty(Path: String; var Empty: Boolean; var Error: String): Boolean; forward;

function SuitableInstallFolder(Path: String; var Error: String): Boolean;
var
  ProtectedDir: String;
  Empty, HasRecord: Boolean;
begin
  Result := False;
  if not DirectLocalPath(Path, Error) then exit;
  if (Length(RemoveBackslashUnlessRoot(Path)) <= 3) or
    SamePath(Path, GetEnv('USERPROFILE')) or
    SamePath(Path, ExpandConstant('{localappdata}')) or
    SamePath(Path, ExpandConstant('{localappdata}\Programs')) or
    UnderOrEqual(Path, ExpandConstant('{win}')) then begin
    Error := 'Choose a separate folder for Relay program files.'; exit;
  end;
  ProtectedDir := ExpandConstant('{localappdata}\Relay');
  if UnderOrEqual(Path, ProtectedDir) or UnderOrEqual(ProtectedDir, Path) then begin
    Error := 'Program files must be separate from Relay profiles and local-server data.'; exit;
  end;
  ProtectedDir := ExpandConstant('{userappdata}\Relay');
  if UnderOrEqual(Path, ProtectedDir) or UnderOrEqual(ProtectedDir, Path) then begin
    Error := 'Program files must be separate from Relay profiles and encryption keys.'; exit;
  end;
  if not InstallationRecords(Path, False, HasRecord, Error) then exit;
  if not HasRecord and DirExists(Path) then begin
    if not InstallationFolderEmpty(Path, Empty, Error) then exit;
    if not Empty then begin
      Error := 'This folder contains other files. Choose an empty folder for Relay.'; exit;
    end;
  end;
  Result := True;
end;

function ProgramsClosed(Path: String; var Error: String): Boolean;
var
  I: Integer;
  Filename: String;
  Handle: THandle;
begin
  Result := False;
  for I := 0 to 9 do begin
    case I of
      0: Filename := 'relay.exe';
      1: Filename := 'relay-server.exe';
      2: Filename := 'livekit-server.exe';
      3: Filename := 'relay-network.exe';
      4: Filename := 'dist\relay.exe';
      5: Filename := 'dist\relay-server.exe';
      6: Filename := 'dist\livekit-server.exe';
      7: Filename := 'dist\relay-network.exe';
      8: Filename := 'relay-iroh.exe';
      9: Filename := 'dist\relay-iroh.exe';
    end;
    Filename := AddBackslash(Path) + Filename;
    if not DirectLocalPath(Filename, Error) then exit;
    if FileExists(Filename) then begin
      Handle := OpenForWrite(Filename, GenericWrite, ShareAll, 0, OpenExisting, 0, 0);
      if Handle = -1 then begin
        Error := 'Relay or its local server is still running. Close it normally, then try again. Your data is kept.'; exit;
      end;
      CloseFile(Handle);
    end;
  end;
  Result := True;
end;

function CheckInstalledPaths(Path: String; var Error: String): Boolean;
var
  Search: TFindRec;
begin
  Result := False;
  if not DirectLocalPath(AddBackslash(Path) + 'relay-installation.ini', Error) then exit;
  if not DirectLocalPath(AddBackslash(Path) + 'unins000.exe', Error) then exit;
  if not DirectLocalPath(AddBackslash(Path) + 'unins000.dat', Error) then exit;
  if not DirectLocalPath(AddBackslash(Path) + 'unins000.msg', Error) then exit;
#include Stage + "\generated-path-validation.iss"
  if FindFirst(AddBackslash(Path) + 'unins*', Search) then begin
    try
      repeat
        if not DirectLocalPath(AddBackslash(Path) + Search.Name, Error) then exit;
      until not FindNext(Search);
    finally FindClose(Search); end;
  end;
  Result := True;
end;

#include "Recovery.iss"

function AutomaticTargetReady(Path: String; var Error: String): Boolean;
var Item: TInstallationRecord; HasRecord: Boolean; I: Integer; Pending: String;
begin
  Result := False;
  if not SamePath(Path, UpdateInstallDirectory) then begin
    Error := 'The update destination changed. Relay was not installed into another folder.'; exit;
  end;
  if not InstallationRecords(Path, False, HasRecord, Error) then exit;
  if not InstallationReadRecord(AddBackslash(Path) + 'relay-installation.ini', False, Item, Error) then exit;
  if not HasRecord or not Item.Present or (Item.Version <> UpdateBaseVersion) or
     (Item.Sequence <> UpdateBaseSequence) or (Item.Hash <> UpdateBaseHash) then begin
    Error := 'The installed Relay build changed. Use the downloaded installer to update or repair Relay.'; exit;
  end;
  for I := 0 to 2 do begin
    case I of
      0: Pending := 'relay-install-pending.ini';
      1: Pending := 'relay-install-pending.tmp';
      2: Pending := 'relay-installation.tmp';
    end;
    if not InstallationRegularPath(AddBackslash(Path) + Pending, True, Error) then exit;
    if FileAttributes(AddBackslash(Path) + Pending) <> InvalidAttributes then begin
      Error := 'An earlier Relay installation needs repair. Run the downloaded installer manually.'; exit;
    end;
  end;
  Result := True;
end;

function InitializeAutomaticUpdate: Boolean;
var Image, Error: String; Size: LongWord;
begin
  Result := False;
  UpdateInstallDirectory := RemoveBackslashUnlessRoot(ExpandConstant('{param:DIR|}'));
  UpdateBaseVersion := ExpandConstant('{param:RELAYBASEVERSION|}');
  UpdateBaseSequence := ExpandConstant('{param:RELAYBASESEQUENCE|}');
  UpdateBaseHash := ExpandConstant('{param:RELAYBASEHASH|}');
  if not InstallationVersion(UpdateBaseVersion) or not InstallationSequence(UpdateBaseSequence) or
     not InstallationHex(UpdateBaseHash, 64) or (UpdateBaseSequence >= '{#ReleaseSequence}') then begin
    Log('Automatic update requires the exact earlier installed build.'); exit;
  end;
  if not DirectLocalPath(UpdateInstallDirectory, Error) then begin Log(Error); exit; end;
  Size := 32768; Image := StringOfChar(#0, Size);
  if UpdateProcessImage(UpdateParentProcess, 0, Image, Size) = 0 then begin
    Log('The running Relay installation could not be identified.'); exit;
  end;
  SetLength(Image, Size);
  if not SamePath(Image, AddBackslash(UpdateInstallDirectory) + '{#RelayClientExecutable}') then begin
    Log('The automatic update destination does not contain the exact running Relay app.'); exit;
  end;
  if not InstallationRegularPath(Image, False, Error) or
     not InstallationRegularPath(AddBackslash(UpdateInstallDirectory) + 'relay.exe', False, Error) or
     not InstallationPinAncestors(UpdateInstallDirectory, Error) or
     not AutomaticTargetReady(UpdateInstallDirectory, Error) or
     not CheckInstalledPaths(UpdateInstallDirectory, Error) then begin Log(Error); exit; end;
  Log('Automatic update is bound to the existing Relay installation.');
  Result := True;
end;

procedure InitializeServerStopChoice;
begin
  WizardForm.ReadyMemo.Height := WizardForm.ReadyMemo.Height - ScaleY(64);
  StopServerChoice := TNewCheckBox.Create(WizardForm);
  StopServerChoice.Parent := WizardForm.ReadyPage;
  StopServerChoice.Left := WizardForm.ReadyMemo.Left;
  StopServerChoice.Top := WizardForm.ReadyMemo.Top + WizardForm.ReadyMemo.Height + ScaleY(8);
  StopServerChoice.Width := WizardForm.ReadyMemo.Width;
  StopServerChoice.Height := ScaleY(20);
  StopServerChoice.Caption := 'Stop my local server and continue';
  StopServerChoice.Checked := True;
  StopServerHint := TNewStaticText.Create(WizardForm);
  StopServerHint.Parent := WizardForm.ReadyPage;
  StopServerHint.Left := StopServerChoice.Left;
  StopServerHint.Top := StopServerChoice.Top + StopServerChoice.Height + ScaleY(4);
  StopServerHint.Width := StopServerChoice.Width;
  StopServerHint.Height := ScaleY(34);
  StopServerHint.AutoSize := False;
  StopServerHint.WordWrap := True;
  StopServerHint.Caption := 'Close the Relay window first. Friends using your server will disconnect. Your accounts, messages and keys stay.';
end;

function RelayWindowClosed(Path: String; var Error: String): Boolean;
var I: Integer; Filename: String; Handle: THandle;
begin
  Result := False;
  for I := 0 to 1 do begin
    if I = 0 then Filename := AddBackslash(Path) + 'relay.exe'
    else Filename := AddBackslash(Path) + 'dist\relay.exe';
    if not DirectLocalPath(Filename, Error) then exit;
    if FileExists(Filename) then begin
      Handle := OpenForWrite(Filename, GenericWrite, ShareAll, 0, OpenExisting, 0, 0);
      if Handle = -1 then begin
        Error := 'Close the Relay window, then click Install again. Setup can stop its local server for you.'; exit;
      end;
      CloseFile(Handle);
    end;
  end;
  Result := True;
end;

function ValidateSetupRelease: String;
var Error: String;
begin
  Result := '';
  if not DirectLocalPath(ExpandConstant('{tmp}'), Error) then begin Result := Error; exit; end;
#include Stage + "\generated-validation.iss"
end;

function StopLocalServerForSetup(var Error: String): Boolean;
var Script, Binary, PowerShell, Params: String; ExitCode: Integer;
begin
  Result := False;
  if not RelayWindowClosed(ExpandConstant('{app}'), Error) then exit;
  ExtractTemporaryFile('stop-local-server.ps1');
  ExtractTemporaryFile('{#RelayStopServerPayload}');
  Script := ExpandConstant('{tmp}\stop-local-server.ps1');
  Binary := ExpandConstant('{tmp}\{#RelayStopServerPayload}');
  if not InstallationRegularPath(Script, False, Error) or
     not InstallationRegularPath(Binary, False, Error) then exit;
  if (InstallationHash(Script) <> '{#RelayStopScriptSHA256}') or
     (InstallationHash(Binary) <> '{#RelayStopServerSHA256}') then begin
    Error := 'The server stop helper could not be verified. No shutdown was requested.'; exit;
  end;
  PowerShell := ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe');
  if not DirectLocalPath(PowerShell, Error) then exit;
  Params := '-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File ' + AddQuotes(Script) +
    ' -InstallDirectory ' + AddQuotes(ExpandConstant('{app}')) + ' -StopBinary ' + AddQuotes(Binary);
  if not Exec(PowerShell, Params, '', SW_HIDE, ewWaitUntilTerminated, ExitCode) or (ExitCode <> 0) then begin
    Error := 'The local server could not finish closing. Wait a moment and click Install again. No process was killed and your data is kept.'; exit;
  end;
  Result := ProgramsClosed(ExpandConstant('{app}'), Error);
end;

function NextButtonClick(CurPageID: Integer): Boolean;
var Error: String;
begin
  Result := True;
  if (CurPageID <> wpReady) or WizardSilent then exit;
  Result := SuitableInstallFolder(ExpandConstant('{app}'), Error);
  if Result then Result := CheckInstalledPaths(ExpandConstant('{app}'), Error);
  if Result and not ProgramsClosed(ExpandConstant('{app}'), Error) then begin
    if StopServerChoice.Checked then begin
      Error := ValidateSetupRelease;
      Result := Error = '';
      if Result then Result := StopLocalServerForSetup(Error);
    end else Result := False;
  end;
  { Returning False keeps the Ready page available: close/wait and click
    Install again instead of cancelling and reopening the whole installer. }
  if not Result then MsgBox(Error, mbInformation, MB_OK);
end;

function InitializeSetup: Boolean;
begin
  Result := InstallationVersion('{#RelayVersion}');
  if not Result then Log('The installer release version is invalid. Existing files were left in place.');
  if Result then Result := InstallationAcquire;
  if Result then Result := InitializeUpdateHandoff;
end;

procedure DeinitializeSetup;
begin EndUpdateFirstShow; CloseUpdateHandoff; InstallationRelease; end;

procedure RestartUpdatedRelay;
var ResultCode: Integer;
begin
  if not UpdateRestartApproved or UpdateRestartStarted then exit;
  { Called only after InstallationFinish has verified the installed payload and
    committed records. The child still waits for this exact installer to exit. }
  UpdateRestartStarted := True;
  if not Exec(ExpandConstant('{app}\relay.exe'),
    '--wait-for-installer=' + InstallerIdentity(''),
    ExpandConstant('{app}'), SW_SHOWNORMAL, ewNoWait, ResultCode) then begin
    Log('Relay was updated but could not reopen: ' + SysErrorMessage(ResultCode));
    if not WizardSilent then
      MsgBox('Relay was updated, but could not reopen. Open Relay from the Start menu.', mbInformation, MB_OK);
  end;
end;

#if Str(RelayRegisterProtocol) == "1"
procedure RegisterInvitationProtocol;
var ResultCode: Integer;
begin
  { Run only after Finish and the committed records. Starting from [Run]
    would consume the client's bounded wait while the final page is open. }
  Log('Starting invitation registration after completed installation.');
  if not Exec(ExpandConstant('{app}\{#RelayClientExecutable}'),
    '--register-invitation-protocol --wait-for-installer=' + InstallerIdentity(''),
    ExpandConstant('{app}'), SW_HIDE, ewNoWait, ResultCode) then
    Log('Invitation registration could not start: ' + SysErrorMessage(ResultCode));
end;
#endif

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then InstallationBegin;
  if CurStep = ssDone then begin
    InstallationFinish;
#if Str(RelayRegisterProtocol) == "1"
    RegisterInvitationProtocol;
#endif
    RestartUpdatedRelay;
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Error: String;
begin
  Result := '';
  if UpdateAutomatic and not AutomaticTargetReady(ExpandConstant('{app}'), Error) then begin
    Result := Error; exit;
  end;
  if not SuitableInstallFolder(ExpandConstant('{app}'), Error) then begin Result := Error; exit; end;
  if not CheckInstalledPaths(ExpandConstant('{app}'), Error) then begin Result := Error; exit; end;
  if WizardSilent and (ExpandConstant('{param:STOPLOCALSERVER|0}') = '1') and
     not ProgramsClosed(ExpandConstant('{app}'), Error) then begin
    Result := ValidateSetupRelease;
    if Result <> '' then exit;
    if not StopLocalServerForSetup(Error) then begin Result := Error; exit; end;
  end;
  if not ProgramsClosed(ExpandConstant('{app}'), Error) then begin Result := Error; exit; end;
  Result := ValidateSetupRelease;
end;

function InitializeUninstall: Boolean;
var
  Error: String;
  HasRecord: Boolean;
begin
  Error := 'Another installer or uninstaller for this Relay installation is running.';
  Result := InstallationAcquire;
  if Result then Result := DirectLocalPath(ExpandConstant('{app}'), Error);
  if Result then Result := InstallationPinAncestors(ExpandConstant('{app}'), Error);
  InstallationEntries := 0;
  if Result then Result := InstallationCheckTree(ExpandConstant('{app}'), 0, Error);
  if Result then Result := InstallationRecords(ExpandConstant('{app}'), True, HasRecord, Error);
  if Result then Result := CheckInstalledPaths(ExpandConstant('{app}'), Error);
  if Result then Result := ProgramsClosed(ExpandConstant('{app}'), Error);
  if not Result then begin
    Log(Error);
    if not UninstallSilent then MsgBox(Error, mbError, MB_OK);
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then begin
    InstallationReleasePins;
    { Only an empty program folder may be removed. Unknown files are preserved. }
    RemoveDir(ExpandConstant('{app}\sources'));
    RemoveDir(ExpandConstant('{app}\licenses'));
    RemoveDir(ExpandConstant('{app}'));
  end;
end;

procedure DeinitializeUninstall;
begin InstallationRelease; end;
