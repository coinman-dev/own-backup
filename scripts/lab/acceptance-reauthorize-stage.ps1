. 'C:\Lab\acceptance-common.ps1'
$work='C:\Lab\reauthorize'
$installed='C:\Program Files\OwnBackup'
function DamageKey([string]$name) {
    $null=Native "$installed\own-backup-service.exe" @('/stop')
    $jobs=(Get-Content 'C:\ProgramData\OwnBackup\jobs.json' -Raw -Encoding UTF8|ConvertFrom-Json).PSObject.BaseObject
    $job=@($jobs|Where-Object{$_.id -eq $script:jobId})[0]
    $path="C:\ProgramData\OwnBackup\.own-backup-service-keys\$($job.archive_encryption.local_key.id).dpapi"
    $bytes=[IO.File]::ReadAllBytes($path);$bytes[$bytes.Length-1]=$bytes[$bytes.Length-1] -bxor 1
    [IO.File]::WriteAllBytes($path,$bytes)
    [IO.File]::WriteAllText("$work\$name-ref.txt",$job.archive_encryption.local_key.id)
    Report "$name-damaged-key" @{local_ref=$job.archive_encryption.local_key.id;sha256=(Get-FileHash $path).Hash;portable_key=$job.archive_encryption.portable_key}
    $null=Native "$installed\own-backup-service.exe" @('/start')
}
function Desktop {
    $answer=New-Object Xml.XmlDocument;$answer.Load('C:\Lab\autounattend.xml')
    $password=$answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    Check ([bool]$password) 'synthetic logon credentials available'
    $key='HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $key DefaultUserName lab;Set-ItemProperty $key DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $key DefaultPassword $password;Set-ItemProperty $key AutoAdminLogon '1';Set-ItemProperty $key AutoLogonCount 1
    $action=New-ScheduledTaskAction -Execute powershell.exe -Argument '-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\Lab\acceptance-reauthorize-user.ps1'
    $trigger=New-ScheduledTaskTrigger -AtLogOn -User "$env:COMPUTERNAME\lab"
    $principal=New-ScheduledTaskPrincipal -UserId "$env:COMPUTERNAME\lab" -LogonType Interactive -RunLevel Highest
    Register-ScheduledTask -TaskName OwnBackupReauthorize -Action $action -Trigger $trigger -Principal $principal -Force|Out-Null
}
try {
    $script:jobId=(Get-Content 'C:\Lab\installer-live\live-create.request.result.json' -Raw -Encoding UTF8|ConvertFrom-Json).result.id
    $phase=if(Test-Path "$work\phase.txt"){(Get-Content "$work\phase.txt" -Raw).Trim()}else{'setup'}
    if($phase -eq 'setup') {
        New-Item -ItemType Directory $work|Out-Null
        $null=Native "$installed\own-backup-service.exe" @('/stop')
        foreach($name in @('own-backup-service.exe','own-backup-gui.exe','own-backup-cli.exe')) {
            Copy-Item "C:\Lab\current-$name" "$installed\$name" -Force
            Check ((Get-FileHash "$installed\$name").Hash -eq (Get-FileHash "C:\Lab\current-$name").Hash) 'fresh binary installed only in the disposable guest'
        }
        DamageKey 'password'
        Phase 'password';Desktop;Say 'REAUTHORIZE-REBOOT phase=password';shutdown.exe /r /t 5|Out-Null;return
    }
    if($phase -eq 'password-done') {
        $old=(Get-Content "$work\password-ref.txt" -Raw).Trim()
        Check (-not(Test-Path "C:\ProgramData\OwnBackup\.own-backup-service-keys\$old.dpapi")) 'password repair retired the old corrupt local authorization'
        DamageKey 'recovery'
        Phase 'recovery';Desktop;Say 'REAUTHORIZE-REBOOT phase=recovery';shutdown.exe /r /t 5|Out-Null;return
    }
    if($phase -eq 'cold') {
        $context=Context;Report 'reauthorize-cold-context' $context
        Check ($context.humans.Count -eq 0 -and $context.explorer -eq 0) 'cold backup acceptance has no interactive user'
        $old=(Get-Content "$work\recovery-ref.txt" -Raw).Trim()
        Check (-not(Test-Path "C:\ProgramData\OwnBackup\.own-backup-service-keys\$old.dpapi")) 'recovery repair retired the second corrupt local authorization'
        $before=(Get-Content "$work\cold-before-run.txt" -Raw).Trim()
        $deadline=[DateTime]::UtcNow.AddMinutes(6)
        do {
            $job=@((Get-Content 'C:\ProgramData\OwnBackup\jobs.json' -Raw -Encoding UTF8|ConvertFrom-Json).PSObject.BaseObject|Where-Object{$_.id -eq $script:jobId})[0]
            if($job.last_run.id -ne $before -and $job.last_run.status -eq 'success'){break}
            Start-Sleep -Seconds 2
        }while([DateTime]::UtcNow -lt $deadline)
        Check ($job.last_run.id -ne $before -and $job.last_run.status -eq 'success' -and $job.last_run.backup_kind -eq 'incremental') 'repaired authorization performed a cold scheduled increment without login'
        Report 'reauthorize-cold-job' $job
        $secretFiles=@('C:\Lab\installer-live\password.txt','C:\Lab\installer-live\recovery.txt')
        $secrets=@($secretFiles|ForEach-Object{([IO.File]::ReadAllText($_)).Trim()})
        $files=@(Get-ChildItem "$installed\Logs" -Filter '*.log')+@(Get-ChildItem 'C:\ProgramData\OwnBackup' -File)
        foreach($file in $files){
            $stream=[IO.File]::Open($file.FullName,'Open','Read','ReadWrite, Delete')
            try{$memory=[IO.MemoryStream]::new();$stream.CopyTo($memory);$text=[Text.Encoding]::UTF8.GetString($memory.ToArray());$memory.Dispose()}finally{$stream.Dispose()}
            foreach($secret in $secrets){Check (-not $text.Contains($secret)) 'product output does not contain a plaintext test secret'}
        }
        Report 'reauthorize-result' @{pass=$true;password_gui=$true;recovery_rpc=$true;cold_without_login=$true;portable_unchanged=$true;old_refs_retired=$true;secrets_found=0;files_scanned=$files.Count}
        Phase 'done';Say 'REAUTHORIZE-PASS';shutdown.exe /s /t 5|Out-Null;return
    }
    if($phase -in @('password','recovery')){Say "REAUTHORIZE-WAIT phase=$phase";return}
    throw "unexpected reauthorization phase $phase"
}catch{Say "REAUTHORIZE-FAIL system phase=$phase line=$($_.InvocationInfo.ScriptLineNumber) reason=$($_.Exception.Message)";shutdown.exe /s /t 5|Out-Null}
