. 'C:\Lab\acceptance-common.ps1'
try {
    $phase=(Get-Content "$work\phase.txt" -Raw -Encoding UTF8).Trim()
    if($phase -eq 'bitlocker-user'){
        $null=Rpc 'source-refuses-archive-password' @{op='provide-source-password';volume='S:';source_password_file="$work\password.txt";remember=$false;expect_error=$true}
        $probe=Rpc 'source-accepts-source-password' @{op='provide-source-password';volume='S:';source_password_file="$work\source-password.txt";remember=$true}
        Check ($probe.ok -and $probe.filesystem -eq 'ntfs') 'only the separate source password opens the BitLocker NTFS filesystem'
        $job=Rpc 'create-mixed-job' @{op='create';encrypted=$true;password_file="$work\password.txt";recovery_file="$work\stand-mixed-recovery.txt";job=@{
            name='stand-mixed';source='S:';sources=@('S:');requires_password=$true;dest_folder='R:\stand\mixed';image_format='qcow2';schedule=@{kind='none'};
            scheme=@{method='incremental';full_every_n=100};exclusions=@{enabled=$true;patterns=@('omit.bin','omit-tiny.txt')};advanced=@{validate_after=$true;retry_attempts=0;after_backup='none'}
        }}
        [IO.File]::WriteAllText("$work\bitlocker-job-id.txt",$job.id)
        $run=Rpc 'mixed-full' @{op='run';id=$job.id};Check ($run.status -eq 'success' -and $run.backup_kind -eq 'full') 'a locked BitLocker source backed up into a sealed OBK archive'
        $null=Rpc 'archive-refuses-source-password' @{op='verify';image=$run.image_path;secret='password';password_file="$work\source-password.txt";expect_error=$true}
        $summary=Rpc 'mixed-verify' @{op='verify';image=$run.image_path;secret='recovery';recovery_file="$work\stand-mixed-recovery.txt"}
        Check ($summary.valid) 'the mixed archive opens with its own recovery envelope'
        Phase 'bitlocker-prepared';Say 'ACCEPT-BITLOCKER-USER-PREPARED';return
    }
    if($phase -ne 'bitlocker-final-user'){throw "wrong bitlocker user phase $phase"}
    $id=(Get-Content "$work\bitlocker-job-id.txt" -Raw -Encoding UTF8).Trim()
    $job=Rpc 'mixed-cold-job' @{op='get';id=$id}
    $image=$job.last_run.image_path
    $null=Rpc 'mixed-restore-recovery' @{op='restore-volume';image=$image;target='\\.\T:';secret='recovery';recovery_file="$work\stand-mixed-recovery.txt"}
    Start-Sleep -Seconds 3
    $expected=(Get-Content "$work\user-final-summary.json" -Raw -Encoding UTF8|ConvertFrom-Json).recovery_restore_hash
    Check ((Get-FileHash 'T:\control.bin').Hash -eq $expected) 'cold scheduled BitLocker-to-OBK increment restores to the exact original plaintext'
    Report 'mixed-restored-content' @{sha256=(Get-FileHash 'T:\control.bin').Hash;source_password_is_separate=$true;recovery_envelope_used=$true}
    Phase 'native-final';Say 'ACCEPT-BITLOCKER-FINAL-PASS'
}catch{Phase 'failed';Say "ACCEPT-FAIL bitlocker-phase=$phase line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)";LogFailure}
