# Elevated task in the active console of the disposable lab user.
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
    if(Test-Path "$work\installer-live-result.json"){return}
    $context=Context
    Check ($context.elevated -and $context.humans.Count -gt 0) 'installer fixture runs elevated in an interactive lab console'
    Report 'installer-live-context' $context
    WaitReady
    $buffer=New-Object byte[] 65536
    $installed='C:\Program Files\OwnBackup'
    $oldCli=(Get-FileHash "$installed\own-backup-cli.exe").Hash
    $newCli=(Get-FileHash 'C:\Lab\own-backup-cli-current.exe').Hash
    Check ($oldCli -ne $newCli) 'upgrade will replace a genuinely different CLI binary'
    [IO.File]::WriteAllText("$work\password.txt",'installer live synthetic password')
    $job=Rpc 'live-create' @{op='create';encrypted=$true;password_file="$work\password.txt";recovery_file="$work\recovery.txt";job=@{
        name='installer-live';source='S:';sources=@('S:');dest_folder='T:\live';image_format='qcow2';schedule=@{kind='none'};
        scheme=@{method='incremental';full_every_n=100};exclusions=@{enabled=$false;patterns=@()};advanced=@{validate_after=$true;retry_attempts=0;after_backup='none';speed_limit_mbps=0}
    }}
    $first=Rpc 'live-first' @{op='run';id=$job.id}
    Check ($first.status -eq 'success') 'a complete encrypted baseline exists before upgrade'
    $baselineHash=(Get-FileHash $first.image_path).Hash
    $keyBefore=($job.archive_encryption|ConvertTo-Json -Depth 20 -Compress)
    [Random]::new(20261009).NextBytes($buffer)
    $f=[IO.File]::Open('S:\control.bin','Open','Write','Read')
    try {for($i=0;$i -lt 1024;$i++){$f.Write($buffer,0,$buffer.Length)};$f.Flush($true)}finally{$f.Dispose()}
    $expected=(Get-FileHash 'S:\control.bin').Hash
    $null=Rpc 'live-slow' @{op='save';id=$job.id;patch=@{advanced=@{speed_limit_mbps=1}}}
    $active=Rpc 'live-active' @{op='start-run-at-progress';id=$job.id}
    Check ($active.blocks_done -gt 0 -and $active.blocks_done -lt $active.blocks_total) 'upgrade begins after real copy progress before completion'
    $pidBefore=(Get-CimInstance Win32_Service -Filter "Name='OwnBackup'").ProcessId
    $started=[DateTime]::UtcNow
    Install 'C:\Lab\installer-current.exe'
    $elapsed=([DateTime]::UtcNow-$started).TotalSeconds
    WaitReady
    $pidAfter=(Get-CimInstance Win32_Service -Filter "Name='OwnBackup'").ProcessId
    Check ($pidAfter -ne 0 -and $pidAfter -ne $pidBefore) 'the old service exited and the newly installed service started'
    Check ((Get-FileHash "$installed\own-backup-cli.exe").Hash -eq $newCli) 'the replacement executable matches the final package'
    Check ((Get-FileHash $first.image_path).Hash -eq $baselineHash) 'upgrade preserved the complete encrypted baseline byte for byte'
    $saved=Rpc 'live-after-upgrade' @{op='get';id=$job.id}
    Check (($saved.archive_encryption|ConvertTo-Json -Depth 20 -Compress) -eq $keyBefore) 'upgrade preserved the archive authorization'
    Check ($saved.last_run.status -eq 'cancelled') 'service shutdown recorded the interrupted copy as cancelled'
    $valid=Rpc 'live-baseline-verify' @{op='verify';image=$first.image_path;secret='recovery';recovery_file="$work\recovery.txt"}
    Check ($valid.valid) 'the prior encrypted chain remains readable after upgrade'
    $null=Rpc 'live-fast' @{op='save';id=$job.id;patch=@{advanced=@{speed_limit_mbps=0}}}
    $next=Rpc 'live-retry' @{op='run';id=$job.id}
    Check ($next.status -eq 'success' -and $next.backup_kind -eq 'incremental') 'the upgraded service continues the existing chain successfully'
    $null=Rpc 'live-restore' @{op='restore-volume';image=$next.image_path;target='\\.\V:';secret='recovery';recovery_file="$work\recovery.txt"}
    Check ((Get-FileHash 'V:\control.bin').Hash -eq $expected) 'the post-upgrade incremental restore matches changed source bytes'
    Report 'installer-live-result' @{pass=$true;active_progress=$active;service_pid_before=$pidBefore;service_pid_after=$pidAfter;upgrade_seconds=$elapsed;baseline_unchanged=$true;authorization_unchanged=$true;cancelled_run_preserved=$true;retry_kind=$next.backup_kind;restored_sha256=$expected;old_cli_sha256=$oldCli;new_cli_sha256=$newCli;system_volume=@{size=(Get-Volume -DriveLetter C).Size;used=(Get-Volume -DriveLetter C).Size-(Get-Volume -DriveLetter C).SizeRemaining}}
    NoLogon
    Unregister-ScheduledTask -TaskName 'OwnBackupInstallerLive' -Confirm:$false
    Say 'INSTALLER-LIVE-PASS'
}catch{Say "INSTALLER-LIVE-FAIL line=$($_.InvocationInfo.ScriptLineNumber) reason=$($_.Exception.Message)";LogFailure}
finally{shutdown.exe /s /t 5|Out-Null}
