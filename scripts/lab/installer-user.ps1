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

$ErrorActionPreference = 'Stop'
try {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $elevated = [Security.Principal.WindowsPrincipal]::new($identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    Say "INSTALLER-USER-BEGIN identity=$($identity.Name) elevated=$elevated ui=$((Get-UICulture).Name) webview2=$(HasWebView)"
    Check (-not $elevated) 'the person at the desk is not elevated'
    # As on a computer that never had the program: nothing the window of an
    # earlier copy remembered. This folder outlives an installation -- a theme
    # chosen in a copy run from another folder came back after installing.
    Remove-Item -LiteralPath $windowData -Recurse -Force -ErrorAction SilentlyContinue
    # The window leaves this file when its page is up; an old one proves nothing.
    $marker = Join-Path ([IO.Path]::GetTempPath()) 'own-backup-gui-up'
    Remove-Item $marker -Force -ErrorAction SilentlyContinue

    # (a) Nobody started it: the service came up with Windows, from a folder
    #     whose name is Cyrillic and has a space in it.
    #     Which file a LocalSystem process runs is not shown to an account
    #     that is not elevated (boot 1 looked, as SYSTEM); what the service
    #     manager was told to start is, and it did start it.
    WaitFor { ServiceRunning } 180 'service running after the reboot'
    SayText 'service-after-reboot' ([string](ImagePath))
    Check (([string](ImagePath)).Trim('"') -eq (Join-Path $cyrillic 'own-backup-service.exe')) 'after a reboot the service runs by itself from the Cyrillic folder'

    # (b) Removal, by hand.
    $before = WindowIds
    Start-Process (Join-Path $cyrillic 'uninstall.exe')
    WaitFor { NewWindow $before } 90 'uninstaller window'
    Start-Sleep -Seconds 2
    $remover = NewWindow $before
    Say "INSTALLER-REMOVER process=$($remover.Name)"
    Focus $remover
    # This copy was installed silently, and a silent run has no wizard page to
    # save the language on: its uninstaller used to open in English with
    # "Please select a language". The setup program now writes it itself.
    Check ($remover.MainWindowTitle -ne 'Installer Language') 'the uninstaller of a silently installed copy does not ask for a language'
    SayTitle 'uninstall' $remover
    HostDo 'uninstall-1-confirm' 'ret'
    # The service is gone: what is on the screen now is the question about the
    # settings, asked while the program's files are still there (deleting the
    # settings is the service executable's work). Enter is its default, No.
    WaitFor { -not (ImagePath) } 400 'service removed'
    Start-Sleep -Seconds 3
    HostDo 'uninstall-2-keep-settings' 'ret'
    WaitFor { -not (Test-Path -LiteralPath (Join-Path $cyrillic 'own-backup-gui.exe')) } 120 'program files removed'
    Start-Sleep -Seconds 2
    HostDo 'uninstall-3-finished' 'ret'
    WaitFor { -not (Alive $remover.Id) } 60 'uninstaller closed'
    WaitFor { -not (Test-Path -LiteralPath $cyrillic) } 30 'program folder removed'
    Check (-not (Test-Path $uninstallKey)) 'removed by hand: nothing left in Programs and Features'
    Check (Test-Path -LiteralPath $dataDir) 'answering No keeps the settings'

    # (c) Installation, by hand, every page.
    $before = WindowIds
    Start-Process 'C:\Lab\own-backup-install.exe'
    WaitFor { NewWindow $before } 90 'setup window'
    Start-Sleep -Seconds 2
    $setup = NewWindow $before
    Say "INSTALLER-SETUP process=$($setup.Name) elevated=$([Lab.Token]::Elevated($setup.Id))"
    Focus $setup
    SayTitle 'language' $setup
    HostDo 'install-1-language' 'ret'
    WaitWizard $setup
    SayTitle 'welcome' $setup
    HostDo 'install-2-welcome' 'ret'
    HostDo 'install-3-license' 'ret'
    HostDo 'install-4-components' 'ret'
    HostDo 'install-5-folder' 'ret'
    WaitFor { (ServiceRunning) -and (Test-Path $uninstallKey) } 400 'installed and the service running'
    Start-Sleep -Seconds 3
    SayTitle 'installed' $setup
    if (-not (HasWebView)) {
        # Yes would open a browser on a machine with no network. Tab to No.
        HostDo 'install-6-no-webview2' 'tab,ret'
    }
    HostDo 'install-7-finish' 'ret'
    WaitFor { -not (Alive $setup.Id) } 60 'setup closed'
    Check ((ImagePath) -eq ('"' + (Join-Path $default 'own-backup-service.exe') + '"')) 'installed by hand into the folder the wizard offered'
    Check ((Get-ItemProperty 'HKLM:\SOFTWARE\OwnBackup').Language -eq '1049') 'the wizard ran in Russian and remembered it for the uninstaller'

    # The last page starts the window, as the person, not as the installer.
    WaitFor { Windowed 'own-backup-gui' } 90 'window started from the last page'
    $gui = Windowed 'own-backup-gui'
    Check ([Lab.Token]::Elevated($gui.Id) -eq 0) 'the window started from the last page is not elevated'
    Check ($gui.Path -eq (Join-Path $default 'own-backup-gui.exe')) 'and it is the installed one'
    if (-not (HasWebView)) {
        # No runtime: the window says so itself, in a native dialog.
        Start-Sleep -Seconds 3
        HostDo 'gui-without-webview2'
        $null = $gui.CloseMainWindow()
        WaitFor { $gui.Refresh(); $gui.HasExited } 30 'runtime diagnosis closed'
        $runtime = Start-Process 'C:\Lab\WebView2Runtime-x64.exe' -ArgumentList '/silent /install' -Verb RunAs -PassThru -Wait
        Check ($runtime.ExitCode -eq 0 -and (HasWebView)) 'WebView2 runtime installed'
        Remove-Item $marker -Force -ErrorAction SilentlyContinue
        Start-Process (Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'Own Backup\Own Backup.lnk')
    }
    WaitFor { (Test-Path $marker) -and (Windowed 'own-backup-gui') } 180 'window rendered'
    Start-Sleep -Seconds 8
    $gui = Windowed 'own-backup-gui'
    Check ($gui.MainWindowTitle -eq 'Own Backup') 'the window opened from the Start menu'
    HostDo 'gui'

    # What the window does for the person the first time it runs.
    $run = (Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run').OwnBackupTray
    Check ($run -eq ('"' + (Join-Path $default 'own-backup-gui.exe') + '" --tray')) 'the window registered the icon beside the clock for this person'
    Check (@(Get-Process own-backup-gui).Count -ge 2) 'the icon has its own process'
    $status = cmd.exe /d /c "`"$(Join-Path $default 'own-backup-cli.exe')`" jobs status 2>&1" | Out-String
    Check ($LASTEXITCODE -eq 0 -and $status.Contains('uptime_secs')) 'the command line reaches the installed service'

    # A first start pins no theme. The list on the first page saves its sort
    # order as soon as it is shown -- that key proves this is the right store
    # and that it has been written; the theme's key must not be beside it.
    WaitFor { StorageHas 'ob.backup.sortKey' } 120 'the window wrote to its storage'
    Check (-not (StorageHas 'ownbackup.theme')) 'a first start saves no theme: a clean profile follows Windows'
    # For eyes: six Tabs reach "Settings" in the side strip.
    Focus $gui
    Say 'HOST shot= keys=tab,tab,tab,tab,tab,tab,ret'
    Start-Sleep -Seconds 12
    HostDo 'gui-settings'

    # (d) Removal once more, this time WITH the settings, the window still
    #     open: the uninstaller closes it and the icon by itself.
    $before = WindowIds
    Start-Process (Join-Path $default 'uninstall.exe')
    WaitFor { NewWindow $before } 90 'uninstaller window'
    Start-Sleep -Seconds 2
    $remover = NewWindow $before
    Focus $remover
    HostDo 'uninstall-4-confirm' 'ret'
    WaitFor { -not (ImagePath) } 400 'service removed'
    Start-Sleep -Seconds 3
    # The question again. Tab moves from No, its default, to Yes.
    HostDo 'uninstall-5-delete-settings' 'tab,ret'
    WaitFor { -not (Test-Path -LiteralPath (Join-Path $default 'own-backup-gui.exe')) } 120 'program files removed'
    Start-Sleep -Seconds 2
    HostDo 'uninstall-6-finished' 'ret'
    WaitFor { -not (Alive $remover.Id) } 60 'uninstaller closed'
    Check (-not (Get-Process own-backup-gui -ErrorAction SilentlyContinue)) 'the uninstaller closed the window and the icon'
    Check (-not (Test-Path -LiteralPath $default)) 'removed with the window open: the program folder is gone'
    Check (-not (Test-Path -LiteralPath $dataDir)) 'answering Yes deletes the settings'
    Check (-not (Test-Path -LiteralPath $windowData)) 'and what the window remembered'
    Say 'INSTALLER-PASS'
} catch {
    Say "INSTALLER-USER-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    # Which windows there were: half of what can go wrong here is "the window
    # this was waiting for belongs to a process called something else".
    SayText 'windows-at-failure' ((Get-Process | Where-Object { $_.MainWindowHandle -ne [IntPtr]::Zero } |
        ForEach-Object { $_.Name + '=' + $_.MainWindowTitle }) -join ' | ')
    HostDo 'failure'
} finally {
    Say 'INSTALLER-DONE'
    shutdown.exe /s /t 5 | Out-Null
}
