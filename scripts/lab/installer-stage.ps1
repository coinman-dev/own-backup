# Boot 1 of scripts/lab/run-installer.py, as SYSTEM with nobody logged on: the
# silent half of the setup program's acceptance. It ends by arming boot 2,
# where the same program is clicked through by a person-shaped script.
#
# ASCII only, on purpose. PowerShell 5.1 reads a script without a byte-order
# mark in the ANSI code page, so a Cyrillic literal here would not survive; the
# one Cyrillic thing this needs, a folder name, is spelled by code point.
#
# Every check is one INSTALLER-OK line on the serial port. The first that fails
# ends the run with INSTALLER-STAGE-FAIL and the machine powers off.

function Say([string]$Text) {
    try { $p = New-Object IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'; $p.Open(); $p.WriteLine($Text); $p.Close() } catch { }
}
function Check([bool]$Ok, [string]$What) {
    if (-not $Ok) { throw "check failed: $What" }
    Say "INSTALLER-OK $What"
}

$setup = 'C:\Lab\own-backup-install.exe'
$serviceKey = 'HKLM:\SYSTEM\CurrentControlSet\Services\OwnBackup'
$uninstallKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\OwnBackup'
$webviewKey = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
$dataDir = 'C:\ProgramData\OwnBackup'
$default = Join-Path $env:ProgramFiles 'OwnBackup'
# "C:\<Programmy>\Own Backup": Cyrillic and a space, the two things a path
# handed to the service manager gets wrong first.
$cyrillic = 'C:\' + (-join [char[]](0x41F, 0x440, 0x43E, 0x433, 0x440, 0x430, 0x43C, 0x43C, 0x44B)) + '\Own Backup'
$dev = 'C:\Lab\dev'
$files = 'own-backup-gui.exe', 'own-backup-service.exe', 'own-backup-cli.exe', 'own-backup-shell.exe',
    'LICENSE.txt', 'NOTICE.txt', 'COMMERCIAL-LICENSE.txt', 'THIRD-PARTY-NOTICES.txt', 'uninstall.exe'

function ImagePath { (Get-ItemProperty $serviceKey -ErrorAction SilentlyContinue).ImagePath }
function ServiceState {
    $service = Get-Service OwnBackup -ErrorAction SilentlyContinue
    if ($service) { [string]$service.Status } else { 'absent' }
}
function WaitState([string]$Want, [int]$Seconds = 60) {
    for ($n = 0; $n -lt $Seconds * 2; $n++) {
        if ((ServiceState) -eq $Want) { return $true }
        Start-Sleep -Milliseconds 500
    }
    $false
}
function ServiceProcessPath {
    $service = Get-CimInstance Win32_Service -Filter "Name='OwnBackup'"
    if ($service -and $service.ProcessId) { (Get-Process -Id $service.ProcessId).Path }
}
function Setup([string]$Arguments) {
    (Start-Process $setup -ArgumentList $Arguments -Wait -PassThru).ExitCode
}
function Tool([string]$Exe, [string]$Argument) {
    $null = cmd.exe /d /c "`"$Exe`" $Argument 2>&1"
    $LASTEXITCODE
}

function CheckInstalled([string]$Dir, [string]$Label) {
    $missing = @($files | Where-Object { -not (Test-Path -LiteralPath (Join-Path $Dir $_)) })
    Check ($missing.Count -eq 0) "$Label files present (missing: $($missing -join ','))"
    $exe = Join-Path $Dir 'own-backup-service.exe'
    # Quoted: an unquoted "C:\Program Files\..." lets C:\Program.exe run as SYSTEM.
    Check ((ImagePath) -eq ('"' + $exe + '"')) "$Label service registered from the installed copy, path quoted"
    $config = Get-ItemProperty $serviceKey
    Check ($config.Start -eq 2 -and $config.ObjectName -eq 'LocalSystem') "$Label service is automatic and LocalSystem"
    Check (WaitState 'Running') "$Label service running"
    Check ((ServiceProcessPath) -eq $exe) "$Label running process is the installed copy"
    $entry = Get-ItemProperty $uninstallKey
    Check ($entry.DisplayName -eq 'Own Backup' -and $entry.DisplayVersion -eq $version -and
        $entry.InstallLocation -eq $Dir -and $entry.UninstallString -eq ('"' + (Join-Path $Dir 'uninstall.exe') + '"')) `
        "$Label Programs-and-Features entry, version $version"
    $link = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'Own Backup\Own Backup.lnk'
    Check (Test-Path -LiteralPath $link) "$Label Start menu shortcut for all users"
    $target = (New-Object -ComObject WScript.Shell).CreateShortcut($link).TargetPath
    Check ($target -eq (Join-Path $Dir 'own-backup-gui.exe')) "$Label shortcut opens the installed window"
    Check (-not (Test-Path -LiteralPath (Join-Path ([Environment]::GetFolderPath('CommonDesktopDirectory')) 'Own Backup.lnk'))) `
        "$Label no desktop shortcut unless asked for"
}

# The uninstaller hands over to a copy of itself in TEMP and returns at once,
# so its exit says nothing; the folder and the registry entry going do.
function Uninstall([string]$Dir) {
    Start-Process (Join-Path $Dir 'uninstall.exe') -ArgumentList '/S' | Out-Null
    for ($n = 0; $n -lt 720; $n++) {
        if (-not (Test-Path -LiteralPath $Dir) -and -not (Test-Path $uninstallKey)) { return }
        Start-Sleep -Milliseconds 500
    }
    $left = @(Get-ChildItem -LiteralPath $Dir -Recurse -Force -ErrorAction SilentlyContinue | ForEach-Object { $_.Name })
    throw "silent uninstall left the folder or the registry entry (in the folder: $($left -join ','))"
}

$ErrorActionPreference = 'Stop'
try {
    if (Test-Path 'C:\Lab\installer-stage.ready') { Say 'INSTALLER-SECOND-BOOT'; return }
    $version = (Get-Content 'C:\Lab\installer-version.txt' -Raw).Trim()
    $os = Get-CimInstance Win32_OperatingSystem
    Say "INSTALLER-PRE whoami=$(whoami) build=$($os.BuildNumber) ui=$((Get-UICulture).Name) acp=$([Text.Encoding]::Default.CodePage)"
    Say "INSTALLER-PRE service=$(ServiceState) image=$(ImagePath)"
    $hasWebView = [bool]((Get-ItemProperty $webviewKey -ErrorAction SilentlyContinue).pv)
    Say "INSTALLER-PRE webview2=$hasWebView setup-sha256=$((Get-FileHash $setup -Algorithm SHA256).Hash)"

    # 1. Over whatever the laboratory image had (its service runs from C:\Lab).
    Check ((Setup '/S') -eq 0) 'T1 silent install exits 0'
    CheckInstalled $default 'T1'
    $installedBuild = (Get-FileHash (Join-Path $default 'own-backup-service.exe')).Hash

    # 2. Again: an update stays where the program is, whatever /D= says.
    Check ((Setup '/S /D=C:\ObkElsewhere') -eq 0) 'T2 silent update exits 0'
    CheckInstalled $default 'T2'
    Check (-not (Test-Path 'C:\ObkElsewhere')) 'T2 an update does not follow /D= to another folder'

    # 3. Removal, silent: the settings stay.
    Check (Test-Path -LiteralPath $dataDir) 'T3 the service made its data folder'
    Uninstall $default
    Check ((ServiceState) -eq 'absent' -and -not (ImagePath)) 'T3 service removed'
    Check (-not (Test-Path -LiteralPath (Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'Own Backup'))) 'T3 Start menu folder removed'
    Check (Test-Path -LiteralPath $dataDir) 'T3 a silent removal keeps the settings'

    # 4. A developer's copy registered from its own folder, then the setup
    #    program into a Cyrillic path: the registration has to move.
    New-Item -ItemType Directory -Force $dev | Out-Null
    $devExe = Join-Path $dev 'own-backup-service.exe'
    Copy-Item 'C:\Lab\own-backup-service-dev.exe' $devExe -Force
    Check ((Get-FileHash $devExe).Hash -eq $installedBuild) 'T4 the developer copy is the build the setup program carries'
    Check ((Tool $devExe '/install') -eq 0) 'T4 /install registers'
    Check ((Tool $devExe '/install') -eq 0) 'T4 /install on a registered service succeeds too'
    Check ((Tool $devExe '/start') -eq 0 -and (WaitState 'Running')) 'T4 the developer copy runs'
    Check ((ServiceProcessPath) -eq $devExe) 'T4 and it is the one running'
    Check ((Setup "/S /D=$cyrillic") -eq 0) 'T4 silent install into a Cyrillic folder exits 0'
    CheckInstalled $cyrillic 'T4'
    Check (Test-Path -LiteralPath $devExe) 'T4 the developer copy itself is left alone'

    # 5. The reverse: the registration belongs to the developer copy again, and
    #    removing the installed program must not take that service with it.
    Check ((Tool $devExe '/stop') -eq 0) 'T5 service stopped'
    Check ((Tool $devExe '/install') -eq 0) 'T5 registration moved back to the developer copy'
    Uninstall $cyrillic
    # No quotes this time: the service quotes a path only when it has to, and
    # this one has no space in it.
    Say "INSTALLER-T5 image=$(ImagePath)"
    Check (([string](ImagePath)).Trim('"') -eq $devExe) 'T5 a service registered elsewhere survives the removal'
    Check ((Tool $devExe '/uninstall') -eq 0 -and (ServiceState) -eq 'absent') 'T5 developer copy unregistered'

    # 6. What boot 2 starts from: installed in the Cyrillic folder, to see the
    #    service come up from there by itself after a reboot.
    Check ((Setup "/S /D=$cyrillic") -eq 0) 'T6 silent install exits 0'
    CheckInstalled $cyrillic 'T6'

    # Boot 2 is the person: an administrator whose programs are NOT elevated
    # until they ask, as on any home machine -- but with the consent prompt
    # answered for them, since nobody is there to click it.
    $admins = ([Security.Principal.SecurityIdentifier]'S-1-5-32-544').Translate([Security.Principal.NTAccount]).Value.Split('\')[-1]
    if (-not @(Get-LocalGroupMember $admins | Where-Object { $_.Name -like '*\lab' }).Count) { Add-LocalGroupMember $admins 'lab' }
    $policy = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System'
    Set-ItemProperty $policy ConsentPromptBehaviorAdmin 0 -Type DWord
    Set-ItemProperty $policy PromptOnSecureDesktop 0 -Type DWord
    $startup = [Environment]::GetFolderPath('CommonStartup')
    Get-ChildItem $startup -Filter 'OwnBackup*.cmd' | Remove-Item -Force
    [IO.File]::WriteAllText((Join-Path $startup 'OwnBackupInstaller.cmd'),
        "@echo off`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\Lab\installer-user.ps1`r`n")
    $answer = New-Object Xml.XmlDocument; $answer.Load('C:\Lab\autounattend.xml')
    $password = $answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $winlogon DefaultUserName 'lab'; Set-ItemProperty $winlogon DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $winlogon DefaultPassword $password; Set-ItemProperty $winlogon AutoAdminLogon '1'; Set-ItemProperty $winlogon AutoLogonCount 1
    [IO.File]::WriteAllText('C:\Lab\installer-stage.ready', 'ready')
    Say 'INSTALLER-STAGE-READY REBOOT'; shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "INSTALLER-STAGE-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
