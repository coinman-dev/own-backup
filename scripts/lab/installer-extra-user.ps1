# Boot 2 of scripts/lab/run-installer.py, in the desktop session of `lab`: the
# setup program as a person meets it. Remove what boot 1 left, install again
# through every page of the wizard, and open the window from the last page.
#
# The wizard is elevated and this script is not, so it cannot press the wizard's
# buttons (Windows does not deliver input from a lower integrity level). It asks
# the host instead: a line "HOST shot=<name> keys=<k1,k2>" makes the driver
# photograph the screen and then press those keys on the virtual keyboard. What
# this script CAN see from here is enough to know when: processes, their window
# titles, the service, the registry.
#
# ASCII only (see installer-stage.ps1). Window titles and the Cyrillic folder
# are not, and neither is the serial port: such text is sent as base64 of
# UTF-16 (INSTALLER-TEXT) and the host reads and checks it.

function Say([string]$Text) {
    try { $p = New-Object IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'; $p.Open(); $p.WriteLine($Text); $p.Close() } catch { }
}
function Check([bool]$Ok, [string]$What) {
    if (-not $Ok) { throw "check failed: $What" }
    Say "INSTALLER-OK $What"
}
# The host needs about three seconds for a photograph and a key.
function HostDo([string]$Shot, [string]$Keys = '') {
    Say "HOST shot=$Shot keys=$Keys"
    Start-Sleep -Seconds 6
}
function SayText([string]$Label, [string]$Text) {
    Say "INSTALLER-TEXT $Label $([Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($Text)))"
}
function SayTitle([string]$Label, $Process) {
    $Process.Refresh()
    SayText $Label ([string]$Process.MainWindowTitle)
}
function WaitFor([scriptblock]$Condition, [int]$Seconds, [string]$What) {
    for ($n = 0; $n -lt $Seconds * 2; $n++) {
        if (& $Condition) { return }
        Start-Sleep -Milliseconds 500
    }
    throw "timeout: $What"
}
function Windowed([string]$Name) {
    Get-Process -Name $Name -ErrorAction SilentlyContinue |
        Where-Object { $_.MainWindowHandle -ne [IntPtr]::Zero } | Select-Object -First 1
}
# The setup program and its uninstaller are found as "a window that was not
# there before": the uninstaller runs as a copy of itself under another name.
function WindowIds { @(Get-Process | Where-Object { $_.MainWindowHandle -ne [IntPtr]::Zero } | ForEach-Object { $_.Id }) }
function NewWindow([int[]]$Before) {
    Get-Process | Where-Object { $_.MainWindowHandle -ne [IntPtr]::Zero -and $Before -notcontains $_.Id } |
        Select-Object -First 1
}
function Alive([int]$Id) { [bool](Get-Process -Id $Id -ErrorAction SilentlyContinue) }
# A window started from this hidden script opens BEHIND the desktop: Windows
# lets only the foreground process put a new window in front, and this script
# is not it. (A person's double click comes from Explorer, which is.) Keys
# would then go to the desktop. Alt+Tab on the real keyboard is allowed to
# change that, so the host presses it until the window is the one in front.
function Focus($Process) {
    for ($n = 0; $n -lt 4; $n++) {
        $Process.Refresh()
        if ([Lab.Token]::GetForegroundWindow() -eq $Process.MainWindowHandle) { return }
        Say 'HOST shot= keys=alt-tab'
        Start-Sleep -Seconds 3
    }
    throw "the window of $($Process.Name) is not in front"
}
# Past the language dialog: the same process, another window.
function WaitWizard($Process) {
    WaitFor {
        $Process.Refresh()
        $Process.MainWindowHandle -ne [IntPtr]::Zero -and $Process.MainWindowTitle -ne 'Installer Language'
    } 30 'wizard window'
    Start-Sleep -Seconds 1
}

Add-Type -Namespace Lab -Name Token -MemberDefinition @'
[DllImport("kernel32.dll")] public static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
[DllImport("advapi32.dll")] public static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
[DllImport("advapi32.dll")] public static extern bool GetTokenInformation(IntPtr token, int kind, out int value, int size, out int returned);
[DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr handle);
[DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
// 1 elevated, 0 not, negative: could not ask.
public static int Elevated(int pid) {
    IntPtr process = OpenProcess(0x1000, false, pid);
    if (process == IntPtr.Zero) return -1;
    IntPtr token;
    if (!OpenProcessToken(process, 0x0008, out token)) { CloseHandle(process); return -2; }
    int value, returned;
    bool ok = GetTokenInformation(token, 20, out value, 4, out returned);
    CloseHandle(token); CloseHandle(process);
    return ok ? value : -3;
}
'@

$serviceKey = 'HKLM:\SYSTEM\CurrentControlSet\Services\OwnBackup'
$uninstallKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\OwnBackup'
$webviewKey = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
$dataDir = 'C:\ProgramData\OwnBackup'
# What the window remembers: the identifier in gui/tauri.conf.json.
$windowData = Join-Path $env:LOCALAPPDATA 'com.ownbackup.gui'
$default = Join-Path $env:ProgramFiles 'OwnBackup'
$cyrillic = 'C:\' + (-join [char[]](0x41F, 0x440, 0x43E, 0x433, 0x440, 0x430, 0x43C, 0x43C, 0x44B)) + '\Own Backup'

# Whether the window's localStorage holds a key by this name. The store is
# Chromium's LevelDB, and an ASCII key lies in it as its own bytes. Written a
# few seconds after the page asks, and held open by the WebView meanwhile.
function StorageHas([string]$Key) {
    $store = Join-Path $windowData 'EBWebView\Default\Local Storage\leveldb'
    foreach ($file in @(Get-ChildItem -LiteralPath $store -File -ErrorAction SilentlyContinue)) {
        try {
            $stream = [IO.File]::Open($file.FullName, 'Open', 'Read', 'ReadWrite, Delete')
            try {
                $bytes = New-Object byte[] $stream.Length
                $null = $stream.Read($bytes, 0, $bytes.Length)
            } finally { $stream.Close() }
        } catch { continue }
        if ([Text.Encoding]::GetEncoding(28591).GetString($bytes).Contains($Key)) { return $true }
    }
    $false
}
function ImagePath { (Get-ItemProperty $serviceKey -ErrorAction SilentlyContinue).ImagePath }
function ServiceRunning { (Get-Service OwnBackup -ErrorAction SilentlyContinue).Status -eq 'Running' }
function HasWebView { [bool]((Get-ItemProperty $webviewKey -ErrorAction SilentlyContinue).pv) }


Add-Type -Namespace StandInstaller -Name Mouse -MemberDefinition @'
[StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left,Top,Right,Bottom; }
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd,out RECT rect);
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x,int y);
[DllImport("user32.dll")] public static extern void mouse_event(uint f,uint x,uint y,uint d,UIntPtr e);
public static void DesktopCheckbox(IntPtr hwnd) { RECT r;if(!GetWindowRect(hwnd,out r))throw new Exception("setup rectangle missing");SetCursorPos(r.Left+188,r.Top+188);mouse_event(2,0,0,0,UIntPtr.Zero);mouse_event(4,0,0,0,UIntPtr.Zero); }
'@
$ErrorActionPreference='Stop'
try {
    Say 'INSTALLER-EXTRA-BEGIN'
    Say "INSTALLER-EXTRA-PROCESSES $((Get-Process | Where-Object{$_.MainWindowHandle -ne [IntPtr]::Zero} | ForEach-Object{$_.Name+':'+$_.MainWindowTitle}) -join ';')"
    $desktop=Join-Path ([Environment]::GetFolderPath('CommonDesktopDirectory')) 'Own Backup.lnk'
    $before=WindowIds
    Start-Process 'C:\Lab\own-backup-install.exe'
    WaitFor {Windowed 'own-backup-install'} 90 'English installer'
    $setup=Windowed 'own-backup-install';Focus $setup
    if($setup.MainWindowTitle -eq 'Installer Language'){HostDo 'extra-language' 'ret';WaitWizard $setup}
    SayTitle 'extra-welcome' $setup
    Check ($setup.MainWindowTitle -eq 'Own Backup Setup') 'the installer wizard caption is English'
    HostDo 'extra-welcome' 'ret'
    HostDo 'extra-license' 'ret'
    # Components precede Directory. Select the optional last item through the
    # real keyboard; the file on the public desktop is the independent proof.
    HostDo 'extra-components-before'
    [StandInstaller.Mouse]::DesktopCheckbox($setup.MainWindowHandle)
    HostDo 'extra-components-desktop'
    HostDo 'extra-components-selected'

    HostDo 'extra-components-next' 'alt-n'
    HostDo 'extra-directory' 'ret'
    WaitFor {ServiceRunning} 400 'fresh wizard install complete'
    Start-Sleep -Seconds 3
    HostDo 'extra-finish' 'ret'
    WaitFor {-not(Alive $setup.Id)} 60 'fresh wizard closed'
    Check (Test-Path $desktop) 'desktop shortcut selected in the wizard exists on the public desktop'
    Check ((Get-ItemProperty 'HKLM:\Software\OwnBackup').Language -eq '1033') 'English language was stored for future uninstall'
    $shell=New-Object -ComObject WScript.Shell;$link=$shell.CreateShortcut($desktop)
    Check ($link.TargetPath -eq (Join-Path $default 'own-backup-gui.exe')) 'the desktop shortcut targets the installed GUI'
    Get-Process own-backup-gui -ErrorAction SilentlyContinue|Stop-Process -Force
    # An update: the next page after Components must be installation, and the
    # supplied alternate directory must not become a second installation.
    $before=WindowIds
    Start-Process 'C:\Lab\own-backup-install.exe' -ArgumentList '/D=C:\MustNotBecomeAnInstall'
    WaitFor {Windowed 'own-backup-install'} 90 'upgrade wizard'
    $setup=Windowed 'own-backup-install';Focus $setup
    if($setup.MainWindowTitle -eq 'Installer Language'){HostDo 'upgrade-language' 'ret';WaitWizard $setup}
    HostDo 'upgrade-welcome' 'ret';HostDo 'upgrade-license' 'ret'
    HostDo 'upgrade-components' 'ret'
    WaitFor {ServiceRunning} 400 'upgrade service running'
    Start-Sleep -Seconds 8
    HostDo 'upgrade-after-components'
    HostDo 'upgrade-finish' 'ret'
    WaitFor {-not(Alive $setup.Id)} 60 'upgrade finished without directory confirmation'
    Check (-not(Test-Path 'C:\MustNotBecomeAnInstall')) 'updating through the wizard did not create the requested alternate directory'
    Check (Test-Path $desktop) 'updating retained the public desktop shortcut'
    Check ((ImagePath) -eq ('"'+(Join-Path $default 'own-backup-service.exe')+'"')) 'upgraded service stayed in the original install folder'
    Say 'INSTALLER-EXTRA-USER-PASS'
    [IO.File]::WriteAllText('C:\Lab\installer-extra.done','done')
    shutdown.exe /s /t 5|Out-Null
}catch{Say "INSTALLER-EXTRA-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)";shutdown.exe /s /t 5|Out-Null}
