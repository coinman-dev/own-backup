. 'C:\Lab\acceptance-common.ps1'
function ServiceWait([string]$state) {
    $deadline=[DateTime]::UtcNow.AddSeconds(90)
    do {if((Get-Service OwnBackup).Status.ToString() -eq $state){return};Start-Sleep -Seconds 1}while([DateTime]::UtcNow -lt $deadline)
    throw "service did not reach $state"
}
function KeyHashes {
    $out=@{}
    foreach($file in Get-ChildItem 'C:\ProgramData\OwnBackup\.own-backup-service-keys' -Filter '*.dpapi'){$out[$file.Name]=(Get-FileHash $file.FullName).Hash}
    return $out
}
function PostStorage {
    $path='C:\ProgramData\OwnBackup\jobs.json';$keyRoot='C:\ProgramData\OwnBackup\.own-backup-service-keys'
    $null=Native 'C:\Lab\own-backup-service.exe' @('/stop');ServiceWait 'Stopped'
    $original=[IO.File]::ReadAllBytes($path);$jobs=(Get-Content $path -Raw -Encoding UTF8|ConvertFrom-Json).PSObject.BaseObject;$keys=KeyHashes
    try {
        foreach($case in @('missing-id','duplicate-id')){
            $fixture=if($case -eq 'missing-id'){@(@{value=$jobs;Count=@($jobs).Count})}else{@($jobs[0],$jobs[0])}
            $fixture=[object[]]$fixture
            JsonFile $path $fixture
            Check ([IO.File]::ReadAllText($path).TrimStart().StartsWith('[')) 'invalid-store fixture is a JSON array, not a syntax/type-error object'
            Say "ACCEPT-STORAGE-CASE $case"
            $before=(Get-FileHash $path).Hash
            $old=$ErrorActionPreference;$ErrorActionPreference='Continue'
            try{$output=(& 'C:\Lab\own-backup-service.exe' '/start' 2>&1|Out-String);$code=$LASTEXITCODE}finally{$ErrorActionPreference=$old}
            ServiceWait 'Stopped'
            Check ((Get-FileHash $path).Hash -eq $before) "$case startup refused without rewriting jobs"
            $after=KeyHashes
            foreach($name in $keys.Keys){Check ($after[$name] -eq $keys[$name]) "$case did not remove a real protected archive authorization"}
            Report "storage-$case" @{startup_code=$code;service_stopped=$true;jobs_unchanged=$true;real_keys_unchanged=$true}
        }
    }finally{[IO.File]::WriteAllBytes($path,$original)}
    $deleted=(Get-Content "$work\user-final-summary.json" -Raw -Encoding UTF8|ConvertFrom-Json).deleted_key_id
    Check (-not(Test-Path "$keyRoot\$deleted.dpapi")) 'deleting the encrypted job retired its local authorization'
    $orphan=[Guid]::NewGuid().ToString();[IO.File]::WriteAllBytes("$keyRoot\$orphan.dpapi",(New-Object byte[] 128))
    $reference=@($jobs|Where-Object{$_.archive_encryption})[0].archive_encryption.local_key.id
    $realKey="$keyRoot\$reference.dpapi"
    $realAcl=Get-Acl $realKey
    # Synthetic files created with WriteAllBytes inherit SYSTEM's default
    # descriptor, while real blob files have a pinned SYSTEM-only descriptor.
    # Give only our 128-byte zero fixtures the production descriptor.
    foreach($candidate in Get-ChildItem $keyRoot -Filter '*.dpapi'){
        if($candidate.Length -eq 128){
            $bytes=[IO.File]::ReadAllBytes($candidate.FullName)
            if(@($bytes|Where-Object{$_ -ne 0}).Count -eq 0){Set-Acl -LiteralPath $candidate.FullName -AclObject $realAcl}
        }
    }
    $oldLog="C:\Lab\Logs\own-backup-service-$([DateTime]::UtcNow.AddDays(-45).ToString('yyyy-MM-dd')).log"
    [IO.File]::WriteAllText($oldLog,'expired synthetic acceptance log')
    $null=Native 'C:\Lab\own-backup-service.exe' @('/start');ServiceWait 'Running';Start-Sleep -Seconds 4
    Check (-not(Test-Path "$keyRoot\$orphan.dpapi")) 'startup swept an unreferenced protected-key blob'
    Check (-not(Test-Path $oldLog)) 'startup removed a service log older than thirty days'
    $after=KeyHashes;$referenced=@($jobs|Where-Object{$_.archive_encryption}|ForEach-Object{[string]$_.archive_encryption.local_key.id+'.dpapi'});foreach($name in $keys.Keys){if($name -in $referenced){Check ($after[$name] -eq $keys[$name]) 'orphan sweep kept the real archive key byte-for-byte'}}
    Report 'post-storage' @{invalid_rows_refused=$true;duplicate_ids_refused=$true;revoked_key_absent=$true;orphan_swept=$true;old_log_removed=$true}
}
function BitLockerState {
    $volumes=@(Get-CimInstance -Namespace 'root/CIMV2/Security/MicrosoftVolumeEncryption' -ClassName Win32_EncryptableVolume|Where-Object{$_.DriveLetter -eq 'S:'})
    Check ($volumes.Count -eq 1) 'one disposable BitLocker volume identified through the native provider'
    $lock=Invoke-CimMethod -InputObject $volumes[0] -MethodName GetLockStatus
    $conversion=Invoke-CimMethod -InputObject $volumes[0] -MethodName GetConversionStatus
    Report 'bitlocker-provider-raw' @{lock_result=$lock;conversion_result=$conversion}
    Check ($lock.ReturnValue -eq 0) 'native BitLocker lock-status method succeeded'
    return @{device=$volumes[0].DeviceID;lock=[int]$lock.LockStatus;status=$(if($conversion.ReturnValue -eq 0){[int]$conversion.ConversionStatus}else{-1});conversion_error=$conversion.ReturnValue;percent=$conversion.EncryptionPercentage}
}
function PrepareBitLocker {
    Check ((Get-Volume -DriveLetter S).FileSystemLabel -eq 'ACCEPTSRC') 'BitLocker target is the disposable source volume'
    $password='Stand-Source-Password-Only-20261007';[IO.File]::WriteAllText("$work\source-password.txt",$password)
    $prior=BitLockerState
    if($prior.status -eq 1){Report 'bitlocker-source' $prior;return}
    $secure=ConvertTo-SecureString $password -AsPlainText -Force
    Enable-BitLocker -MountPoint 'S:' -PasswordProtector -Password $secure -EncryptionMethod XtsAes128 -UsedSpaceOnly -SkipHardwareTest | Out-Null
    $deadline=[DateTime]::UtcNow.AddMinutes(6)
    do{$state=Get-BitLockerVolume -MountPoint 'S:';if($state.VolumeStatus -eq 'FullyEncrypted'){break};Start-Sleep -Seconds 2}while([DateTime]::UtcNow -lt $deadline)
    Check ($state.VolumeStatus -eq 'FullyEncrypted') 'the disposable source finished XTS-AES-128 encryption'
    Lock-BitLocker -MountPoint 'S:' -ForceDismount
    $final=BitLockerState;Report 'bitlocker-source' $final;Check ($final.lock -eq 1) 'the BitLocker fixture is locked'
}
function PostBrokenAuthorization {
    $id=(Get-Content "$work\bitlocker-job-id.txt" -Raw -Encoding UTF8).Trim();$path='C:\ProgramData\OwnBackup\jobs.json'
    $null=Native 'C:\Lab\own-backup-service.exe' @('/stop');ServiceWait 'Stopped'
    $jobs=(Get-Content $path -Raw -Encoding UTF8|ConvertFrom-Json).PSObject.BaseObject;$job=@($jobs|Where-Object{$_.id -eq $id})[0]
    $keyPath="C:\ProgramData\OwnBackup\.own-backup-service-keys\$($job.archive_encryption.local_key.id).dpapi"
    $blob=[IO.File]::ReadAllBytes($keyPath);$wrong=$blob.Clone();$wrong[$wrong.Length-1]=$wrong[$wrong.Length-1] -bxor 1
    $before=@{};Get-ChildItem 'R:\stand\mixed' -Filter '*.obk'|ForEach-Object{$before[$_.Name]=(Get-FileHash $_.FullName).Hash}
    $previous=$job.last_run.id;$job.schedule.kind='daily';$job.schedule.time=(Get-Date).AddMinutes(1).ToString('HH:mm');$job.schedule.run_missed=$false;$job.verification.schedule.kind='none'
    JsonFile $path $jobs;[IO.File]::WriteAllBytes($keyPath,$wrong)
    try {
        $null=Native 'C:\Lab\own-backup-service.exe' @('/start');ServiceWait 'Running'
        $deadline=[DateTime]::UtcNow.AddMinutes(4)
        do{
            Start-Sleep -Seconds 2
            $current=@((Get-Content $path -Raw -Encoding UTF8|ConvertFrom-Json).PSObject.BaseObject|Where-Object{$_.id -eq $id})[0]
            if($current.last_run -and $current.last_run.id -ne $previous){break}
        }while([DateTime]::UtcNow -lt $deadline)
        Check ($current.last_run.id -ne $previous -and $current.last_run.status -eq 'error') 'a corrupt archive authorization fails a scheduled BitLocker-source backup even with its remembered source password'
        $after=@(Get-ChildItem 'R:\stand\mixed' -Filter '*.obk')
        Check ($after.Count -eq $before.Count) 'missing archive authorization created no plaintext or partial replacement'
        foreach($file in $after){Check ((Get-FileHash $file.FullName).Hash -eq $before[$file.Name]) 'failed authorization did not change an existing encrypted archive'}
        Report 'broken-authorization' @{run=$current.last_run;source_password_still_remembered=$true;new_images=0;old_archives_unchanged=$true}
    }finally{
        $null=Native 'C:\Lab\own-backup-service.exe' @('/stop');ServiceWait 'Stopped'
        [IO.File]::WriteAllBytes($keyPath,$blob)
        $jobs=(Get-Content $path -Raw -Encoding UTF8|ConvertFrom-Json).PSObject.BaseObject
        @($jobs|Where-Object{$_.id -eq $id})[0].schedule.kind='none';JsonFile $path $jobs
        $null=Native 'C:\Lab\own-backup-service.exe' @('/start');ServiceWait 'Running'
    }
    ScanProductOutput
}
function ScanProductOutput {
    # Scan only product output, not synthetic source files / fixture scripts.
    $secrets=@((Get-Content "$work\password.txt" -Raw -Encoding UTF8).Trim(),(Get-Content "$work\source-password.txt" -Raw -Encoding UTF8).Trim())
    Get-ChildItem $work -Filter '*-recovery.txt'|ForEach-Object{$secrets+=(Get-Content $_.FullName -Raw -Encoding UTF8).Trim()}
    $files=@(Get-ChildItem 'C:\Lab\Logs' -File -ErrorAction SilentlyContinue)+@(Get-ChildItem 'C:\ProgramData\OwnBackup' -File)
    foreach($file in $files){
        $reader=[IO.File]::Open($file.FullName,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::ReadWrite)
        try{$memory=[IO.MemoryStream]::new();$reader.CopyTo($memory);$text=[Text.Encoding]::UTF8.GetString($memory.ToArray());$memory.Dispose()}finally{$reader.Dispose()}
        foreach($secret in $secrets){if($text.Contains($secret)){throw "plaintext test secret found in product output: $($file.Name)"}}
    }
    $scratch=@(Get-ChildItem 'R:\stand' -File -Recurse|Where-Object{$_.Name -match '\.(partial|scratch|stage)|sealed-scratch|consolidating'})
    Check ($scratch.Count -eq 0) 'completed encrypted operations left no scratch or transaction artifacts'
    Check $true 'product logs and public runtime files contain neither plaintext archive/source passwords nor recovery keys'
    Report 'secrets-output' @{files_scanned=$files.Count;secrets_found=0;scratch_artifacts=0;fixture_blob_restored=$true;password_reauthorization_ui_not_tested=$true}
}
