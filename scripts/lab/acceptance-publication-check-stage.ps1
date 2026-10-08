. 'C:\Lab\acceptance-common.ps1'
$work='C:\Lab\publication-check'
try {
    if(Test-Path "$work\prepared.ready"){Say 'PUBLICATION-CHECK-WAIT';return}
    Check (Test-Path 'C:\Lab\service-kill\service-kill-result.json') 'the parent is the accepted live-upgrade fixture'
    Check ((Get-Volume -DriveLetter T).FileSystemLabel -eq 'OBKLIVEARCH') 'only synthetic archive storage is attached'
    New-Item -ItemType Directory $work|Out-Null
    $answer=New-Object Xml.XmlDocument;$answer.Load('C:\Lab\autounattend.xml')
    $password=$answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    Check ([bool]$password) 'disposable console credentials available'
    $key='HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $key DefaultUserName 'lab';Set-ItemProperty $key DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $key DefaultPassword $password;Set-ItemProperty $key AutoAdminLogon '1';Set-ItemProperty $key AutoLogonCount 1
    $action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\Lab\acceptance-publication-check-user.ps1'
    $trigger=New-ScheduledTaskTrigger -AtLogOn -User "$env:COMPUTERNAME\lab"
    $principal=New-ScheduledTaskPrincipal -UserId "$env:COMPUTERNAME\lab" -LogonType Interactive -RunLevel Highest
    Register-ScheduledTask -TaskName OwnBackupPublicationCheck -Action $action -Trigger $trigger -Principal $principal -Force|Out-Null
    [IO.File]::WriteAllText("$work\prepared.ready",'ready')
    Say 'PUBLICATION-CHECK-REBOOT';shutdown.exe /r /t 5|Out-Null
}catch{Say "PUBLICATION-CHECK-FAIL setup line=$($_.InvocationInfo.ScriptLineNumber) reason=$($_.Exception.Message)";shutdown.exe /s /t 5|Out-Null}
