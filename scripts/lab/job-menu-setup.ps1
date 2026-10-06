# Boot 1: update the service as SYSTEM, then enter the active console. The
# production pipe rejects session-0 clients, so the CLI acceptance runs under
# the logged-in lab user on boot 2, before the GUI acceptance on boot 3.
# ASCII only: PowerShell 5.1 reads BOM-less scripts in the ANSI code page.
function Say([string]$Text) {
    try { $p = New-Object IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'; $p.Open(); $p.WriteLine($Text); $p.Close() } catch { }
}

$ErrorActionPreference = 'Stop'
$ready = 'C:\Lab\job-menu-setup.ready'
try {
    $staged = 'C:\Lab\own-backup-service-menu.exe'
    $want = (Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash
    if ((Get-FileHash 'C:\Lab\own-backup-service.exe' -Algorithm SHA256).Hash -ne $want) {
        $output = cmd.exe /d /c "$staged /update 2>&1" | Out-String
        if ($LASTEXITCODE -ne 0) { throw "service update failed: $output" }
    }
    if ((Get-FileHash 'C:\Lab\own-backup-service.exe' -Algorithm SHA256).Hash -ne $want) {
        throw 'installed service hash mismatch'
    }
    Say 'MENU-OK the service under test is the one installed'
    if (Test-Path -LiteralPath $ready) { Say 'MENU-SETUP-ALREADY-READY'; return }
    $startup = [Environment]::GetFolderPath('CommonStartup')
    Get-ChildItem $startup -Filter 'OwnBackup*.cmd' | Remove-Item -Force
    New-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\RunOnce' `
        -Name OwnBackupJobMenuStage -PropertyType String -Force `
        -Value 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\Lab\job-menu-stage.ps1' | Out-Null
    $answer = New-Object Xml.XmlDocument; $answer.Load('C:\Lab\autounattend.xml')
    $password = $answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    if (-not $password) { throw 'disposable lab password missing' }
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $winlogon DefaultUserName 'lab'; Set-ItemProperty $winlogon DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $winlogon DefaultPassword $password; Set-ItemProperty $winlogon AutoAdminLogon '1'; Set-ItemProperty $winlogon AutoLogonCount 1
    [IO.File]::WriteAllText($ready, 'ready')
    Say 'MENU-SETUP-READY REBOOT'; shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "MENU-SETUP-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
