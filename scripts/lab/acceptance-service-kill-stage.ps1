. 'C:\Lab\acceptance-common.ps1'
$work='C:\Lab\service-kill'
try {
    if(Test-Path "$work\prepared.ready"){Say 'SERVICE-KILL-WAIT';return}
    Check (Test-Path 'C:\Lab\installer-live\installer-live-result.json') 'the parent is the accepted live-upgrade fixture'
    Check ((Get-Volume -DriveLetter T).FileSystemLabel -eq 'OBKLIVEARCH') 'only synthetic archive storage is attached'
    New-Item -ItemType Directory $work|Out-Null
    if(Test-Path 'C:\Lab\own-backup-service-fixed.exe') {
        $installed='C:\Program Files\OwnBackup\own-backup-service.exe'
        $null=Native $installed @('/stop')
        Copy-Item 'C:\Lab\own-backup-service-fixed.exe' $installed -Force
        Check ((Get-FileHash $installed).Hash -eq (Get-FileHash 'C:\Lab\own-backup-service-fixed.exe').Hash) 'the fixed service was deployed only to the disposable guest'
        $null=Native $installed @('/start')
        Report 'fixed-service-deployment' @{sha256=(Get-FileHash $installed).Hash;path=$installed}
    }
    $answer=New-Object Xml.XmlDocument;$answer.Load('C:\Lab\autounattend.xml')
    $password=$answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    Check ([bool]$password) 'disposable console credentials available'
    $key='HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $key DefaultUserName 'lab';Set-ItemProperty $key DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $key DefaultPassword $password;Set-ItemProperty $key AutoAdminLogon '1';Set-ItemProperty $key AutoLogonCount 1
    $action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\Lab\acceptance-service-kill-user.ps1'
    $trigger=New-ScheduledTaskTrigger -AtLogOn -User "$env:COMPUTERNAME\lab"
    $principal=New-ScheduledTaskPrincipal -UserId "$env:COMPUTERNAME\lab" -LogonType Interactive -RunLevel Highest
    Register-ScheduledTask -TaskName OwnBackupServiceKill -Action $action -Trigger $trigger -Principal $principal -Force|Out-Null
    [IO.File]::WriteAllText("$work\prepared.ready",'ready')
    Say 'SERVICE-KILL-REBOOT';shutdown.exe /r /t 5|Out-Null
}catch{Say "SERVICE-KILL-FAIL setup line=$($_.InvocationInfo.ScriptLineNumber) reason=$($_.Exception.Message)";shutdown.exe /s /t 5|Out-Null}
