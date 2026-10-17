# System-OBK restore acceptance (CHECKS 1), guest side. One script, run by
# phase: at the first logon after Setup (install), then at every startup as
# SYSTEM through a scheduled task (backup, verify, logon, verify2).
#
#   install  - install the product silently, make the control data, record its
#              snapshot, schedule this script at startup, power off
#   backup   - format the store disk, turn on the lab user's logon, reboot
#   backup-rpc - (user session) create an encrypted bootable whole-disk job for
#              the system disk and run it, power off. The service answers only
#              the console user, so this half cannot run as SYSTEM. The phase is
#              set to 'verify' BEFORE the copy: the restored system wakes up there.
#   verify   - on the RESTORED disk: the snapshot matches, UEFI and Secure Boot,
#              the partition GUIDs, a new write; then a real logon
#   logon    - (user session) the logon itself, then a reboot
#   verify2  - the write survived the reboot; PASS, power off
#
# Everything is reported over COM1. ASCII only: PowerShell 5.1 reads a script
# without a byte-order mark in the ANSI code page.
param([switch]$User)
$ErrorActionPreference = 'Stop'
$lab = 'C:\Lab'
$work = 'C:\Lab\sys'
$data = 'C:\LabData'
$installed = 'C:\Program Files\OwnBackup'

function Say([string]$text) {
    $port = New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One'
    try {
        for ($attempt = 0; $attempt -lt 20; $attempt++) {
            try { $port.Open(); break } catch { if ($attempt -eq 19) { throw }; Start-Sleep -Milliseconds 100 }
        }
        $port.WriteLine($text)
    } finally { $port.Dispose() }
}
function Check([bool]$ok, [string]$text) { if (-not $ok) { throw "check failed: $text" }; Say "SYS-OK $text" }
function JsonFile([string]$path, $value) {
    [IO.File]::WriteAllText($path, (ConvertTo-Json -InputObject $value -Depth 20), [Text.UTF8Encoding]::new($false))
}
function Report([string]$name, $value) {
    $path = Join-Path $work "$name.json"
    JsonFile $path $value
    Say "SYS-REPORT $name $([Convert]::ToBase64String([IO.File]::ReadAllBytes($path)))"
}
function Phase([string]$value) { [IO.File]::WriteAllText("$work\phase.txt", $value) }
function Rpc([string]$name, $request) {
    $path = Join-Path $work "$name.request.json"
    JsonFile $path $request
    $out = Join-Path $work "$name.out"; $err = Join-Path $work "$name.err"
    $p = Start-Process "$lab\audit_autonomous.exe" -ArgumentList ('"' + $path + '"') -RedirectStandardOutput $out -RedirectStandardError $err -NoNewWindow -PassThru
    $null = $p.Handle
    if (-not $p.WaitForExit(3600000)) { $p.Kill(); throw "RPC timeout: $name" }
    $p.Refresh()
    $resultPath = [IO.Path]::ChangeExtension($path, 'result.json')
    $result = if (Test-Path $resultPath) { Get-Content $resultPath -Raw -Encoding UTF8 | ConvertFrom-Json } else { $null }
    if ($result) { Report $name $result }
    if ($p.ExitCode -ne 0) { throw "RPC failed: $name exit=$($p.ExitCode) $([IO.File]::ReadAllText($err))" }
    return $result.result
}
function WaitService {
    for ($i = 0; $i -lt 120; $i++) {
        $s = Get-Service OwnBackup -ErrorAction SilentlyContinue
        if ($s -and $s.Status -eq 'Running' -and (Test-Path 'C:\ProgramData\OwnBackup\service.token')) { Start-Sleep -Seconds 3; return }
        Start-Sleep -Seconds 1
    }
    throw 'the OwnBackup service did not come up'
}
function Sha([byte[]]$bytes) {
    $h = [Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($h.ComputeHash($bytes))).Replace('-', '') } finally { $h.Dispose() }
}

