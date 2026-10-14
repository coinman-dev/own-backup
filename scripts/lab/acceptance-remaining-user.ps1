. 'C:\Lab\acceptance-common.ps1'
try {
    $jobs=Rpc 'remaining-jobs' @{op='activity'}
    $all=(& 'C:\Lab\own-backup-cli.exe' jobs list 2>&1|Out-String)
    $config=(Get-Content 'C:\ProgramData\OwnBackup\jobs.json' -Raw -Encoding UTF8|ConvertFrom-Json).PSObject.BaseObject
    $diff=@($config|Where-Object{$_.name -eq 'stand-diff'})[0]
    if(-not $diff){throw 'the accepted differential fixture job is missing'}
    $image=$diff.last_run.image_path;$expected=(Get-FileHash 'S:\control.bin').Hash
    $null=Rpc 'restore-recovery-fixed' @{op='restore-volume';image=$image;target='\\.\T:';secret='recovery';recovery_file="$work\stand-diff-recovery.txt"}
    $deadline=[DateTime]::UtcNow.AddSeconds(60)
    do{if(Test-Path 'T:\control.bin'){break};Start-Sleep -Seconds 1}while([DateTime]::UtcNow -lt $deadline)
    Check (Test-Path 'T:\control.bin') 'the restored device-path target has its T drive letter back without fixture intervention'
    Check ((Get-FileHash 'T:\control.bin').Hash -eq $expected) 'recovery-key restore returns the exact differential control bytes'
    Check (-not(Test-Path 'T:\omit.bin') -and -not(Test-Path 'T:\omit-tiny.txt')) 'both excluded files remain absent after restoration'
    $keyId=$diff.archive_encryption.local_key.id
    $null=Rpc 'delete-diff-job' @{op='delete';id=$diff.id;files=$false}
    $summary=Rpc 'portable-only-recovery' @{op='verify';image=$image;secret='recovery';recovery_file="$work\stand-diff-recovery.txt"}
    Check ($summary.valid) 'recovery verification works after removing the job and its local authorization'
    Report 'user-final-summary' @{scheduled_cancellation=$true;corruption_detected=$true;new_full_after_corruption=$true;differential_retention=$true;recovery_restore_hash=$expected;deleted_key_id=$keyId;portable_recovery_without_job=$true;earlier_proofs_in_previous_attempt=$true;device_path_letter_fixed=$true}
    & 'C:\Lab\acceptance-gui.ps1'
    Phase 'done';Say 'ACCEPT-USER-FINAL-PASS'
}catch{Phase 'failed';Say "ACCEPT-FAIL remaining-user line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)";LogFailure}
