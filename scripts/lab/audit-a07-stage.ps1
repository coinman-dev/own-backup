# SYSTEM, disposable overlay and a newly created 2 GiB source disk only.
function Say([string]$Text) {
    try { $p = New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One'; $p.Open(); $p.WriteLine($Text); $p.Close() } catch { }
}
$ErrorActionPreference = 'Stop'
try {
    if (Test-Path 'C:\Lab\a07-stage.ready') {
        Start-Process powershell.exe -ArgumentList '-NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-a07-watch.ps1' -WindowStyle Hidden | Out-Null
        Say 'A07-STAGE-SECOND-BOOT'; return
    }
    Say "A07-STAGE-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    foreach ($name in @('user','watch')) {
        $errors = $null
        $null = [Management.Automation.Language.Parser]::ParseFile("C:\Lab\audit-a07-$name.ps1", [ref]$null, [ref]$errors)
        if ($errors) { throw "script syntax: $name $errors" }
    }
    New-Item -ItemType Directory -Force 'C:\Lab\a07','C:\Lab\a07-gate' | Out-Null
    $disk = Get-Disk -Number 1
    if ($disk.IsBoot -or $disk.IsSystem -or $disk.Size -ne 2GB -or $disk.PartitionStyle -ne 'RAW') { throw 'unexpected disposable disk layout' }
    Initialize-Disk -Number 1 -PartitionStyle GPT | Out-Null
    $part = New-Partition -DiskNumber 1 -Size 1536MB -DriveLetter E
    Format-Volume -Partition $part -FileSystem NTFS -NewFileSystemLabel A07Source -Confirm:$false | Out-Null
    for ($d=0; $d -lt 12; $d++) {
        $folder = "E:\records$d"; New-Item -ItemType Directory $folder | Out-Null
        for ($f=0; $f -lt 1000; $f++) { [IO.File]::WriteAllText("$folder\record$f.txt", "A07-$d-$f") }
    }
    $random = New-Object Random 707
    $buffer = New-Object byte[] (1MB)
    $stream = [IO.File]::OpenWrite('E:\control.bin')
    try { for ($n=0; $n -lt 128; $n++) { $random.NextBytes($buffer); $stream.Write($buffer,0,$buffer.Length) } } finally { $stream.Dispose() }
    $part = Get-Partition -DriveLetter E
    $fixture = @{ source='E:'; disk=1; partition=$part.PartitionNumber; offset=$part.Offset; size=$part.Size; records=12000; control_bytes=128MB; control_sha256=(Get-FileHash 'E:\control.bin' -Algorithm SHA256).Hash }
    [IO.File]::WriteAllText('C:\Lab\a07\fixture.json', ($fixture | ConvertTo-Json))
    Say "A07-FIXTURE $($fixture | ConvertTo-Json -Compress)"
    Set-Disk -Number 1 -IsOffline $true
    $output = cmd.exe /d /c 'C:\Lab\audit_a07_cancel.exe scanner 2>&1' | Out-String
    $code=$LASTEXITCODE; [IO.File]::WriteAllText('C:\Lab\a07\scanner.log',$output)
    foreach ($line in ($output -split "`r?`n")) { if ($line) { Say $line } }
    if ($code -ne 0) { throw 'native scanner cancellation failed' }
    Set-Disk -Number 1 -IsOffline $false
    if (-not (Test-Path 'E:\control.bin')) { throw 'source did not return online' }
    Stop-Service OwnBackup -ErrorAction SilentlyContinue
    # Remove synthetic jobs inherited from the lab base, only in this clone.
    foreach ($name in @('jobs.json','activity.json')) { [IO.File]::WriteAllText("C:\ProgramData\OwnBackup\$name",'[]') }
    $output = cmd.exe /d /c 'C:\Lab\own-backup-service-a07.exe /update 2>&1' | Out-String
    if ($LASTEXITCODE -ne 0) { throw "service update: $output" }
    $wanted=(Get-FileHash 'C:\Lab\own-backup-service-a07.exe' -Algorithm SHA256).Hash
    if ((Get-FileHash 'C:\Lab\own-backup-service.exe' -Algorithm SHA256).Hash -ne $wanted) { throw 'SCM binary hash mismatch' }
    Say "A07-SERVICE sha256=$wanted state=$((Get-Service OwnBackup).Status)"
    foreach ($folder in @('C:\Lab\a07','C:\Lab\a07-gate')) {
        $null = & icacls.exe $folder /grant '*S-1-5-4:(OI)(CI)M'
        if ($LASTEXITCODE -ne 0) { throw 'fixture ACL failed' }
    }
    $admins=([Security.Principal.SecurityIdentifier]'S-1-5-32-544').Translate([Security.Principal.NTAccount]).Value.Split('\')[-1]
    if (@(Get-LocalGroupMember $admins | Where-Object { $_.Name -like '*\lab' }).Count) { Remove-LocalGroupMember $admins "$env:COMPUTERNAME\lab" }
    $startup=[Environment]::GetFolderPath('CommonStartup')
    Get-ChildItem $startup -Filter 'OwnBackup*.cmd' | Remove-Item -Force
    [IO.File]::WriteAllText((Join-Path $startup 'OwnBackupA07.cmd'), "@echo off`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-a07-user.ps1`r`n")
    $answer=New-Object Xml.XmlDocument; $answer.Load('C:\Lab\autounattend.xml')
    $labPassword=$answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    $winlogon='HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $winlogon DefaultUserName 'lab'; Set-ItemProperty $winlogon DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $winlogon DefaultPassword $labPassword; Set-ItemProperty $winlogon AutoAdminLogon '1'; Set-ItemProperty $winlogon AutoLogonCount 1
    [IO.File]::WriteAllText('C:\Lab\a07-stage.ready','ready')
    Say 'A07-STAGE-READY REBOOT'; shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "A07-STAGE-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
