# SYSTEM: prepare the real split-ESP fixture and exercise CLI refusals.
function Say([string]$Text) {
    try { $p=New-Object System.IO.Ports.SerialPort 'COM1',115200,'None',8,'One';$p.Open();$p.WriteLine($Text);$p.Close() } catch { }
}
function Native([string]$Text) {
    $output=cmd.exe /d /c "$Text 2>&1" | Out-String
    $code=$LASTEXITCODE
    if ($code -ne 0) { throw "native exit=$code command=$Text output=$output" }
    return $output
}
function Diskpart([string]$Text) {
    [IO.File]::WriteAllText('C:\Lab\a06\diskpart.txt',$Text)
    $null=Native 'diskpart.exe /s C:\Lab\a06\diskpart.txt'
}
function HashRange([int]$Disk,[long]$Offset,[long]$Length) {
    $file=[IO.FileStream]::new("\\.\PhysicalDrive$Disk",[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::ReadWrite)
    $hash=[Security.Cryptography.SHA256]::Create();$buffer=[byte[]]::new(1MB)
    try {
        $null=$file.Seek($Offset,[IO.SeekOrigin]::Begin);$left=$Length
        while ($left -gt 0) {
            $want=[int][Math]::Min($left,$buffer.Length);$read=$file.Read($buffer,0,$want)
            if ($read -le 0) { throw 'raw hash ended early' }
            $null=$hash.TransformBlock($buffer,0,$read,$buffer,0);$left-=$read
        }
        $null=$hash.TransformFinalBlock([byte[]]::new(0),0,0)
        return ([BitConverter]::ToString($hash.Hash)).Replace('-','')
    } finally { $file.Dispose();$hash.Dispose() }
}
function Snapshot {
    $c=Get-Partition -DriveLetter C
    return @{kernel=(Get-FileHash 'C:\Windows\System32\kernel32.dll' -Algorithm SHA256).Hash;
        system_head=(HashRange 0 0 1MB);system_boot=(HashRange 0 $c.Offset 64KB);esp=(HashRange 1 0 512MB)}
}
function RemountSpare {
    Set-Disk -Number 2 -IsOffline $false
    $parts=@(Get-Partition -DiskNumber 2 | Where-Object {$_.Size -eq 64MB -and $_.GptType -eq '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}'})
    if ($parts.Count -ne 1) { throw 'unique restored NTFS fixture missing' }
    $p=$parts[0]
    if ($p.DriveLetter -ne 'T') { Add-PartitionAccessPath -DiskNumber 2 -PartitionNumber $p.PartitionNumber -AccessPath 'T:\' }
    for ($tick=0;$tick -lt 30 -and -not (Test-Path 'T:\a06-control.bin');$tick++) { Start-Sleep -Seconds 1 }
    if (-not (Test-Path 'T:\a06-control.bin')) { throw 'restored spare volume did not mount' }
}
$ErrorActionPreference='Stop'
[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false)
$OutputEncoding=[Text.UTF8Encoding]::new($false)
try {
    New-Item -ItemType Directory -Force 'C:\Lab\a06' | Out-Null
    if (Test-Path 'C:\Lab\a06\stage.ready') {
        $phase=(Get-Content 'C:\Lab\a06\phase.txt' -Raw).Trim()
        $login='HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
        $settings=Get-ItemProperty $login
        if ($phase -ne 'reboot-proof' -and ($settings.AutoAdminLogon -ne '1' -or [int]$settings.AutoLogonCount -lt 1)) {
            $answer=New-Object Xml.XmlDocument;$answer.Load('C:\Lab\autounattend.xml')
            $password=$answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
            Set-ItemProperty $login DefaultUserName 'lab';Set-ItemProperty $login DefaultDomainName $env:COMPUTERNAME
            Set-ItemProperty $login DefaultPassword $password;Set-ItemProperty $login AutoAdminLogon '1';Set-ItemProperty $login AutoLogonCount 3
            Say 'A06-LOGIN-RESET REBOOT'
            shutdown.exe /r /t 5 | Out-Null
            return
        }
        if ((Get-Content 'C:\Lab\a06\phase.txt' -Raw).Trim() -ne 'reboot-proof' -and -not (Test-Path 'C:\Lab\a06\strict-rpc-v2.ready')) {
            foreach ($pattern in @('rpc-*.json','rpc-*.marker','remounted-*.marker','positive-ready.marker')) { Remove-Item "C:\Lab\a06\$pattern" -Force -ErrorAction SilentlyContinue }
            [IO.File]::WriteAllText('C:\Lab\a06\phase.txt','rpc')
            [IO.File]::WriteAllText('C:\Lab\a06\strict-rpc-v2.ready','ready')
        }
        Start-Process powershell.exe -ArgumentList '-NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-a06-watch.ps1' -WindowStyle Hidden | Out-Null
        return
    }
    Say "A06-STAGE-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    $inventory=@{disks=@(Get-Disk | Select-Object Number,Size,Guid,IsBoot,IsSystem,PartitionStyle);partitions=@(Get-Partition | Select-Object DiskNumber,PartitionNumber,Size,Offset,Guid,DriveLetter,GptType)}
    [IO.File]::WriteAllText('C:\Lab\a06\inventory.json',($inventory|ConvertTo-Json -Depth 5),([Text.UTF8Encoding]::new($false)))
    Say "A06-INVENTORY $($inventory|ConvertTo-Json -Depth 5 -Compress)"
    foreach ($script in @('audit-a06-user.ps1','audit-a06-watch.ps1')) {
        $errors=$null;$null=[Management.Automation.Language.Parser]::ParseFile("C:\Lab\$script",[ref]$null,[ref]$errors)
        if ($errors) { throw "script syntax invalid $script $errors" }
    }
    $null=Native 'C:\Lab\own-backup-service.exe /stop'
    [IO.File]::WriteAllText('C:\ProgramData\OwnBackup\jobs.json','[]')
    $null=Native 'C:\Lab\own-backup-service-a06.exe /update'
    Say "A06-SERVICE sha256=$((Get-FileHash 'C:\Lab\own-backup-service.exe' -Algorithm SHA256).Hash)"
    $mount=Native 'mountvol.exe S: /s'
    Say "A06-ESP-MOUNT path=S: native=true output=$mount"
    if (-not (Test-Path 'S:\EFI\Microsoft\Boot\bootmgfw.efi')) { throw 'active ESP is not accessible as S:' }
    $spare=Get-Disk -Number 2
    if ($spare.Size -ne 256MB -or $spare.IsBoot -or $spare.IsSystem) { throw 'disk 2 is not the disposable spare' }
    if ($spare.PartitionStyle -eq 'RAW') {
        Initialize-Disk -Number 2 -PartitionStyle GPT | Out-Null
        New-Partition -DiskNumber 2 -Size 64MB -DriveLetter T | Format-Volume -FileSystem NTFS -NewFileSystemLabel A06DATA -Confirm:$false | Out-Null
    }
    if ((Get-Partition -DriveLetter T).DiskNumber -ne 2) { throw 'T: fixture missing' }
    $dataPart=(Get-Partition -DriveLetter T).PartitionNumber
    [IO.File]::WriteAllText('C:\Lab\a06\source-partition.txt',[string]$dataPart)
    $data=[byte[]]::new(8MB);([Random]::new(20261003)).NextBytes($data)
    [IO.File]::WriteAllBytes('T:\a06-control.bin',$data)
    $null=& icacls.exe 'T:\a06-control.bin' /grant '*S-1-5-4:M'
    $control=(Get-FileHash 'T:\a06-control.bin' -Algorithm SHA256).Hash
    [IO.File]::WriteAllText('C:\Lab\a06\control.sha256',$control)
    foreach ($name in @('volume.qcow2','disk.qcow2')) { Remove-Item "C:\Lab\a06\$name" -Force -ErrorAction SilentlyContinue }
    $null=Native 'C:\Lab\own-backup-cli.exe backup T: --output C:\Lab\a06\volume.qcow2 --format qcow2 --no-vss'
    $null=Native "C:\Lab\own-backup-cli.exe backup-disk --disk 2 --partitions $dataPart --output C:\Lab\a06\disk.qcow2 --format qcow2"
    $null=Native 'C:\Lab\own-backup-cli.exe verify C:\Lab\a06\volume.qcow2'
    $null=Native 'C:\Lab\own-backup-cli.exe verify C:\Lab\a06\disk.qcow2'
    $null=Native 'C:\Lab\audit_a06_guard.exe live C:\Lab\a06\live-guard.json'
    $live=Get-Content 'C:\Lab\a06\live-guard.json' -Raw | ConvertFrom-Json
    Say "A06-LIVE-GUARD $((Get-Content 'C:\Lab\a06\live-guard.json' -Raw) -replace '\s+',' ')"
    if ($live.system_disk -ne 0 -or $live.esp_disk -ne 1) { throw 'split boot fixture not confirmed' }
    foreach ($mode in @('media','environment')) {
        $oldAvailable=Test-Path 'C:\Lab\audit_a06_before.exe'
        if ($oldAvailable) { $null=Native "C:\Lab\audit_a06_before.exe $mode C:\Lab\a06\before-$mode.json" }
        $null=Native "C:\Lab\audit_a06_media_override.exe $mode C:\Lab\a06\after-$mode.json"
        if ($oldAvailable) { $before=Get-Content "C:\Lab\a06\before-$mode.json" -Raw | ConvertFrom-Json }
        $after=Get-Content "C:\Lab\a06\after-$mode.json" -Raw | ConvertFrom-Json
        if ($oldAvailable -and ($before.real_windows_pe -or $before.disk_refused -or $before.volume_refused)) { throw "old bypass not reproduced: $mode" }
        if ($after.guard_media -or -not $after.disk_refused -or -not $after.volume_refused -or $after.system_disk -ne 0) { throw "override still disarms guard: $mode" }
        Say "A06-OVERRIDE-FIX mode=$mode old_probe_present=$oldAvailable after_refused=true writes=false"
    }
    $before=Snapshot
    $cases=@(
        @('volume-C','restore C:\Lab\a06\volume.qcow2 --target C: --yes'),
        @('volume-ESP','restore C:\Lab\a06\volume.qcow2 --target S: --yes'),
        @('disk-0','restore-disk C:\Lab\a06\disk.qcow2 --target-disk 0 --yes'),
        @('disk-0-force','restore-disk C:\Lab\a06\disk.qcow2 --target-disk 0 --yes --force'),
        @('disk-1','restore-disk C:\Lab\a06\disk.qcow2 --target-disk 1 --yes'),
        @('disk-1-force','restore-disk C:\Lab\a06\disk.qcow2 --target-disk 1 --yes --force'))
    $reports=New-Object Collections.ArrayList
    foreach ($case in $cases) {
        $output=cmd.exe /d /c "C:\Lab\own-backup-cli.exe $($case[1]) 2>&1" | Out-String
        $code=$LASTEXITCODE
        if ($code -ne 1 -or $output -notmatch 'Restore incompatible:' -or $output -match 'Restore complete:|Disk restore complete:') { throw "not a guard refusal: $($case[0]) exit=$code $output" }
        $null=$reports.Add(@{name=$case[0];exit=$code;output=$output})
        Say "A06-CLI-REFUSED name=$($case[0]) code=$code"
    }
    $after=Snapshot
    foreach ($key in $before.Keys) { if ($before[$key] -ne $after[$key]) { throw "CLI changed protected bytes: $key" } }
    [IO.File]::WriteAllText('C:\Lab\a06\cli-results.json',(@{pass=$true;cases=$reports;before=$before;after=$after}|ConvertTo-Json -Depth 6),([Text.UTF8Encoding]::new($false)))
    [IO.File]::WriteAllText('C:\Lab\a06\baseline.json',($before|ConvertTo-Json),([Text.UTF8Encoding]::new($false)))
    $positive=New-Object Collections.ArrayList
    foreach ($kind in @('volume','disk')) {
        $changed=[IO.File]::ReadAllBytes('T:\a06-control.bin');$changed[0]=$changed[0] -bxor 0x55;[IO.File]::WriteAllBytes('T:\a06-control.bin',$changed)
        if ((Get-FileHash 'T:\a06-control.bin' -Algorithm SHA256).Hash -eq $control) { throw 'positive target mutation failed' }
        $text=if ($kind -eq 'volume') {'restore C:\Lab\a06\volume.qcow2 --target T: --yes'} else {'restore-disk C:\Lab\a06\disk.qcow2 --target-disk 2 --yes --force'}
        $output=Native "C:\Lab\own-backup-cli.exe $text"
        RemountSpare
        if ((Get-FileHash 'T:\a06-control.bin' -Algorithm SHA256).Hash -ne $control) { throw "CLI positive restore differs: $kind" }
        $null=$positive.Add(@{kind=$kind;pass=$true;sha256=$control;output=$output})
        Say "A06-CLI-POSITIVE-PASS kind=$kind hash=$control"
    }
    [IO.File]::WriteAllText('C:\Lab\a06\cli-positive.json',($positive|ConvertTo-Json -Depth 4),([Text.UTF8Encoding]::new($false)))
    $admins=([Security.Principal.SecurityIdentifier]'S-1-5-32-544').Translate([Security.Principal.NTAccount]).Value.Split('\')[-1]
    if (@(Get-LocalGroupMember $admins | Where-Object {$_.Name -like '*\lab'}).Count) { Remove-LocalGroupMember -Group $admins -Member "$env:COMPUTERNAME\lab" }
    $null=& icacls.exe 'C:\Lab\a06' /grant '*S-1-5-4:(OI)(CI)M'
    if ($LASTEXITCODE -ne 0) { throw 'shared result access failed' }
    $startup=[Environment]::GetFolderPath('CommonStartup')
    foreach ($name in @('OwnBackupAudit29.cmd','OwnBackupA13Single.cmd','OwnBackupA02GuiProbe.cmd','OwnBackupA05.cmd')) { Remove-Item (Join-Path $startup $name) -Force -ErrorAction SilentlyContinue }
    [IO.File]::WriteAllText((Join-Path $startup 'OwnBackupA06.cmd'),"@echo off`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-a06-user.ps1`r`n")
    $answer=New-Object Xml.XmlDocument;$answer.Load('C:\Lab\autounattend.xml')
    $password=$answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    $winlogon='HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $winlogon DefaultUserName 'lab';Set-ItemProperty $winlogon DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $winlogon DefaultPassword $password;Set-ItemProperty $winlogon AutoAdminLogon '1';Set-ItemProperty $winlogon AutoLogonCount 2
    [IO.File]::WriteAllText('C:\Lab\a06\phase.txt','rpc')
    [IO.File]::WriteAllText('C:\Lab\a06\stage.ready','ready')
    Say 'A06-CLI-PASS protected_hashes_unchanged=true REBOOT'
    shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "A06-STAGE-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
