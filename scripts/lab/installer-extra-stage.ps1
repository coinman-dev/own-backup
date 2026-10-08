function Say([string]$text){try{$p=New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One';$p.Open();$p.WriteLine($text);$p.Close()}catch{}}
$ErrorActionPreference='Stop'
try {
    if(Test-Path 'C:\Lab\installer-extra.setup'){Say 'INSTALLER-EXTRA-WAITING';return}
    # Only the disposable VM; remove the developer service registration so the
    # first wizard is an actual fresh installation.
    & 'C:\Lab\own-backup-service.exe' /stop |Out-Null
    & 'C:\Lab\own-backup-service.exe' /uninstall |Out-Null
    Remove-Item 'HKLM:\Software\OwnBackup' -Recurse -Force -ErrorAction SilentlyContinue
    New-Item 'HKLM:\Software\OwnBackup' -Force|Out-Null
    New-ItemProperty 'HKLM:\Software\OwnBackup' -Name Language -Value '1033' -PropertyType String -Force|Out-Null
    Get-ChildItem ([Environment]::GetFolderPath('CommonStartup')) -Filter 'OwnBackup*.cmd'|Remove-Item -Force
    if(-not(Test-Path 'C:\Program Files (x86)\Microsoft\EdgeWebView\Application')){& 'C:\Lab\WebView2Runtime-x64.exe' /silent /install|Out-Null}
    $answer=New-Object Xml.XmlDocument;$answer.Load('C:\Lab\autounattend.xml')
    $password=$answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    $key='HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $key DefaultUserName 'lab';Set-ItemProperty $key DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $key DefaultPassword $password;Set-ItemProperty $key AutoAdminLogon '1';Set-ItemProperty $key AutoLogonCount 1
    New-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\RunOnce' -Name OwnBackupInstallerExtra -PropertyType String -Force -Value 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\Lab\installer-extra-user.ps1'|Out-Null
    & powercfg.exe /h off|Out-Null
    [IO.File]::WriteAllText('C:\Lab\installer-extra.setup','ready')
    Say 'INSTALLER-EXTRA-REBOOT';shutdown.exe /r /t 5|Out-Null
}catch{Say "INSTALLER-EXTRA-FAIL setup line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)";shutdown.exe /s /t 5|Out-Null}
