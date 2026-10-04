function Say([string]$Text) {
    try { $p=New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One';$p.Open();$p.WriteLine($Text);$p.Close() } catch { }
}
$ErrorActionPreference='Stop'
try {
    if (Test-Path 'C:\Lab\iso-stage.ready') {
        Start-Process powershell.exe -ArgumentList '-NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-h03-iso-watch.ps1' -WindowStyle Hidden | Out-Null
        Say 'H03-ISO-SECOND-BOOT';return
    }
    Say 'H03-ISO-PREPARE'
    New-Item -ItemType Directory -Force 'C:\Lab\iso-results' | Out-Null
    $base='C:\Recovery\WindowsRE\Winre.wim'
    [IO.File]::WriteAllText('C:\Lab\iso-results\base-before.json',(@{base=$base;sha256=(Get-FileHash $base -Algorithm SHA256).Hash} | ConvertTo-Json))
    $null=& icacls.exe 'C:\Lab\iso-results' /grant '*S-1-5-4:(OI)(CI)M'
    if ($LASTEXITCODE -ne 0) { throw 'fixture ACL failed' }
    $installer='C:\Lab\WebView2Runtime-x64.exe'
    if ((Get-FileHash $installer -Algorithm SHA256).Hash -ne 'FA07BC67C2A68763E127F6AABF5622B40A0220228899E78DAF02CE2CC3971A50') { throw 'runtime installer hash differs' }
    if ((Get-AuthenticodeSignature $installer).Status -ne 'Valid') { throw 'runtime signature invalid' }
    $p=Start-Process $installer -ArgumentList '/silent /install' -PassThru -Wait
    if ($p.ExitCode -ne 0) { throw "runtime install exit=$($p.ExitCode)" }
    $output=cmd.exe /d /c 'C:\Lab\own-backup-service-iso.exe /update 2>&1' | Out-String
    if ($LASTEXITCODE -ne 0) { throw "service update failed: $output" }
    $admins=([Security.Principal.SecurityIdentifier]'S-1-5-32-544').Translate([Security.Principal.NTAccount]).Value.Split('\')[-1]
    if (@(Get-LocalGroupMember $admins | Where-Object { $_.Name -like '*\lab' }).Count) { Remove-LocalGroupMember $admins "$env:COMPUTERNAME\lab" }
    $startup=[Environment]::GetFolderPath('CommonStartup')
    Get-ChildItem $startup -Filter 'OwnBackup*.cmd' | Remove-Item -Force
    [IO.File]::WriteAllText((Join-Path $startup 'OwnBackupIso.cmd'),"@echo off`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-h03-iso-user.ps1`r`n")
    $answer=New-Object Xml.XmlDocument;$answer.Load('C:\Lab\autounattend.xml')
    $password=$answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    $key='HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $key DefaultUserName 'lab';Set-ItemProperty $key DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $key DefaultPassword $password;Set-ItemProperty $key AutoAdminLogon '1';Set-ItemProperty $key AutoLogonCount 1
    [IO.File]::WriteAllText('C:\Lab\iso-stage.ready','ready')
    Say 'H03-ISO-PREPARED REBOOT';shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "H03-ISO-STAGE-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
