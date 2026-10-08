# SYSTEM on a disposable Windows disk and a new one-GiB data disk.
. 'C:\Lab\acceptance-common.ps1'
$work='C:\Lab\installer-live'
function WaitReady {
    for($i=0;$i -lt 60;$i++) {
        try {$r=Rpc 'live-ready' @{op='status'};if($r){return}}catch{}
        Start-Sleep -Seconds 1
    }
    throw 'installed service not ready'
}
function Install([string]$exe) {
    $p=Start-Process $exe -ArgumentList '/S' -PassThru
    $null=$p.Handle
    if(-not $p.WaitForExit(360000)){$p.Kill();throw 'installer timeout'}
    $p.Refresh();Check ($p.ExitCode -eq 0) 'installer returned success'
}
try {
    if(Test-Path "$work\desktop-prepared.ready"){Say 'INSTALLER-LIVE-WAIT';return}
    if(Test-Path $work){throw 'previous live-installer attempt must be preserved'}
    New-Item -ItemType Directory $work | Out-Null
    $service=Get-Service OwnBackup -ErrorAction SilentlyContinue
    if($service){$null=Native 'C:\Lab\own-backup-service.exe' @('/stop');$null=Native 'C:\Lab\own-backup-service.exe' @('/uninstall')}
    # The copied fixture's old schedules refer to detached synthetic disks.
    $path='C:\ProgramData\OwnBackup\jobs.json'
    $jobs=(Get-Content $path -Raw -Encoding UTF8|ConvertFrom-Json).PSObject.BaseObject
    foreach($job in $jobs){$job.schedule.kind='none';$job.verification.schedule.kind='none'}
    JsonFile $path $jobs
    $disks=@(Get-Disk|Where-Object{$_.Size -eq 1GB -and $_.PartitionStyle -eq 'RAW' -and -not $_.IsBoot -and -not $_.IsSystem})
    Check ($disks.Count -eq 1 -and -not(Test-Path 'S:\') -and -not(Test-Path 'V:\') -and -not(Test-Path 'T:\')) 'one untouched disposable data disk and free fixture letters'
    $disk=$disks[0]
    Initialize-Disk -Number $disk.Number -PartitionStyle GPT|Out-Null
    foreach($letter in @('S','V')) {
        $p=New-Partition -DiskNumber $disk.Number -Size 128MB -DriveLetter $letter
        Format-Volume -Partition $p -FileSystem NTFS -NewFileSystemLabel "OBKLIVE$letter" -Confirm:$false|Out-Null
    }
    $p=New-Partition -DiskNumber $disk.Number -UseMaximumSize -DriveLetter T
    Format-Volume -Partition $p -FileSystem NTFS -NewFileSystemLabel OBKLIVEARCH -Confirm:$false|Out-Null
    $buffer=New-Object byte[] 65536
    [Random]::new(20261008).NextBytes($buffer)
    $f=[IO.File]::Create('S:\control.bin')
    try {for($i=0;$i -lt 1024;$i++){$f.Write($buffer,0,$buffer.Length)};$f.Flush($true)}finally{$f.Dispose()}
    Install 'C:\Lab\installer-before.exe'
    $answer=New-Object Xml.XmlDocument;$answer.Load('C:\Lab\autounattend.xml')
    $password=$answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    Check ([bool]$password) 'disposable desktop credentials available'
    $key='HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $key DefaultUserName 'lab';Set-ItemProperty $key DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $key DefaultPassword $password;Set-ItemProperty $key AutoAdminLogon '1';Set-ItemProperty $key AutoLogonCount 1
    Get-ChildItem ([Environment]::GetFolderPath('CommonStartup')) -Filter 'OwnBackup*.cmd'|Remove-Item -Force
    $action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\Lab\acceptance-installer-live-user.ps1'
    $trigger=New-ScheduledTaskTrigger -AtLogOn -User "$env:COMPUTERNAME\lab"
    $principal=New-ScheduledTaskPrincipal -UserId "$env:COMPUTERNAME\lab" -LogonType Interactive -RunLevel Highest
    Register-ScheduledTask -TaskName 'OwnBackupInstallerLive' -Action $action -Trigger $trigger -Principal $principal -Force|Out-Null
    [IO.File]::WriteAllText("$work\desktop-prepared.ready",'ready')
    Say 'INSTALLER-LIVE-REBOOT';shutdown.exe /r /t 5|Out-Null
}catch{Say "INSTALLER-LIVE-FAIL setup line=$($_.InvocationInfo.ScriptLineNumber) reason=$($_.Exception.Message)";shutdown.exe /s /t 5|Out-Null}
