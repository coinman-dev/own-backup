# Disposable QEMU acceptance: prepare a locked BitLocker data volume as SYSTEM,
# save its password from the active lab session, then let the service scheduler
# back it up after a reboot with nobody logged on.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

function RunNative([string]$Command) {
    $output = (cmd.exe /d /c "$Command 2>&1" | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) { throw "exit=$LASTEXITCODE command=$Command output=$output" }
    return $output
}

function SetLabAutoLogon {
    $answer = New-Object System.Xml.XmlDocument
    $answer.Load('C:\Lab\autounattend.xml')
    $password = $answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty -Path $winlogon -Name DefaultUserName -Value 'lab'
    Set-ItemProperty -Path $winlogon -Name DefaultDomainName -Value $env:COMPUTERNAME
    Set-ItemProperty -Path $winlogon -Name DefaultPassword -Value $password
    Set-ItemProperty -Path $winlogon -Name AutoAdminLogon -Value '1'
    Set-ItemProperty -Path $winlogon -Name AutoLogonCount -Value 1
    $startup = [Environment]::GetFolderPath('CommonStartup')
    [IO.File]::WriteAllText((Join-Path $startup 'OwnBackupH01Unattended.cmd'),
        "@echo off`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-h01-unattended-user.ps1`r`n")
}