# --- Control data -----------------------------------------------------------
function MakeData {
    New-Item -ItemType Directory -Force "$data\files", "$data\acl" | Out-Null
    $rng = New-Object Random 20261016
    for ($i = 1; $i -le 40; $i++) {
        $bytes = New-Object byte[] ($rng.Next(256KB, 2MB))
        $rng.NextBytes($bytes)
        [IO.File]::WriteAllBytes(("$data\files\f{0:D3}.bin" -f $i), $bytes)
    }
    $big = New-Object byte[] (64MB); $rng.NextBytes($big)
    [IO.File]::WriteAllBytes("$data\big.bin", $big)
    # A second name for the same file.
    $null = & fsutil.exe hardlink create "$data\big-link.bin" "$data\big.bin"
    # An alternate data stream.
    Set-Content -LiteralPath "$data\ads.txt" -Value 'main stream'
    Set-Content -LiteralPath "$data\ads.txt" -Stream 'secret' -Value 'alternate stream content 2026-10-16'
    # A file with its own protected ACL: inheritance off, lab may only read.
    Set-Content -LiteralPath "$data\acl\private.txt" -Value 'private'
    # By SID: group names are translated on a Russian Windows.
    $null = & icacls.exe "$data\acl\private.txt" /inheritance:r /grant:r '*S-1-5-18:F' '*S-1-5-32-544:F' 'lab:R'
    if ($LASTEXITCODE -ne 0) { throw "icacls failed: $LASTEXITCODE" }
    # A name outside ASCII.
    $name = -join ([char[]]@(0x041A, 0x043E, 0x043F, 0x0438, 0x044F))
    Set-Content -LiteralPath "$data\$name.txt" -Value 'unicode name'
    New-Item -Path 'HKLM:\SOFTWARE\OwnBackupLab' -Force | Out-Null
    Set-ItemProperty -Path 'HKLM:\SOFTWARE\OwnBackupLab' -Name Marker -Value 'system-obk-2026-10-16'
}

function Snapshot {
    $items = @()
    foreach ($f in Get-ChildItem -LiteralPath $data -Recurse -Force) {
        $rel = $f.FullName.Substring($data.Length)
        if ($rel -eq '\after-restore.txt') { continue }
        $row = [ordered]@{ path = $rel; dir = $f.PSIsContainer; sddl = (Get-Acl -LiteralPath $f.FullName).Sddl }
        if (-not $f.PSIsContainer) {
            $row.length = $f.Length
            $row.sha256 = (Get-FileHash -LiteralPath $f.FullName -Algorithm SHA256).Hash
            $row.streams = @(Get-Item -LiteralPath $f.FullName -Stream * | Where-Object { $_.Stream -ne ':$DATA' } |
                ForEach-Object { [ordered]@{ name = $_.Stream; sha256 = (Sha ([byte[]](Get-Content -LiteralPath $f.FullName -Stream $_.Stream -Encoding Byte -Raw))) } })
            $row.links = @(& fsutil.exe hardlink list $f.FullName | Sort-Object)
        }
        $items += [pscustomobject]$row
    }
    $boot = Get-Partition | Where-Object { $_.IsBoot } | Select-Object -First 1
    $disk = $boot.DiskNumber
    return [ordered]@{
        disk_guid = "$((Get-Disk -Number $disk).Guid)".ToUpper()
        items = $items
        registry = (Get-ItemProperty -Path 'HKLM:\SOFTWARE\OwnBackupLab' -Name Marker).Marker
        partitions = @(Get-Partition -DiskNumber $disk | Sort-Object PartitionNumber |
            ForEach-Object { [ordered]@{ number = $_.PartitionNumber; guid = "$($_.Guid)".ToUpper(); type = "$($_.GptType)".ToUpper(); offset = $_.Offset; size = $_.Size } })
    }
}

# PowerShell 5.1 may write an array as {"value": [...], "Count": n}; read both.
function Items($snap) {
    $list = @($snap.items)
    if ($list.Count -eq 1 -and $list[0].PSObject.Properties['value']) { $list = @($list[0].value) }
    return ,$list
}

