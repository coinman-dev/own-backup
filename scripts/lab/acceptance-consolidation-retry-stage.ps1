. 'C:\Lab\acceptance-common.ps1'
try {
    $ready='C:\Lab\acceptance-consolidation-retry.ready'
    if(Test-Path $ready){Say 'ACCEPT-CON-RETRY-WAIT';return}
    Check ((Get-Volume -DriveLetter R).FileSystemLabel -eq 'ACCEPTARCH') 'retry uses only the disposable archive volume'
    $id=(Get-Content "$work\bitlocker-job-id.txt" -Raw -Encoding UTF8).Trim()
    $jobs=(Get-Content 'C:\ProgramData\OwnBackup\jobs.json' -Raw -Encoding UTF8|ConvertFrom-Json).PSObject.BaseObject
    Check (@($jobs|Where-Object{$_.id -eq $id}).Count -eq 1) 'the encrypted fixture identity survived the UAC refusal'
    Check ((Get-Volume -DriveLetter R).SizeRemaining -ge 2GB) 'scratch space remains after the refused attempt'
    Get-ChildItem ([Environment]::GetFolderPath('CommonStartup')) -Filter 'OwnBackup*.cmd'|Remove-Item -Force
    [IO.File]::WriteAllText($ready,'ready')
    AutoLogon 'acceptance-consolidation-retry-user.ps1'
    Say 'ACCEPT-REBOOT phase=consolidation-retry-user';shutdown.exe /r /t 5|Out-Null
}catch{Say "ACCEPT-FAIL consolidation-retry-stage line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)";shutdown.exe /s /t 5|Out-Null}
