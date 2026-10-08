# Silent uninstall by a second elevated local account, in a disposable VM.
. 'C:\Lab\acceptance-common.ps1'
$work='C:\Lab\installer-cross-account'
$mountedHive=$false
try {
    if(Test-Path $work){throw 'preserve the previous cross-account attempt'}
    New-Item -ItemType Directory $work|Out-Null
    Unregister-ScheduledTask -TaskName 'OwnBackupInstallerLive' -Confirm:$false -ErrorAction SilentlyContinue
    NoLogon
    $installed='C:\Program Files\OwnBackup'
    Check (Test-Path "$installed\uninstall.exe") 'the accepted live-upgrade package is installed'
    Check ((Get-Volume -DriveLetter T).FileSystemLabel -eq 'OBKLIVEARCH') 'backup storage is the disposable live-upgrade disk'
    $lab=Get-LocalUser -Name lab
    $sid=$lab.SID.Value
    if(-not(Test-Path "Registry::HKEY_USERS\$sid")) {
        & reg.exe load "HKU\$sid" 'C:\Users\lab\NTUSER.DAT'|Out-Null
        Check ($LASTEXITCODE -eq 0) 'the original fixture profile hive was opened'
        $mountedHive=$true
    }
    $run="Registry::HKEY_USERS\$sid\Software\Microsoft\Windows\CurrentVersion\Run"
    New-Item $run -Force|Out-Null
    $entry='"C:\Program Files\OwnBackup\own-backup-gui.exe" --tray'
    New-ItemProperty $run -Name OwnBackupTray -Value $entry -PropertyType String -Force|Out-Null
    $ownerStore='C:\Users\lab\AppData\Local\com.ownbackup.gui'
    New-Item -ItemType Directory -Force $ownerStore|Out-Null
    [IO.File]::WriteAllText("$ownerStore\cross-account.marker",'Original lab profile storage must survive another account.')
    $images=@(Get-ChildItem 'T:\live' -Filter '*.obk'|ForEach-Object{[ordered]@{name=$_.Name;sha256=(Get-FileHash $_.FullName).Hash}}|Sort-Object {$_.name})
    $jobsHash=(Get-FileHash 'C:\ProgramData\OwnBackup\jobs.json').Hash
    $keyHash=@(Get-ChildItem 'C:\ProgramData\OwnBackup\.own-backup-service-keys' -Filter '*.dpapi'|ForEach-Object{[ordered]@{name=$_.Name;sha256=(Get-FileHash $_.FullName).Hash}}|Sort-Object {$_.name})
    $password='OwnBackup_Lab_Admin_20261008!'
    $alternate=New-LocalUser -Name 'obk-alt-admin' -Password (ConvertTo-SecureString $password -AsPlainText -Force) -PasswordNeverExpires
    Add-LocalGroupMember -Group (Get-LocalGroup -SID 'S-1-5-32-544').Name -Member $alternate
    Check ($alternate.SID.Value -ne $sid) 'uninstall account differs from the original profile owner'
    $action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -ExecutionPolicy Bypass -File C:\Lab\acceptance-installer-cross-account-user.ps1'
    Register-ScheduledTask -TaskName 'OwnBackupCrossAccount' -Action $action -User "$env:COMPUTERNAME\obk-alt-admin" -Password $password -RunLevel Highest -Force|Out-Null
    Start-ScheduledTask 'OwnBackupCrossAccount'
    $deadline=[DateTime]::UtcNow.AddSeconds(180)
    while(-not(Test-Path "$work\alternate-result.json") -and [DateTime]::UtcNow -lt $deadline){Start-Sleep -Milliseconds 500}
    Check (Test-Path "$work\alternate-result.json") 'the alternate administrator completed the uninstall task'
    $other=Get-Content "$work\alternate-result.json" -Raw -Encoding UTF8|ConvertFrom-Json
    Report 'cross-account-alternate' $other
    Check ($other.pass -and $other.elevated -and $other.sid -eq $alternate.SID.Value) 'uninstall ran with the alternate elevated token'
    Check (-not(Get-Service OwnBackup -ErrorAction SilentlyContinue)) 'silent uninstall removed its registered service'
    Check (-not(Test-Path "$installed\own-backup-gui.exe")) 'installed program files were removed'
    Check ((Get-ItemProperty $run).OwnBackupTray -eq $entry) 'the original user retains its tray startup entry'
    Check (Test-Path "$ownerStore\cross-account.marker") 'the original user retains its GUI storage'
    Check ((Get-FileHash 'C:\ProgramData\OwnBackup\jobs.json').Hash -eq $jobsHash) 'silent uninstall preserves the job database'
    $after=@(Get-ChildItem 'T:\live' -Filter '*.obk'|ForEach-Object{[ordered]@{name=$_.Name;sha256=(Get-FileHash $_.FullName).Hash}}|Sort-Object {$_.name})
    $keysAfter=@(Get-ChildItem 'C:\ProgramData\OwnBackup\.own-backup-service-keys' -Filter '*.dpapi'|ForEach-Object{[ordered]@{name=$_.Name;sha256=(Get-FileHash $_.FullName).Hash}}|Sort-Object {$_.name})
    Check ((ConvertTo-Json -InputObject $images -Compress) -eq (ConvertTo-Json -InputObject $after -Compress)) 'all encrypted backup files are unchanged'
    Check ((ConvertTo-Json -InputObject $keyHash -Compress) -eq (ConvertTo-Json -InputObject $keysAfter -Compress)) 'protected local authorizations are unchanged'
    Report 'installer-cross-account-result' @{pass=$true;silent=$true;original_sid=$sid;alternate_sid=$alternate.SID.Value;original_storage_kept=$true;original_startup_kept=$true;alternate_startup_removed=$other.startup_removed;jobs_kept=$true;protected_keys_kept=$true;archive_hashes=$after;interactive_credential_prompt_tested=$false;remove_settings_yes_tested=$false}
    Say 'INSTALLER-CROSS-ACCOUNT-PASS'
}catch{Say "INSTALLER-CROSS-ACCOUNT-FAIL line=$($_.InvocationInfo.ScriptLineNumber) reason=$($_.Exception.Message)"}
finally {
    if($mountedHive){[GC]::Collect();& reg.exe unload "HKU\$sid"|Out-Null}
    shutdown.exe /s /t 5|Out-Null
}
