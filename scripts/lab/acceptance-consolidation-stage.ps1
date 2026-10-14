. 'C:\Lab\acceptance-common.ps1'
try {
    if(Test-Path 'C:\Lab\acceptance-consolidation.ready'){
        Say 'ACCEPT-CON-DIAG'
        foreach($name in @('consolidation-stderr.txt','consolidation-stdout.txt')){
            $file="C:\Lab\acceptance\$name"
            if(Test-Path $file){$bytes=[IO.File]::ReadAllBytes($file);Say "ACCEPT-CON-FILE name=$name bytes=$($bytes.Length) base64=$([Convert]::ToBase64String($bytes))"}
        }
        shutdown.exe /s /t 5|Out-Null;return
    }
    $id=(Get-Content "$work\bitlocker-job-id.txt" -Raw -Encoding UTF8).Trim()
    $jobs=(Get-Content 'C:\ProgramData\OwnBackup\jobs.json' -Raw -Encoding UTF8|ConvertFrom-Json).PSObject.BaseObject
    Check (@($jobs|Where-Object{$_.id -eq $id}).Count -eq 1) 'the disposable mixed job is available in the stand snapshot'
    Check ((Get-Volume -DriveLetter R).FileSystemLabel -eq 'ACCEPTARCH') 'consolidation destination is the synthetic archive volume'
    Check ((Get-Volume -DriveLetter R).SizeRemaining -ge 2GB) 'encrypted consolidation has at least two GiB of scratch space'
    Get-ChildItem ([Environment]::GetFolderPath('CommonStartup')) -Filter 'OwnBackup*.cmd'|Remove-Item -Force
    [IO.File]::WriteAllText('C:\Lab\acceptance-consolidation.ready','ready')
    AutoLogon 'acceptance-consolidation-user.ps1'
    Say 'ACCEPT-REBOOT phase=consolidation-user';shutdown.exe /r /t 5|Out-Null
}catch{Say "ACCEPT-FAIL consolidation-stage line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)";shutdown.exe /s /t 5|Out-Null}
