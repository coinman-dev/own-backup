. 'C:\Lab\acceptance-common.ps1'
function Mutation([int]$seed) {
    $buffer=New-Object byte[] 1MB;([Random]::new($seed)).NextBytes($buffer)
    $file=[IO.File]::Open('S:\control.bin',[IO.FileMode]::Open,[IO.FileAccess]::Write,[IO.FileShare]::Read)
    try {for ($i=0;$i -lt 24;$i++) {$file.Write($buffer,0,$buffer.Length)};$file.Flush($true)}finally{$file.Dispose()}
    return (Get-FileHash 'S:\control.bin').Hash
}
function NewEncrypted([string]$name,[string]$method,[string]$folder,[int]$rollover=100,[int]$keep=5) {
    return Rpc "create-$name" @{op='create';encrypted=$true;password_file="$work\password.txt";recovery_file="$work\$name-recovery.txt";job=@{
        name=$name;source='S:';sources=@('S:');dest_folder=$folder;image_format='qcow2';schedule=@{kind='none'};
        scheme=@{method=$method;only_incremental=$false;full_every_n=$rollover;keep_chains=$keep};
        exclusions=@{enabled=$true;patterns=@('omit.bin','omit-tiny.txt');patterns_linux=@()};
        advanced=@{validate_after=$true;verify_latest_only=$false;retry_attempts=0;after_backup='none'}
    }}
}
function Run([string]$name,[string]$id) { $run=Rpc $name @{op='run';id=$id};Check ($run.status -eq 'success') "$name finished successfully";return $run }
function Verify([string]$name,[string]$image,[string]$mode,[string]$key) {
    return Rpc $name @{op='verify';image=$image;secret=$mode;password_file="$work\password.txt";recovery_file=$key}
}
function MountedHash([string]$image,[string]$key,[string]$mode='password') {
    $null=Rpc 'mount' @{op='mount';image=$image;secret=$mode;password_file="$work\password.txt";recovery_file=$key}
    try {
        $deadline=[DateTime]::UtcNow.AddSeconds(60)
        do {
            $roots=@(Get-Volume | Where-Object { $_.DriveLetter -and $_.DriveLetter -notin @('C','S','T','R') } | ForEach-Object { "$($_.DriveLetter):\" } | Where-Object { Test-Path (Join-Path $_ 'control.bin') })
            if ($roots.Count -eq 1) {break};Start-Sleep -Seconds 1
        } while ([DateTime]::UtcNow -lt $deadline)
        Check ($roots.Count -eq 1) 'one mounted archive volume identified independently'
        $root=$roots[0]
        Check (-not (Test-Path (Join-Path $root 'omit.bin')) -and -not (Test-Path (Join-Path $root 'omit-tiny.txt'))) 'large and resident excluded files have no directory entries in the mounted OBK'
        if (Test-Path "$work\excluded-extents.json") {
            $requestId=[Guid]::NewGuid().ToString()
            JsonFile "$work\mounted-clusters.request.json" @{id=$requestId;root=$root;image=$image}
            $deadline=[DateTime]::UtcNow.AddSeconds(60)
            do {
                Start-Sleep -Milliseconds 200
                $checked=if(Test-Path "$work\mounted-clusters.result.json"){Get-Content "$work\mounted-clusters.result.json" -Raw -Encoding UTF8|ConvertFrom-Json}else{$null}
                if($checked -and $checked.id -eq $requestId){break}
            }while([DateTime]::UtcNow -lt $deadline)
            Check ($checked -and $checked.id -eq $requestId -and $checked.zero -and $checked.bytes_checked -ge 32MB) 'SYSTEM read at least 32 MiB of excluded source extents through the mounted archive and found zero data'
            Report 'mounted-cluster-result' $checked
        }
        $hash=(Get-FileHash (Join-Path $root 'control.bin')).Hash
        Report 'mounted-control' @{image=$image;root=$root;sha256=$hash;large_exclusion_absent=$true;resident_exclusion_absent=$true}
        return $hash
    } finally {$null=Rpc 'unmount' @{op='unmount';image=$image}}
}
function ScheduleVerify([string]$id,[string]$name) {
    $time=(Get-Date).AddMinutes(1).ToString('HH:mm')
    $job=Rpc $name @{op='save';id=$id;patch=@{schedule=@{kind='none'};verification=@{schedule=@{kind='daily';time=$time;run_missed=$false};all_versions=$false}}}
    return $job
}
try {
    $phase=(Get-Content "$work\phase.txt" -Raw -Encoding UTF8).Trim()
    Report 'user-context' (Context)
    $deadline=[DateTime]::UtcNow.AddMinutes(3)
    do {
        try {$null=Rpc 'service-ready' @{op='status'};break}catch{if ([DateTime]::UtcNow -ge $deadline){throw};Start-Sleep -Seconds 2}
    } while ($true)
    if ($phase -eq 'prepare-user') {
        [IO.File]::WriteAllText("$work\password.txt",'Stand-Archive-Password-Only-20261007')
        $job=NewEncrypted 'stand-inc' 'incremental' 'R:\stand\inc'
        [IO.File]::WriteAllText("$work\job-id.txt",$job.id)
        $full=Run 'initial-full' $job.id;Check ($full.backup_kind -eq 'full') 'first encrypted backup is a full copy'
        $hash=Mutation 1007
        $inc=Run 'initial-incremental' $job.id;Check ($inc.backup_kind -eq 'incremental') 'changed source produced an encrypted increment'
        Report 'initial-content' @{sha256=$hash;image=$inc.image_path}
        Check ((MountedHash $inc.image_path "$work\stand-inc-recovery.txt") -eq $hash) 'mounted encrypted increment contains exactly the changed control file'
        foreach ($mode in @('none','wrong-password','wrong-recovery')) {
            $null=Rpc "verify-refuses-$mode" @{op='verify';image=$inc.image_path;secret=$mode;expect_error=$true}
        }
        $summary=Verify 'verify-password' $inc.image_path 'password' "$work\stand-inc-recovery.txt"
        Check ($summary.valid) 'password opens the whole encrypted chain'
        $summary=Verify 'verify-recovery' $inc.image_path 'recovery' "$work\stand-inc-recovery.txt"
        Check ($summary.valid) 'recovery key opens the whole encrypted chain'
        try {$files=@(Get-ChildItem 'C:\ProgramData\OwnBackup\.own-backup-service-keys' -ErrorAction Stop);$readable=$true}catch{$readable=$false}
        Check (-not $readable) 'non-elevated console cannot enumerate the service key directory'
        Phase 'prepared';Say 'ACCEPT-USER-PREPARED';return
    }
    if ($phase -ne 'final-user') {throw "unexpected user phase $phase"}
    $id=(Get-Content "$work\job-id.txt" -Raw -Encoding UTF8).Trim()
    $job=Rpc 'before-cancel' @{op='get';id=$id}
    $originalRun=$job.last_run.id
    if(-not(Test-Path "$work\resume-corruption.flag")){
    $null=ScheduleVerify $id 'schedule-cancel'
    $cancel=Rpc 'cancel-scheduled' @{op='cancel-scheduled';id=$id}
    Check ($cancel.run.status -eq 'cancelled' -and $cancel.observed.blocks_total -gt 0) 'scheduled verification cancelled during actual block reading'
    $job=Rpc 'after-cancel' @{op='get';id=$id}
    Check ($job.last_run.id -eq $originalRun) 'cancelled verification did not change the last backup'
    }
    $null=Rpc 'disable-verification' @{op='save';id=$id;patch=@{verification=@{schedule=@{kind='none'}}}}
    $latest=$job.last_run.image_path
    $listing=Rpc 'corruption-target-versions' @{op='versions';id=$id}
    $chain=@($listing.chains|Where-Object{$_.current})[0]
    $image=@($chain.images)[0].image_path
    # Corrupt an authenticated record of the small latest increment, keeping
    # the headers intact. The result must be a failed check, never empty PASS.
    $stream=[IO.File]::Open($image,[IO.FileMode]::Open,[IO.FileAccess]::ReadWrite,[IO.FileShare]::Read)
    try {$offset=[long]($stream.Length/2);$stream.Position=$offset;$byte=$stream.ReadByte();$stream.Position=$offset;$stream.WriteByte([byte]($byte -bxor 1));$stream.Flush($true)}finally{$stream.Dispose()}
    Report 'corruption' @{image=$image;latest=$latest;offset=$offset;changed_bytes=1;file_bytes=(Get-Item $image).Length;target='full-data-record'}
    $previous=$job.last_verification.id
    $null=ScheduleVerify $id 'schedule-corruption'
    $deadline=[DateTime]::UtcNow.AddMinutes(4)
    do {
        Start-Sleep -Seconds 2
        $job=Rpc 'poll-corruption' @{op='get';id=$id}
        if ($job.last_verification -and $job.last_verification.id -ne $previous) {break}
    } while ([DateTime]::UtcNow -lt $deadline)
    Check ($job.last_verification.id -ne $previous -and $job.last_verification.status -eq 'error') 'scheduled verification detects the damaged encrypted version'
    Check (@($job.invalid_images).Count -gt 0 -or $job.verification_blocked) 'failed version is durably refused as a parent'
    $null=Rpc 'disable-corrupt-schedule' @{op='save';id=$id;patch=@{verification=@{schedule=@{kind='none'}}}}
    $new=Run 'full-after-corruption' $id;Check ($new.backup_kind -eq 'full') 'a damaged parent causes a fresh full backup'
    $hash=(Get-FileHash 'S:\control.bin').Hash
    Check ((MountedHash $new.image_path "$work\stand-inc-recovery.txt" 'recovery') -eq $hash) 'the new chain is readable by the portable recovery key'

    # Differential archives: siblings on the base, rollover, and retention.
    $diff=NewEncrypted 'stand-diff' 'differential' 'R:\stand\diff' 2 1
    $kinds=@();$runs=@()
    for ($n=1;$n -le 5;$n++) {
        $hash=Mutation (2000+$n)
        $run=Run "diff-$n" $diff.id;$kinds+=$run.backup_kind;$runs+=$run
        if ($n -eq 3) {
            Check ($runs[1].base_image_path -eq $runs[0].image_path -and $runs[2].base_image_path -eq $runs[0].image_path) 'both differential versions belong to their full base'
            Check ((MountedHash $run.image_path "$work\stand-diff-recovery.txt") -eq $hash) 'differential version resolves to exactly its captured source'
        }
    }
    Check (($kinds -join ',') -eq 'full,differential,differential,full,differential') 'encrypted differential rollover made the expected two chains'
    $versions=Rpc 'diff-versions' @{op='versions';id=$diff.id}
    Check (@($versions.chains).Count -eq 1 -and @($versions.chains[0].images).Count -eq 2) 'encrypted retention kept just the newest full and differential'
    foreach ($old in $runs[0..2]) {Check (-not (Test-Path $old.image_path)) 'retention removed an old encrypted restore point'}
    $summary=Verify 'diff-final-verify' $run.image_path 'recovery' "$work\stand-diff-recovery.txt"
    Check ($summary.valid) 'remaining differential chain verifies with a recovery key'
    $null=Rpc 'restore-recovery' @{op='restore-volume';image=$run.image_path;target='\\.\T:';secret='recovery';recovery_file="$work\stand-diff-recovery.txt"}
    Start-Sleep -Seconds 3
    Check ((Get-FileHash 'T:\control.bin').Hash -eq $hash) 'restoring the differential copy by recovery key returns the exact control bytes'
    Check (-not (Test-Path 'T:\omit.bin') -and -not (Test-Path 'T:\omit-tiny.txt')) 'excluded files remain absent after restoration'
    # Removing the job revokes its local authorization; archives still open
    # solely from their portable recovery envelope.
    $keyId=$diff.archive_encryption.local_key.id
    $null=Rpc 'delete-diff-job' @{op='delete';id=$diff.id;files=$false}
    $summary=Verify 'portable-only-recovery' $run.image_path 'recovery' "$work\stand-diff-recovery.txt"
    Check ($summary.valid) 'recovery verification works after the job and its local authorization are deleted'
    $null=Rpc 'activity' @{op='activity'}
    Report 'user-final-summary' @{scheduled_cancellation=$true;corruption_detected=$true;new_full_after_corruption=$true;differential_retention=$true;recovery_restore_hash=$hash;deleted_key_id=$keyId;portable_recovery_without_job=$true}
    if(Test-Path 'C:\Lab\acceptance-gui.ps1'){& 'C:\Lab\acceptance-gui.ps1'}
    Phase 'done';Say 'ACCEPT-USER-FINAL-PASS'
} catch {
    Phase 'failed';Say "ACCEPT-FAIL user-phase=$phase line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    LogFailure
}