function PrepareVolume {
    $existingDisk = @(Get-Disk | Where-Object {
        -not $_.IsBoot -and -not $_.IsSystem -and
        $_.Size -ge 3GB -and $_.Size -le 5GB -and $_.PartitionStyle -eq 'GPT'
    })
    if ($existingDisk.Count -eq 1 -and (Test-Path 'C:\Lab\audit-h01-control.sha256')) {
        $partition = @(Get-Partition -DiskNumber $existingDisk[0].Number | Where-Object { $_.DriveLetter })
        if ($partition.Count -ne 1) { throw 'existing test disk has no unique drive letter' }
        $volume = "$($partition[0].DriveLetter):"
        Say "H01-NIGHT-RESUME volume=$volume disk=$($existingDisk[0].Number)"
        return $volume
    }
    $disk = @(Get-Disk | Where-Object {
        -not $_.IsBoot -and -not $_.IsSystem -and
        $_.Size -ge 3GB -and $_.Size -le 5GB -and $_.PartitionStyle -eq 'RAW'
    })
    if ($disk.Count -ne 1) { throw "expected one blank 4-GiB test disk, found $($disk.Count)" }
    Initialize-Disk -Number $disk[0].Number -PartitionStyle GPT | Out-Null
    $partition = New-Partition -DiskNumber $disk[0].Number -UseMaximumSize -AssignDriveLetter
    Format-Volume -Partition $partition -FileSystem NTFS -NewFileSystemLabel H01BL -Confirm:$false | Out-Null
    $letter = [string](Get-Partition -DiskNumber $disk[0].Number -PartitionNumber $partition.PartitionNumber).DriveLetter
    $volume = "${letter}:"
    Say "H01-NIGHT-VOLUME volume=$volume disk=$($disk[0].Number) size=$($partition.Size)"

    $control = "$volume\h01-control.bin"
    $file = [IO.File]::Create($control)
    $bytes = New-Object byte[] 1048576
    $rng = New-Object Random 20260929
    for ($n = 0; $n -lt 8; $n++) { $rng.NextBytes($bytes); $file.Write($bytes, 0, $bytes.Length) }
    $file.Close()
    $hash = (Get-FileHash -LiteralPath $control -Algorithm SHA256).Hash
    [IO.File]::WriteAllText('C:\Lab\audit-h01-control.sha256', $hash)
    Say "H01-NIGHT-CONTROL sha256=$hash bytes=8388608"

    $password = ConvertTo-SecureString 'Str0ng-Lab-Passphrase-2026' -AsPlainText -Force
    Enable-BitLocker -MountPoint $volume -PasswordProtector -Password $password `
        -EncryptionMethod XtsAes128 -SkipHardwareTest -ErrorAction Stop | Out-Null
    for ($tick = 0; $tick -lt 240; $tick++) {
        $state = Get-BitLockerVolume -MountPoint $volume
        if ($state.VolumeStatus -eq 'FullyEncrypted') { break }
        if ($tick % 12 -eq 0) { Say "H01-NIGHT-ENCRYPT status=$($state.VolumeStatus) percent=$($state.EncryptionPercentage)" }
        Start-Sleep -Seconds 2
    }
    $state = Get-BitLockerVolume -MountPoint $volume
    if ($state.VolumeStatus -ne 'FullyEncrypted') { throw "BitLocker conversion did not finish: $($state.VolumeStatus)" }
    Lock-BitLocker -MountPoint $volume -ForceDismount -ErrorAction Stop | Out-Null
    Say "H01-NIGHT-LOCK status=$((Get-BitLockerVolume -MountPoint $volume).LockStatus)"
    return $volume
}

function SayServiceLog {
    $log = Get-ChildItem 'C:\Lab\Logs' -Filter 'own-backup-service-*.log' -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $log) { return }
    foreach ($line in Get-Content -LiteralPath $log.FullName | Select-Object -Last 18) {
        $plain = $line -replace '[^\x20-\x7E]', '?'
        Say "H01-NIGHT-SERVICE-LINE $($plain.Substring(0, [Math]::Min(450, $plain.Length)))"
    }
}

$ErrorActionPreference = 'Stop'
$setup = 'C:\Lab\audit-h01-unattended.setup'
$ready = 'C:\Users\lab\AppData\Local\audit-h01-unattended.ready'
$imageReady = 'C:\Users\lab\AppData\Local\audit-h01-unattended-image-ready'
try {
    if (-not (Test-Path -LiteralPath $setup)) {
        Say "H01-NIGHT-PREPARE $([DateTime]::UtcNow.ToString('o'))"
        $volume = PrepareVolume
        $wanted = 'BB06982EBE7B68130E7326F204F1A1F54B92228310DB682AF2B4AE17DD116903'
        $installed = (Get-FileHash 'C:\Lab\own-backup-service.exe' -Algorithm SHA256).Hash
        if ($installed -ne $wanted) {
            RunNative 'C:\Lab\own-backup-service-h01.exe /update' | Out-Null
            $installed = (Get-FileHash 'C:\Lab\own-backup-service.exe' -Algorithm SHA256).Hash
        }
        Say "H01-NIGHT-SERVICE sha256=$installed"
        if ($installed -ne $wanted) { throw 'installed service differs from H01 migration build' }
        [IO.File]::WriteAllText($setup, $volume)
        SetLabAutoLogon
        Say 'H01-NIGHT-SETUP-READY USER-LOGIN'
        shutdown.exe /r /t 5 | Out-Null
        return
    }
    if (-not (Test-Path -LiteralPath $ready)) {
        Say 'H01-NIGHT-WAITING-FOR-USER-RPC'
        return
    }
    if (Test-Path -LiteralPath $imageReady) {
        Say 'H01-NIGHT-WAITING-FOR-USER-CONTENT-CHECK'
        return
    }

    Say "H01-NIGHT-VERIFY $([DateTime]::UtcNow.ToString('o'))"
    $explorer = @(Get-Process explorer -ErrorAction SilentlyContinue).Count
    Say "H01-NIGHT-CONTEXT user=$([Security.Principal.WindowsIdentity]::GetCurrent().Name) explorer=$explorer"
    if ($explorer -ne 0) { throw 'an interactive user is logged on during unattended backup' }
    $volume = [IO.File]::ReadAllText($setup)
    $state = Get-BitLockerVolume -MountPoint $volume -ErrorAction SilentlyContinue
    Say "H01-NIGHT-SOURCE-LOCK $($state.LockStatus)"
    $id = [IO.File]::ReadAllText('C:\Users\lab\AppData\Local\audit-h01-unattended-job-id.txt').Trim()
    $boot = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime()
    $run = $null
    for ($tick = 0; $tick -lt 300; $tick++) {
        try {
            $jobs = @(Get-Content 'C:\ProgramData\OwnBackup\jobs.json' -Raw | ConvertFrom-Json)
            $job = @($jobs | Where-Object { $_.id -eq $id }) | Select-Object -First 1
            if ($job -and $job.last_run -and $job.last_run.status -in @('success', 'error', 'cancelled')) {
                $candidate = $job.last_run
                if ([DateTime]::Parse($candidate.started_at).ToUniversalTime() -ge $boot) {
                    $run = $candidate
                    break
                }
            }
        } catch { }
        if ($tick % 15 -eq 0) { Say "H01-NIGHT-WAIT tick=$tick" }
        Start-Sleep -Seconds 2
    }
    if (-not $run) { throw 'scheduler did not finish a new run in ten minutes' }
    Say "H01-NIGHT-RUN status=$($run.status) kind=$($run.backup_kind) bytes=$($run.bytes_copied)"
    if ($run.status -ne 'success') { throw "scheduled backup failed: $($run.error)" }
    $images = @($run.images)
    if ($images.Count -ne 1) { throw "expected one source image, got $($images.Count)" }
    $image = [string]$images[0].image_path
    if (-not (Test-Path -LiteralPath $image)) { throw "image missing: $image" }
    Say "H01-NIGHT-IMAGE path=$image bytes=$((Get-Item -LiteralPath $image).Length) sha256=$((Get-FileHash -LiteralPath $image -Algorithm SHA256).Hash)"
    $log = Get-ChildItem 'C:\Lab\Logs' -Filter 'own-backup-service-*.log' |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    $fired = @(Get-Content -LiteralPath $log.FullName | Where-Object { $_ -match 'Scheduler: startup event fired' }).Count
    Say "H01-NIGHT-SCHEDULER-FIRED count=$fired"
    if ($fired -lt 1) { throw 'scheduler startup event was not logged' }
    [IO.File]::WriteAllText('C:\Users\lab\AppData\Local\audit-h01-unattended-image.txt', $image)
    [IO.File]::WriteAllText($imageReady, 'ready')
    Say 'H01-NIGHT-BACKUP-PASS CONTENT-CHECK-NEXT'
    SayServiceLog
    SetLabAutoLogon
    shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "H01-NIGHT-FAIL id=$($_.FullyQualifiedErrorId) line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    SayServiceLog
}
