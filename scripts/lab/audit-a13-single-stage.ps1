# SYSTEM startup task in a disposable VM: one automatic standard-user logon
# to verify that Own Backup refuses a second GUI instance.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

$ErrorActionPreference = 'Stop'
try {
    if (Test-Path 'C:\Lab\audit-a13-single.ready') { Say 'A13-SINGLE-SECOND-BOOT'; return }
    Say "A13-SINGLE-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    $answer = New-Object System.Xml.XmlDocument
    $answer.Load('C:\Lab\autounattend.xml')
    $password = $answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty -Path $winlogon -Name DefaultUserName -Value 'lab'
    Set-ItemProperty -Path $winlogon -Name DefaultDomainName -Value $env:COMPUTERNAME
    Set-ItemProperty -Path $winlogon -Name DefaultPassword -Value $password
    Set-ItemProperty -Path $winlogon -Name AutoAdminLogon -Value '1'
    Set-ItemProperty -Path $winlogon -Name AutoLogonCount -Value 1
    $startup = [Environment]::GetFolderPath('CommonStartup')
    Remove-Item -LiteralPath (Join-Path $startup 'OwnBackupAudit29.cmd') -Force -ErrorAction SilentlyContinue
    [IO.File]::WriteAllText((Join-Path $startup 'OwnBackupA13Single.cmd'),
        "@echo off`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-a13-single-user.ps1`r`n")
    [IO.File]::WriteAllText('C:\Lab\audit-a13-single.ready', 'ready')
    Say 'A13-SINGLE-READY REBOOT'
    shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "A13-SINGLE-STAGE-FAIL $($_.Exception.Message)"
}