function Compare-Snapshot($before, $after) {
    $bi = Items $before; $ai = Items $after
    Say "SYS-SNAPSHOT before=$($bi.Count) after=$($ai.Count) first=$(ConvertTo-Json -InputObject $bi[0] -Compress -Depth 4)"
    $b = @{}; foreach ($i in $bi) { if ($null -ne $i.path) { $b[[string]$i.path] = $i } }
    $a = @{}; foreach ($i in $ai) { if ($null -ne $i.path) { $a[[string]$i.path] = $i } }
    Check ($b.Count -eq $bi.Count -and $b.Count -gt 0) "every source item has a path ($($b.Count) of $($bi.Count))"
    Check ($b.Count -eq $a.Count) "same number of control items ($($b.Count))"
    foreach ($k in $b.Keys) {
        Check $a.ContainsKey($k) "present: $k"
        $x = $b[$k]; $y = $a[$k]
        Check ($x.sddl -eq $y.sddl) "same ACL: $k"
        if (-not $x.dir) {
            Check ($x.sha256 -eq $y.sha256 -and $x.length -eq $y.length) "same content: $k"
            Check ((ConvertTo-Json -InputObject @($x.streams) -Compress) -eq (ConvertTo-Json -InputObject @($y.streams) -Compress)) "same streams: $k"
            Check ((@($x.links) -join '|') -eq (@($y.links) -join '|')) "same hard links: $k"
        }
    }
    Check ($before.registry -eq $after.registry) 'registry marker restored'
    $bp = @{}; foreach ($p in $before.partitions) { $bp[[int]$p.number] = $p }
    foreach ($p in $after.partitions) {
        if ($bp.ContainsKey([int]$p.number)) {
            Check ($bp[[int]$p.number].guid -eq $p.guid) "partition $($p.number) keeps its GUID"
        }
    }
}

# The control data again, from the same seed: an expectation that does not
# depend on anything saved on the disk being checked.
function Verify-Generated {
    $rng = New-Object Random 20261016
    for ($i = 1; $i -le 40; $i++) {
        $bytes = New-Object byte[] ($rng.Next(256KB, 2MB))
        $rng.NextBytes($bytes)
        $path = "$data\files\f{0:D3}.bin" -f $i
        Check ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -eq (Sha $bytes)) "content of $path"
    }
    $big = New-Object byte[] (64MB); $rng.NextBytes($big)
    $want = Sha $big
    Check ((Get-FileHash -LiteralPath "$data\big.bin" -Algorithm SHA256).Hash -eq $want) 'content of big.bin'
    Check ((Get-FileHash -LiteralPath "$data\big-link.bin" -Algorithm SHA256).Hash -eq $want) 'content of big-link.bin'
    $links = @(& fsutil.exe hardlink list "$data\big.bin")
    Check ($links.Count -eq 2 -and ($links -join '|') -match 'big-link\.bin') "big.bin and big-link.bin are one file ($($links -join ', '))"
    Check ((Get-Content -LiteralPath "$data\ads.txt" -Stream 'secret') -eq 'alternate stream content 2026-10-16') 'alternate data stream intact'
    Check ((Get-Content -LiteralPath "$data\ads.txt") -eq 'main stream') 'main stream intact'
    $acl = Get-Acl -LiteralPath "$data\acl\private.txt"
    Check $acl.AreAccessRulesProtected 'private.txt keeps inheritance off'
    $rules = @($acl.Access | ForEach-Object {
        $sid = try { $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value } catch { "$($_.IdentityReference)" }
        "$sid=$($_.FileSystemRights)" })
    $labSid = (New-Object Security.Principal.NTAccount "$env:COMPUTERNAME\lab").Translate([Security.Principal.SecurityIdentifier]).Value
    Check ($rules.Count -eq 3) "private.txt has exactly three rules ($($rules -join '; '))"
    Check ($rules -contains 'S-1-5-18=FullControl' -and $rules -contains 'S-1-5-32-544=FullControl') 'SYSTEM and Administrators keep full control'
    Check (@($rules | Where-Object { $_ -like "$labSid=Read*" }).Count -eq 1) 'lab keeps read only'
    $name = -join ([char[]]@(0x041A, 0x043E, 0x043F, 0x0438, 0x044F))
    Check ((Get-Content -LiteralPath "$data\$name.txt") -eq 'unicode name') 'file with a Cyrillic name intact'
    Check ((Get-ItemProperty -Path 'HKLM:\SOFTWARE\OwnBackupLab' -Name Marker).Marker -eq 'system-obk-2026-10-16') 'registry marker restored'
    $expected = Get-Content "$lab\expected-partitions.json" -Raw -Encoding UTF8 | ConvertFrom-Json
    $bootDisk = (Get-Partition | Where-Object { $_.IsBoot } | Select-Object -First 1).DiskNumber
    $now = @{}; Get-Partition -DiskNumber $bootDisk | ForEach-Object { $now[[int]$_.PartitionNumber] = $_ }
    foreach ($p in @($expected)) {
        $here = $now[[int]$p.number]
        Check ($here -and "$($here.Guid)".ToUpper() -eq "$($p.guid)".ToUpper()) "partition $($p.number) keeps GUID $($p.guid)"
        Check ($here.Offset -eq $p.offset -and $here.Size -eq $p.size) "partition $($p.number) keeps offset and size"
    }
}

