. 'C:\Lab\acceptance-common.ps1'
New-Item -ItemType Directory -Force -Path $work | Out-Null
try {
    $phase=if (Test-Path "$work\phase.txt") { (Get-Content "$work\phase.txt" -Raw -Encoding UTF8).Trim() } else { 'setup' }
    Say "ACCEPT-STAGE phase=$phase"
    if(Test-Path 'C:\Lab\acceptance-resume.txt'){
        $request=(Get-Content 'C:\Lab\acceptance-resume.txt' -Raw -Encoding UTF8).Trim()
        $consumed=if(Test-Path "$work\resume-consumed.txt"){(Get-Content "$work\resume-consumed.txt" -Raw -Encoding UTF8).Trim()}else{''}
        if($request -ne $consumed -and $request -eq 'scan-final-only-1'){
            . 'C:\Lab\acceptance-post.ps1';ScanProductOutput
            [IO.File]::WriteAllText("$work\resume-consumed.txt",$request)
            Phase 'done';Say 'ACCEPT-SUITE-PASS';shutdown.exe /s /t 5|Out-Null;return
        }
        if($request -ne $consumed -and $request -eq 'cold-mixed-retry-1'){
            SetFutureSchedules ((Get-Content "$work\bitlocker-job-id.txt" -Raw -Encoding UTF8).Trim())
            [IO.File]::WriteAllText("$work\resume-consumed.txt",$request)
            NoLogon;Phase 'cold-mixed'
            Say 'ACCEPT-REBOOT phase=cold-mixed';shutdown.exe /r /t 5|Out-Null;return
        }
        if($request -ne $consumed -and $request -in @('bitlocker-resume-1','bitlocker-resume-2')){
            . 'C:\Lab\acceptance-post.ps1';$state=BitLockerState;Report 'bitlocker-source-resumed' $state
            Check ($state.lock -eq 1) 'the previously prepared disposable source is fully encrypted and locked'
            [IO.File]::WriteAllText("$work\resume-consumed.txt",$request)
            Phase 'bitlocker-user';AutoLogon 'acceptance-bitlocker-user.ps1'
            Say 'ACCEPT-REBOOT phase=bitlocker-user';shutdown.exe /r /t 5|Out-Null;return
        }
        if($request -ne $consumed -and $request -in @('post-storage-retry-1','post-storage-retry-2','post-storage-retry-3')){
            [IO.File]::WriteAllText("$work\resume-consumed.txt",$request)
            . 'C:\Lab\acceptance-post.ps1';PostStorage;PrepareBitLocker
            Phase 'bitlocker-user';AutoLogon 'acceptance-bitlocker-user.ps1'
            Say 'ACCEPT-REBOOT phase=bitlocker-user';shutdown.exe /r /t 5|Out-Null;return
        }
        if($request -ne $consumed -and $request -in @('gui-only-retry-1','gui-only-retry-2','gui-only-retry-3','gui-only-retry-4','gui-only-retry-5','gui-only-retry-6')){
            Say 'ACCEPT-GUI-DIAG-BEGIN'
            foreach($dir in @('C:\Lab\Logs','C:\ProgramData\OwnBackup\Logs','C:\Users\lab\AppData\Local\Temp\Logs')){
                Say "ACCEPT-GUI-DIAG-DIR $dir"
                foreach($file in Get-ChildItem $dir -Filter '*gui*.log' -ErrorAction SilentlyContinue){Say "ACCEPT-GUI-DIAG-FILE $($file.Name)"; Report "gui-log-$($file.BaseName)" @([IO.File]::ReadAllLines($file.FullName,[Text.Encoding]::UTF8) | Select-Object -Last 40)}
            }
            $runtime=Get-ItemProperty 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}' -ErrorAction SilentlyContinue
            Report 'gui-runtime-before' @{version=$runtime.pv;processes=@(Get-Process own-backup-gui -ErrorAction SilentlyContinue|Select-Object Id,SessionId,Path);service=(Get-Service OwnBackup).Status.ToString()}
            if(-not $runtime.pv){
                $null=Native 'C:\Lab\WebView2Runtime-x64.exe' @('/silent','/install')
                Report 'gui-runtime-installed' @{version=(Get-ItemProperty 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}').pv}
            }
            [IO.File]::WriteAllText("$work\resume-consumed.txt",$request)
            Phase 'final-user';AutoLogon 'acceptance-gui-only-user.ps1'
            Say 'ACCEPT-REBOOT phase=gui-only';shutdown.exe /r /t 5|Out-Null;return
        }
        if($request -ne $consumed -and $request -in @('restore-retry-1','restore-retry-2')){
            $null=Native 'powercfg.exe' @('/h','off')
            $null=Native 'C:\Lab\own-backup-service-acceptance.exe' @('/update')
            Check ((Get-FileHash 'C:\Lab\own-backup-service.exe').Hash -eq (Get-FileHash 'C:\Lab\own-backup-service-acceptance.exe').Hash) 'the service includes the device-path letter fix'
            Report 'target-before-remount' @{volumes=@(Get-Volume|Select-Object DriveLetter,FileSystem,FileSystemLabel,Path,Size);partitions=@(Get-Partition -DiskNumber 1|Select-Object PartitionNumber,DriveLetter,AccessPaths,Offset,Size)}
            Update-HostStorageCache
            $fixture=Get-Content "$work\source-fixture.json" -Raw -Encoding UTF8|ConvertFrom-Json
            $sourceSpec=@($fixture.partitions|Where-Object{$_.DriveLetter -eq 'S'})[0]
            $targetSpec=@($fixture.partitions|Where-Object{$_.DriveLetter -eq 'T'})[0]
            if(-not $sourceSpec -or -not $targetSpec){throw 'original fixture partition identities are missing'}
            # Initialize-Disk inserted an MSR before our partitions. Select by
            # the saved identities/offsets, never by equal partition sizes.
            foreach($spec in @($sourceSpec,$targetSpec)){
                $part=Get-Partition -DiskNumber $fixture.disk -PartitionNumber $spec.PartitionNumber
                Check ($part.Offset -eq $spec.Offset -and $part.Size -eq $spec.Size) 'restore fixture remount uses the original partition identity and range'
                if($part.DriveLetter -ne $spec.DriveLetter){Set-Partition -DiskNumber $fixture.disk -PartitionNumber $spec.PartitionNumber -NewDriveLetter $spec.DriveLetter}
            }
            Start-Sleep -Seconds 3
            Report 'target-after-fixture-remount' @{control_present=(Test-Path 'T:\control.bin');volumes=@(Get-Volume|Select-Object DriveLetter,FileSystem,FileSystemLabel,Path,Size)}
            [IO.File]::WriteAllText("$work\resume-consumed.txt",$request)
            Phase 'final-user';AutoLogon 'acceptance-remaining-user.ps1'
            Say 'ACCEPT-REBOOT phase=restore-retry';shutdown.exe /r /t 5|Out-Null;return
        }
        if($request -ne $consumed -and $request -eq 'corruption-retry-1'){
            $null=Native 'powercfg.exe' @('/h','off')
            $null=Native 'C:\Lab\own-backup-service.exe' @('/stop')
            $old=Get-Content "$work\corruption.json" -Raw -Encoding UTF8|ConvertFrom-Json
            $stream=[IO.File]::Open($old.image,[IO.FileMode]::Open,[IO.FileAccess]::ReadWrite)
            try{$stream.Position=[long]$old.offset;$byte=$stream.ReadByte();$stream.Position=[long]$old.offset;$stream.WriteByte([byte]($byte -bxor 1));$stream.Flush($true)}finally{$stream.Dispose()}
            $jobs=(Get-Content 'C:\ProgramData\OwnBackup\jobs.json' -Raw -Encoding UTF8|ConvertFrom-Json).PSObject.BaseObject
            foreach($j in $jobs){$j.schedule.kind='none';$j.verification.schedule.kind='none'}
            JsonFile 'C:\ProgramData\OwnBackup\jobs.json' $jobs
            $null=Native 'C:\Lab\own-backup-service.exe' @('/start')
            [IO.File]::WriteAllText("$work\resume-corruption.flag",'skip cancellation already proven in the previous boot')
            [IO.File]::WriteAllText("$work\resume-consumed.txt",$request)
            # Preserve the original Rust result files: serialization by the
            # older PowerShell report adapter lost their note properties.
            foreach($file in Get-ChildItem $work -Filter '*.request.result.json'){
                Say "ACCEPT-REPORT recovered-$($file.BaseName) $([Convert]::ToBase64String([IO.File]::ReadAllBytes($file.FullName)))"
            }
            Phase 'final-user';AutoLogon 'acceptance-user.ps1'
            Say 'ACCEPT-REBOOT phase=resume-corruption';shutdown.exe /r /t 5|Out-Null;return
        }
    }

    if ($phase -eq 'setup') {
        $null=Native 'powercfg.exe' @('/h','off')
        $null=Native 'icacls.exe' @($work,'/grant',"$($env:COMPUTERNAME)\lab:(OI)(CI)M")
        $null=Native 'C:\Lab\own-backup-service-acceptance.exe' @('/update')
        Check ((Get-FileHash 'C:\Lab\own-backup-service.exe').Hash -eq (Get-FileHash 'C:\Lab\own-backup-service-acceptance.exe').Hash) 'installed service is the fresh acceptance build'
        $null=Native 'C:\Lab\own-backup-service.exe' @('/stop')
        # Only the new disposable VM. Never the owner's data or installation.
        JsonFile 'C:\ProgramData\OwnBackup\jobs.json' @()
        JsonFile 'C:\ProgramData\OwnBackup\activity.json' @()
        $null=Native 'C:\Lab\own-backup-service.exe' @('/start')
        $disks=@(Get-Disk | Where-Object { -not $_.IsBoot -and -not $_.IsSystem -and $_.Size -eq 10GB -and $_.PartitionStyle -eq 'RAW' })
        Check ($disks.Count -eq 1) 'one blank 10-GiB disposable data disk identified'
        $disk=$disks[0]
        Initialize-Disk -Number $disk.Number -PartitionStyle GPT | Out-Null
        foreach ($spec in @(@{letter='S';label='ACCEPTSRC';size=1536MB},@{letter='T';label='ACCEPTTGT';size=1536MB},@{letter='R';label='ACCEPTARCH';size=0})) {
            $part=if ($spec.size) { New-Partition -DiskNumber $disk.Number -Size $spec.size -DriveLetter $spec.letter } else { New-Partition -DiskNumber $disk.Number -UseMaximumSize -DriveLetter $spec.letter }
            Format-Volume -Partition $part -FileSystem NTFS -NewFileSystemLabel $spec.label -Confirm:$false | Out-Null
        }
        $rng=[Random]::new(20261007);$buffer=New-Object byte[] 1MB
        foreach ($fixture in @(@{name='control.bin';mib=512},@{name='omit.bin';mib=32})) {
            $file=[IO.File]::Create("S:\$($fixture.name)")
            try {for ($i=0;$i -lt $fixture.mib;$i++) {$rng.NextBytes($buffer);$file.Write($buffer,0,$buffer.Length)};$file.Flush($true)}finally{$file.Dispose()}
        }
        [IO.File]::WriteAllText('S:\omit-tiny.txt','excluded-resident-file-20261007')
        Report 'source-fixture' @{control_sha256=(Get-FileHash 'S:\control.bin').Hash;source_bytes=(Get-Volume -DriveLetter S).Size;disk=$disk.Number;partitions=@(Get-Partition -DiskNumber $disk.Number | Select-Object PartitionNumber,DriveLetter,Offset,Size)}
        Get-ChildItem ([Environment]::GetFolderPath('CommonStartup')) -Filter 'OwnBackup*.cmd' | Remove-Item -Force
        AutoLogon 'acceptance-user.ps1';Phase 'prepare-user'
        Say 'ACCEPT-REBOOT phase=prepare-user';shutdown.exe /r /t 5 | Out-Null;return
    }
    if ($phase -in @('prepare-user','final-user','bitlocker-user','bitlocker-final-user')) {
        if ($phase -eq 'final-user') {. 'C:\Lab\acceptance-clusters.ps1';SourceExtents}
        Say "ACCEPT-WAITING-FOR-USER-AUTOMATION phase=$phase"
        $deadline=[DateTime]::UtcNow.AddMinutes(25)
        do {
            if ($phase -eq 'final-user') {MountedClusterRequest}
            Start-Sleep -Milliseconds 200
            $next=(Get-Content "$work\phase.txt" -Raw -Encoding UTF8).Trim()
            if ($next -ne $phase) {break}
        } while ([DateTime]::UtcNow -lt $deadline)
        if ($phase -eq 'prepare-user' -and $next -eq 'prepared') {
            SetFutureSchedules ((Get-Content "$work\job-id.txt" -Raw -Encoding UTF8).Trim());NoLogon;Phase 'cold1'
            Say 'ACCEPT-REBOOT phase=cold1';shutdown.exe /r /t 5 | Out-Null;return
        }
        if ($phase -eq 'final-user' -and $next -eq 'done') {
            . 'C:\Lab\acceptance-post.ps1';PostStorage;PrepareBitLocker
            Phase 'bitlocker-user';AutoLogon 'acceptance-bitlocker-user.ps1'
            Say 'ACCEPT-REBOOT phase=bitlocker-user';shutdown.exe /r /t 5 | Out-Null;return
        }
        if ($phase -eq 'bitlocker-user' -and $next -eq 'bitlocker-prepared') {
            SetFutureSchedules ((Get-Content "$work\bitlocker-job-id.txt" -Raw -Encoding UTF8).Trim());NoLogon;Phase 'cold-mixed'
            Say 'ACCEPT-REBOOT phase=cold-mixed';shutdown.exe /r /t 5 | Out-Null;return
        }
        if ($phase -eq 'bitlocker-final-user' -and $next -eq 'native-final') {
            . 'C:\Lab\acceptance-post.ps1';PostBrokenAuthorization
            Phase 'done';Say 'ACCEPT-SUITE-PASS';shutdown.exe /s /t 5 | Out-Null;return
        }
        throw "user automation did not complete: phase=$phase next=$next"
    }
    if ($phase -in @('cold1','cold2','cold-mixed')) {
        $context=Context;Report "$phase-context" $context
        Check ($context.explorer -eq 0 -and @($context.humans).Count -eq 0) "$phase has no interactive human logons"
        $id=if($phase -eq 'cold-mixed'){(Get-Content "$work\bitlocker-job-id.txt" -Raw -Encoding UTF8).Trim()}else{(Get-Content "$work\job-id.txt" -Raw -Encoding UTF8).Trim()}
        if($phase -eq 'cold-mixed'){. 'C:\Lab\acceptance-post.ps1';$locked=BitLockerState;Report 'mixed-source-state' $locked;Check ($locked.lock -eq 1) 'the mixed-source volume remains BitLocker-locked after the cold boot'}
        $boot=[DateTime]::Parse($context.boot).ToUniversalTime()
        $deadline=[DateTime]::UtcNow.AddMinutes(9)
        do {
            $allJobs=(Get-Content 'C:\ProgramData\OwnBackup\jobs.json' -Raw -Encoding UTF8 | ConvertFrom-Json).PSObject.BaseObject
            $job=@($allJobs | Where-Object { $_.id -eq $id })[0]
            $backup=$job.last_run;$verify=$job.last_verification
            if ($backup -and $verify -and [DateTime]::Parse($backup.started_at).ToUniversalTime() -ge $boot -and [DateTime]::Parse($verify.started_at).ToUniversalTime() -ge $boot) { break }
            Say "ACCEPT-WAIT phase=$phase backup=$($backup.status) verify=$($verify.status)";Start-Sleep -Seconds 10
        } while ([DateTime]::UtcNow -lt $deadline)
        Report "$phase-job" $job
        Check ($backup -and [DateTime]::Parse($backup.started_at).ToUniversalTime() -ge $boot -and $backup.status -eq 'success' -and $backup.backup_kind -eq 'incremental') "$phase scheduled encrypted incremental completed"
        Check ($verify -and [DateTime]::Parse($verify.started_at).ToUniversalTime() -ge $boot -and $verify.status -eq 'success') "$phase scheduled encrypted verification completed"
        Check (@($verify.verification.images).Count -ge $(if($phase -eq 'cold-mixed'){2}else{3}) -and $job.last_run.id -eq $backup.id) "$phase verification kept backup history and checked every restore point"
        if ($phase -eq 'cold1') {SetFutureSchedules $id;Phase 'cold2';NoLogon}
        elseif($phase -eq 'cold-mixed'){Phase 'bitlocker-final-user';AutoLogon 'acceptance-bitlocker-user.ps1'}
        else {Phase 'final-user';AutoLogon 'acceptance-user.ps1'}
        Say "ACCEPT-REBOOT phase=$phase";shutdown.exe /r /t 5 | Out-Null;return
    }
    if ($phase -eq 'done') {Say 'ACCEPT-SUITE-PASS';shutdown.exe /s /t 5 | Out-Null;return}
    throw "unknown phase $phase"
} catch {
    Say "ACCEPT-FAIL phase=$phase line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    LogFailure;shutdown.exe /s /t 5 | Out-Null
}
