. 'C:\Lab\acceptance-common.ps1'
$work='C:\Lab\service-kill'
$installed='C:\Program Files\OwnBackup'
function WaitReady {
    for($i=0;$i -lt 90;$i++){
        try{$r=Rpc 'kill-ready' @{op='status'};if($r){return}}catch{}
        Start-Sleep -Seconds 1
    }
    throw 'service did not return after termination'
}
function Images {
    return @(Get-ChildItem 'T:\live' -Filter '*.obk'|ForEach-Object{[ordered]@{name=$_.Name;bytes=$_.Length;sha256=(Get-FileHash $_.FullName).Hash}}|Sort-Object {$_.name})
}
try {
    $context=Context
    Check ($context.elevated -and $context.humans.Count -gt 0) 'kill test runs in the authorized elevated lab console'
    Report 'service-kill-context' $context
    WaitReady
    $id=(Get-Content 'C:\Lab\installer-live\live-create.request.result.json' -Raw -Encoding UTF8|ConvertFrom-Json).result.id
    $job=Rpc 'kill-before-job' @{op='get';id=$id}
    Check ($job.last_run.status -eq 'success') 'a complete encrypted chain exists before the forced stop'
    $latest=$job.last_run.image_path
    $keyBefore=ConvertTo-Json -InputObject $job.archive_encryption -Depth 20 -Compress
    $before=Images
    Report 'kill-before-images' $before
    $buffer=New-Object byte[] 65536
    [Random]::new(20261010).NextBytes($buffer)
    $f=[IO.File]::Open('S:\control.bin','Open','Write','Read')
    try{for($i=0;$i -lt 1024;$i++){$f.Write($buffer,0,$buffer.Length)};$f.Flush($true)}finally{$f.Dispose()}
    $expected=(Get-FileHash 'S:\control.bin').Hash
    $null=Rpc 'kill-slow' @{op='save';id=$id;patch=@{advanced=@{speed_limit_mbps=1}}}
    $active=Rpc 'kill-active' @{op='start-run-at-progress';id=$id}
    Check ($active.blocks_done -gt 0 -and $active.blocks_done -lt $active.blocks_total) 'a real encrypted copy is in flight'
    $service=Get-CimInstance Win32_Service -Filter "Name='OwnBackup'"
    $targetPid=[int]$service.ProcessId
    $process=Get-Process -Id $targetPid
    Check ($targetPid -gt 0 -and $process.Path -eq "$installed\own-backup-service.exe") 'termination targets only the installed guest backup service'
    $beforeNames=@($before|ForEach-Object{$_['name']})
    $deadline=[DateTime]::UtcNow.AddSeconds(30)
    do {
        $during=@(Get-ChildItem 'T:\live' -Filter '*.obk'|Where-Object{$beforeNames -notcontains $_.Name}|ForEach-Object{
            # NTFS directory enumeration can retain Length=0 while the writer
            # holds its file open. Query EOF through a shared read handle.
            $stream=[IO.File]::Open($_.FullName,'Open','Read','ReadWrite, Delete')
            try{[pscustomobject]@{Name=$_.Name;Length=$stream.Length}}finally{$stream.Dispose()}
        })
        if($during.Count -eq 1 -and $during[0].Length -gt 4MB){break}
        Start-Sleep -Milliseconds 100
    }while([DateTime]::UtcNow -lt $deadline)
    Report 'kill-during-images' $during
    Check ($during.Count -eq 1 -and $during[0].Length -gt 4MB) 'one unfinished encrypted output is present before termination'
    Report 'kill-cut' @{pid=$targetPid;active=$active;new_outputs=$during;method='Stop-Process -Force';expected_control_sha256=$expected}
    Stop-Process -Id $targetPid -Force
    for($i=0;$i -lt 30;$i++){if(-not(Get-Process -Id $targetPid -ErrorAction SilentlyContinue)){break};Start-Sleep -Milliseconds 100}
    Check (-not(Get-Process -Id $targetPid -ErrorAction SilentlyContinue)) 'the service process was forcibly terminated'
    $null=Native "$installed\own-backup-service.exe" @('/start')
    WaitReady
    $newPid=(Get-CimInstance Win32_Service -Filter "Name='OwnBackup'").ProcessId
    Check ($newPid -ne 0 -and $newPid -ne $targetPid) 'a new service process owns the store after restart'
    $after=Rpc 'kill-after-job' @{op='get';id=$id}
    Check ((ConvertTo-Json -InputObject $after.archive_encryption -Depth 20 -Compress) -eq $keyBefore) 'local archive authorization survived termination'
    foreach($image in $before){Check ((Get-FileHash (Join-Path 'T:\live' $image.name)).Hash -eq $image.sha256) 'a published encrypted image is byte-for-byte unchanged'}
    $valid=Rpc 'kill-prior-verify' @{op='verify';image=$latest;secret='recovery';recovery_file='C:\Lab\installer-live\recovery.txt'}
    Check ($valid.valid) 'the previous complete chain remains readable by portable key'
    Report 'kill-after-restart-images' (Images)
    $null=Rpc 'kill-fast' @{op='save';id=$id;patch=@{advanced=@{speed_limit_mbps=0}}}
    $retry=Rpc 'kill-retry' @{op='run';id=$id}
    Report 'kill-retry-outcome' @{status=$retry.status;error=$retry.error;kind=$retry.backup_kind;images=(Images)}
    Check ($retry.status -eq 'success') 'the next backup succeeds without manually deleting the interrupted output'
    $left=@($during|Where-Object{Test-Path (Join-Path 'T:\live' $_.Name)})
    Report 'kill-unfinished-after-retry' $left
    Check ($left.Count -eq 0) 'the interrupted output was automatically retired from the restore-point namespace'
    $null=Rpc 'kill-retry-restore' @{op='restore-volume';image=$retry.image_path;target='\\.\V:';secret='recovery';recovery_file='C:\Lab\installer-live\recovery.txt'}
    Check ((Get-FileHash 'V:\control.bin').Hash -eq $expected) 'the retried backup restores the changed source exactly'
    Report 'service-kill-result' @{pass=$true;pid_before=$targetPid;pid_after=$newPid;active_progress=$active;published_unchanged=$true;authorization_unchanged=$true;manual_cleanup=$false;retry_kind=$retry.backup_kind;restored_sha256=$expected;unfinished_before=$during;final_versions=(Rpc 'kill-versions' @{op='versions';id=$id})}
    Say 'SERVICE-KILL-PASS'
}catch{Say "SERVICE-KILL-FAIL line=$($_.InvocationInfo.ScriptLineNumber) reason=$($_.Exception.Message)";LogFailure}
finally{NoLogon;Unregister-ScheduledTask -TaskName OwnBackupServiceKill -Confirm:$false -ErrorAction SilentlyContinue;shutdown.exe /s /t 5|Out-Null}