function AutoLogon {
    $answer = New-Object Xml.XmlDocument; $answer.Load("$lab\autounattend.xml")
    $password = $answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    $key = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $key DefaultUserName 'lab'; Set-ItemProperty $key DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $key DefaultPassword $password; Set-ItemProperty $key AutoAdminLogon '1'; Set-ItemProperty $key AutoLogonCount 1
    $startup = Join-Path ([Environment]::GetFolderPath('CommonStartup')) 'OwnBackupSysLogon.cmd'
    [IO.File]::WriteAllText($startup, "@echo off`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File $lab\sysrestore-guest.ps1 -User`r`n", [Text.Encoding]::ASCII)
}
function NoLogon {
    $key = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $key AutoAdminLogon '0'
    Remove-ItemProperty $key DefaultPassword -ErrorAction SilentlyContinue
    Remove-Item (Join-Path ([Environment]::GetFolderPath('CommonStartup')) 'OwnBackupSysLogon.cmd') -ErrorAction SilentlyContinue
}

$phase = 'install'
try {
    if (Test-Path "$work\phase.txt") { $phase = (Get-Content "$work\phase.txt" -Raw).Trim() }
    Say "SYS-BEGIN phase=$phase utc=$([DateTime]::UtcNow.ToString('o')) user=$([Security.Principal.WindowsIdentity]::GetCurrent().Name)"

    if ($phase -eq 'install') {
        $media = Get-PSDrive -PSProvider FileSystem | ForEach-Object { Join-Path $_.Root 'lab' } |
            Where-Object { Test-Path -LiteralPath (Join-Path $_ 'sys-marker.txt') } | Select-Object -First 1
        Check ([bool]$media) "lab media found at $media"
        New-Item -ItemType Directory -Force $lab, $work | Out-Null
        Copy-Item -Path (Join-Path $media '*') -Destination $lab -Recurse -Force
        Get-ChildItem $lab -Recurse -File | ForEach-Object { $_.IsReadOnly = $false }

        $setup = Start-Process "$lab\own-backup-install.exe" -ArgumentList '/S' -Wait -PassThru
        Check ($setup.ExitCode -eq 0) "installer exit $($setup.ExitCode)"
        WaitService
        Check (Test-Path "$installed\own-backup-service.exe") 'product installed'

        # Hibernation off: a hiberfil the size of RAM only bloats the disks.
        $null = & powercfg.exe /h off
        MakeData
        $snap = Snapshot
        JsonFile "$work\snapshot-source.json" $snap
        # Not 'snapshot-source': Report writes $work\<name>.json, and that name
        # overwrote the full snapshot with this summary (2026-10-16).
        Report 'snapshot-summary' @{ items = $snap.items.Count; partitions = $snap.partitions; registry = $snap.registry }
        Report 'firmware-source' @{ secure_boot = (Confirm-SecureBootUEFI); bcd = (& bcdedit.exe /enum '{current}' | Out-String) }

        # From now on this script runs at every startup as SYSTEM, after
        # refreshing C:\Lab from the CD when one is inserted.
        $boot = @'
$media = $null
for ($i = 0; $i -lt 30 -and -not $media; $i++) {
    $media = Get-PSDrive -PSProvider FileSystem | Where-Object { $_.Root -ne 'C:\' } |
        ForEach-Object { Join-Path $_.Root 'lab' } |
        Where-Object { Test-Path -LiteralPath (Join-Path $_ 'sys-marker.txt') } | Select-Object -First 1
    if (-not $media) { Start-Sleep -Seconds 2 }
}
if ($media) {
    Get-ChildItem 'C:\Lab' -File | ForEach-Object { try { $_.IsReadOnly = $false } catch { } }
    Copy-Item -Path (Join-Path $media '*') -Destination 'C:\Lab' -Recurse -Force -ErrorAction SilentlyContinue
    Get-ChildItem 'C:\Lab' -File | ForEach-Object { try { $_.IsReadOnly = $false } catch { } }
}
& 'C:\Lab\sysrestore-guest.ps1'
'@
        [IO.File]::WriteAllText("$lab\sys-boot.ps1", $boot, [Text.Encoding]::ASCII)
        schtasks.exe /Create /TN 'OwnBackupSysRestore' /SC ONSTART /RU SYSTEM /RL HIGHEST /F `
            /TR "powershell -NoProfile -ExecutionPolicy Bypass -File $lab\sys-boot.ps1" | Out-Null
        NoLogon
        Phase 'backup'
        Say 'SYS-INSTALL-DONE'
        shutdown.exe /s /t 5 | Out-Null
        return
    }

    # The phase says 'verify' from before the copy started. The restored system
    # boots alone; a store disk beside it means this is still the source and
    # the copy never finished — retry it.
    # The user half does two things and nothing else.
    if ($User -and $phase -notin @('backup-rpc', 'logon')) { Say "SYS-WAIT phase=$phase user"; return }

    if (-not $User -and $phase -eq 'verify' -and (Get-Volume -FileSystemLabel OBKSTORE -ErrorAction SilentlyContinue)) {
        Say 'SYS-RETRY the store disk is attached: the copy did not finish, back to backup'
        $phase = 'backup'; Phase 'backup'
    }

    if ($phase -eq 'backup') {
        WaitService
        # A retried run finds the store it made last time.
        if (-not (Get-Volume -FileSystemLabel OBKSTORE -ErrorAction SilentlyContinue)) {
            $raw = @(Get-Disk | Where-Object { $_.PartitionStyle -eq 'RAW' })
            Check ($raw.Count -eq 1) 'one blank store disk attached'
            Initialize-Disk -Number $raw[0].Number -PartitionStyle GPT
            $part = New-Partition -DiskNumber $raw[0].Number -UseMaximumSize -DriveLetter S
            Format-Volume -Partition $part -FileSystem NTFS -NewFileSystemLabel OBKSTORE -Confirm:$false | Out-Null
        }
        $store = Get-Volume -FileSystemLabel OBKSTORE
        if ($store.DriveLetter -ne 'S') { Get-Partition -Volume $store | Set-Partition -NewDriveLetter S }
        Check (Test-Path 'S:') 'store disk mounted at S:'
        New-Item -ItemType Directory -Force 'S:\obk-lab', 'S:\obk' | Out-Null
        # Synthetic secret of a throwaway VM; WinPE reads it from the store disk.
        [IO.File]::WriteAllText('S:\obk-lab\password.txt', 'ObkSys-Lab-2026-x7')
        [IO.File]::WriteAllText('S:\obk-lab\store.marker', 'Own Backup system restore acceptance store')
        AutoLogon
        Phase 'backup-rpc'
        Say 'SYS-BACKUP-PREPARED rebooting into the lab user'
        shutdown.exe /r /t 5 | Out-Null
        return
    }

    if ($User -and $phase -eq 'backup-rpc') {
        Phase 'verify'
        $job = Rpc 'create-job' @{ op = 'create-system-disk-job'; encrypted = $true;
            password_file = 'S:\obk-lab\password.txt'; recovery_file = 'S:\obk-lab\recovery.txt';
            job = @{ name = 'SysLab'; dest_folder = 'S:\obk' } }
        $run = Rpc 'run' @{ op = 'run'; id = $job.id; timeout_secs = 3600 }
        Check ($run.status -eq 'success') "backup run succeeded: $($run.error)"
        Check ($run.image_path -like 'S:\obk\*.obk') "encrypted image written: $($run.image_path)"
        Report 'backup-result' @{ image = $run.image_path; kind = $run.backup_kind; bytes = $run.bytes_copied; secs = $run.duration_secs }
        Say 'SYS-BACKUP-DONE'
        shutdown.exe /s /t 5 | Out-Null
        return
    }

    if ($phase -eq 'verify') {
        $lab = 'C:\Lab'
        Verify-Generated
        # The full snapshot as well, where one was kept (later runs).
        $before = Get-Content "$work\snapshot-source.json" -Raw -Encoding UTF8 | ConvertFrom-Json
        if (@(Items $before).Count -gt 1) { Compare-Snapshot $before (Snapshot) } else { Say 'SYS-NOTE no full source snapshot on this disk; checked against the generator' }
        Check ((Confirm-SecureBootUEFI) -eq $true) 'booted with Secure Boot on'
        $bcd = & bcdedit.exe /enum '{current}' | Out-String
        Check ($bcd -match 'winload\.efi') 'booted through UEFI (winload.efi)'
        Report 'firmware-restored' @{ bcd = $bcd; firmware = (& bcdedit.exe /enum firmware | Out-String) }
        Set-Content -LiteralPath "$data\after-restore.txt" -Value "written after restore $([DateTime]::UtcNow.ToString('o'))"
        AutoLogon
        Phase 'logon'
        Say 'SYS-VERIFY-DONE rebooting into a logon'
        shutdown.exe /r /t 5 | Out-Null
        return
    }

    if ($User -and $phase -eq 'logon') {
        # Not $user: PowerShell names ignore case, and that is the -User switch.
        $who = [Security.Principal.WindowsIdentity]::GetCurrent().Name
        $explorer = @(Get-Process explorer -ErrorAction SilentlyContinue).Count
        Say "SYS-LOGON user=$who explorer=$explorer"
        JsonFile "$work\logon.json" @{ user = $who; explorer = $explorer; utc = [DateTime]::UtcNow.ToString('o') }
        Phase 'verify2'
        shutdown.exe /r /t 5 | Out-Null
        return
    }
    # Everything else belongs to the other half; wait for it.
    if ($phase -in @('backup-rpc', 'logon')) {
        # AutoLogonCount is one logon, and Winlogon turns it off once used: a
        # retried phase arms it again — which only counts from the next boot.
        $armed = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon').AutoAdminLogon -eq '1'
        if (-not $armed) {
            AutoLogon
            Say "SYS-REARM phase=$phase rebooting into the lab user"
            shutdown.exe /r /t 5 | Out-Null
            return
        }
        Say "SYS-WAIT phase=$phase"
        return
    }

    if ($phase -eq 'verify2') {
        Check (Test-Path "$data\after-restore.txt") 'the write after restore survived a reboot'
        $logon = Get-Content "$work\logon.json" -Raw | ConvertFrom-Json
        Check ($logon.user -like '*\lab') "the lab user logged on ($($logon.user))"
        NoLogon
        Phase 'done'
        Report 'result' @{ pass = $true; logon = $logon }
        Say 'SYS-PASS'
        shutdown.exe /s /t 5 | Out-Null
        return
    }
    if ($phase -eq 'done') { Say 'SYS-ALREADY-DONE'; shutdown.exe /s /t 5 | Out-Null; return }
    throw "unknown phase $phase"
} catch {
    Say "SYS-FAIL phase=$phase line=$($_.InvocationInfo.ScriptLineNumber) reason=$($_.Exception.Message)"
    # Plain lines, each on its own: one stuck call must not take the rest.
    try { Say "SYS-DIAG service=$((Get-Service OwnBackup -ErrorAction SilentlyContinue).Status)" } catch { }
    try {
        Get-ChildItem $work -Filter '*.err' -ErrorAction SilentlyContinue |
            ForEach-Object { Say "SYS-DIAG helper $($_.Name): $(([IO.File]::ReadAllText($_.FullName)) -replace '\s+', ' ')" }
    } catch { }
    try {
        $log = Get-ChildItem "$installed\Logs" -Filter 'own-backup-service-*.log' -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime | Select-Object -Last 1
        if ($log) {
            $copy = Join-Path $env:TEMP 'service-tail.log'
            Copy-Item -LiteralPath $log.FullName $copy -Force
            if (Test-Path 'S:\obk-lab') { Copy-Item -LiteralPath $log.FullName 'S:\obk-lab\' -Force }
            Get-Content -LiteralPath $copy -Tail 60 | ForEach-Object { Say "SYS-LOG $_" }
        } else { Say 'SYS-DIAG no service log' }
    } catch { Say "SYS-DIAG log unreadable: $($_.Exception.Message)" }
    try {
        & wevtutil.exe qe Application /q:"*[System[(EventID=1000 or EventID=1001 or EventID=1026)]]" /c:4 /rd:true /f:text 2>&1 |
            Select-Object -First 80 | ForEach-Object { Say "SYS-EVT $_" }
    } catch { }
    shutdown.exe /s /t 10 | Out-Null
}
