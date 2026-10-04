function Say([string]$Text) {
    try { $p=New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One'; $p.Open(); $p.WriteLine($Text); $p.Close() } catch { }
}
$ErrorActionPreference='Stop'
try {
    if (Test-Path 'C:\Lab\rustsec-stage.ready') {
        Start-Process powershell.exe -ArgumentList '-NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-rustsec-runtime.ps1' -WindowStyle Hidden | Out-Null
        Say 'RUSTSEC-SECOND-BOOT'; return
    }
    New-Item -ItemType Directory -Force 'C:\Lab\rustsec-results' | Out-Null
    $null=& icacls.exe 'C:\Lab\rustsec-results' /grant '*S-1-5-4:(OI)(CI)M'
    if ($LASTEXITCODE -ne 0) { throw 'result ACL failed' }
    $output=cmd.exe /d /c 'C:\Lab\own-backup-service-rustsec.exe /update 2>&1' | Out-String
    if ($LASTEXITCODE -ne 0) { throw "service update failed: $output" }
    $admins=([Security.Principal.SecurityIdentifier]'S-1-5-32-544').Translate([Security.Principal.NTAccount]).Value.Split('\')[-1]
    if (@(Get-LocalGroupMember $admins | Where-Object { $_.Name -like '*\lab' }).Count) { Remove-LocalGroupMember $admins "$env:COMPUTERNAME\lab" }
    $startup=[Environment]::GetFolderPath('CommonStartup')
    Get-ChildItem $startup -Filter 'OwnBackup*.cmd' | Remove-Item -Force
    [IO.File]::WriteAllText((Join-Path $startup 'OwnBackupRustSec.cmd'),"@echo off`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-rustsec-user.ps1`r`n")
    $answer=New-Object Xml.XmlDocument; $answer.Load('C:\Lab\autounattend.xml')
    $password=$answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    $winlogon='HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $winlogon DefaultUserName 'lab'; Set-ItemProperty $winlogon DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $winlogon DefaultPassword $password; Set-ItemProperty $winlogon AutoAdminLogon '1'; Set-ItemProperty $winlogon AutoLogonCount 1
    [IO.File]::WriteAllText('C:\Lab\rustsec-stage.ready','ready')
    Say 'RUSTSEC-STAGE-READY REBOOT'; shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "RUSTSEC-STAGE-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
