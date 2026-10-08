. 'C:\Lab\acceptance-common.ps1'
$work='C:\Lab\publication-check'
try {
    Check ((Context).elevated) 'publication regression runs in the elevated lab console'
    $existing=(Get-Content 'C:\Lab\installer-live\live-create.request.result.json' -Raw -Encoding UTF8|ConvertFrom-Json).result.id
    $all=Rpc 'post-kill-all-versions' @{op='verify-job';id=$existing;all=$true;secret='recovery';recovery_file='C:\Lab\installer-live\recovery.txt'}
    Check ($all.valid -and @($all.images).Count -eq 3) 'all three published versions verify after service termination and recovery'
    # Both a resident file and real allocated data go through the post-copy
    # exclusion path while the entire image is still private.
    [IO.File]::WriteAllText('S:\omit-tiny.txt','Resident exclusion fixture')
    $buffer=New-Object byte[] 65536;[Random]::new(20261011).NextBytes($buffer)
    $f=[IO.File]::Create('S:\omit.bin')
    try{for($i=0;$i -lt 128;$i++){$f.Write($buffer,0,$buffer.Length)};$f.Flush($true)}finally{$f.Dispose()}
    [IO.File]::WriteAllText("$work\password.txt",'publication regression synthetic password')
    $partition=Get-Partition -DriveLetter S
    foreach($kind in @('volume','disk')) {
        $request=@{name="publication-$kind";source='S:';sources=@('S:');dest_folder="T:\publication-$kind";image_format='qcow2';schedule=@{kind='none'};
            scheme=@{method='incremental';full_every_n=100};exclusions=@{enabled=$true;patterns=@('omit.bin','omit-tiny.txt')};advanced=@{validate_after=$true;retry_attempts=0;after_backup='none';speed_limit_mbps=0}}
        if($kind -eq 'disk'){$request.disk_source=@{disk_number=$partition.DiskNumber;disk_model='Disposable GPT data disk';parts=@(@{part_index=$partition.PartitionNumber;device='S:';filesystem='ntfs'});bootable=$false}}
        $job=Rpc "publication-$kind-create" @{op='create';encrypted=$true;password_file="$work\password.txt";recovery_file="$work\$kind-recovery.txt";job=$request}
        $full=Rpc "publication-$kind-full" @{op='run';id=$job.id}
        Check ($full.status -eq 'success' -and $full.backup_kind -eq 'full') "staged $kind full backup completes with exclusions"
        $valid=Rpc "publication-$kind-verify" @{op='verify';image=$full.image_path;secret='recovery';recovery_file="$work\$kind-recovery.txt"}
        Check $valid.valid "published $kind image verifies through its final name"
        $inc=Rpc "publication-$kind-inc" @{op='run';id=$job.id}
        Check ($inc.status -eq 'success' -and $inc.backup_kind -eq 'incremental') "staged $kind incremental continues its parent"
        $checked=Rpc "publication-$kind-all" @{op='verify-job';id=$job.id;all=$true;secret='recovery';recovery_file="$work\$kind-recovery.txt"}
        Check ($checked.valid -and @($checked.images).Count -eq 2) "both published $kind versions verify"
        if($kind -eq 'volume') {
            $null=Rpc 'publication-volume-restore' @{op='restore-volume';image=$inc.image_path;target='\\.\V:';secret='recovery';recovery_file="$work\volume-recovery.txt"}
            Check ((Get-FileHash 'V:\control.bin').Hash -eq (Get-FileHash 'S:\control.bin').Hash) 'excluded volume restore retains the control data'
            Check (-not(Test-Path 'V:\omit.bin') -and -not(Test-Path 'V:\omit-tiny.txt')) 'resident and allocated exclusions stay absent after private-stage publication'
        }
        $left=@(Get-ChildItem "T:\publication-$kind" -Force|Where-Object{$_.Name -like '.own-backup-*'})
        Check ($left.Count -eq 0) "successful $kind backup leaves no temporary image or owner lease"
    }
    Report 'publication-check-result' @{pass=$true;all_existing_versions=$all;volume_full_incremental_exclusions=$true;disk_full_incremental_exclusions=$true;temporary_files_after_success=0;resident_excluded=$true;allocated_excluded=$true}
    Say 'PUBLICATION-CHECK-PASS'
}catch{Say "PUBLICATION-CHECK-FAIL line=$($_.InvocationInfo.ScriptLineNumber) reason=$($_.Exception.Message)";LogFailure}
finally{NoLogon;Unregister-ScheduledTask -TaskName OwnBackupPublicationCheck -Confirm:$false -ErrorAction SilentlyContinue;shutdown.exe /s /t 5|Out-Null}
