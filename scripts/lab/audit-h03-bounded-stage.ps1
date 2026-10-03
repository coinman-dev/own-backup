# SYSTEM in a fresh overlay; only the new 1 GiB disk and C:\Lab fixtures are written.
function Say([string]$Text) {
    try { $p=New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One'; $p.Open(); $p.WriteLine($Text); $p.Close() } catch { }
}
function SaveJson([string]$Path,$Value) { [IO.File]::WriteAllText($Path,($Value | ConvertTo-Json -Depth 12)) }
function TargetVolume {
    $disk=Get-Disk -Number 1
    if ($disk.IsBoot -or $disk.IsSystem -or $disk.Size -ne 1GB) { throw 'unexpected target disk' }
    SaveJson 'C:\Lab\h03-results\target-at-mount.json' @{disk=$disk | Select-Object Number,Guid,IsOffline,IsReadOnly,PartitionStyle,OperationalStatus}
    if ($disk.IsOffline) { Set-Disk -Number 1 -IsOffline $false }
    if ($disk.IsReadOnly) { Set-Disk -Number 1 -IsReadOnly $false }
    Update-HostStorageCache
    $parts=@(Get-Partition -DiskNumber 1 | Where-Object { $_.Type -ne 'Reserved' -and $_.Size -gt 100MB })
    if ($parts.Count -ne 1) { throw 'one restored data partition required' }
    if ($parts[0].DriveLetter -ne 'R') { Set-Partition -DiskNumber 1 -PartitionNumber $parts[0].PartitionNumber -NewDriveLetter R }
    $observed=@()
    for ($tick=0; $tick -lt 120; $tick++) {
        $volume=Get-Volume -Partition (Get-Partition -DiskNumber 1 -PartitionNumber $parts[0].PartitionNumber) -ErrorAction SilentlyContinue
        $observed+=@{utc=[DateTime]::UtcNow.ToString('o'); filesystem=$volume.FileSystem; label=$volume.FileSystemLabel; letter=$volume.DriveLetter}
        if ($volume.FileSystem -eq 'NTFS' -and $volume.FileSystemLabel -eq 'OBKSRC') {
            SaveJson 'C:\Lab\h03-results\mount-observations.json' $observed
            return
        }
        Start-Sleep -Milliseconds 500
    }
    SaveJson 'C:\Lab\h03-results\mount-observations.json' $observed
    throw 'restored NTFS did not become ready within 60 seconds'
}
function FilesCheck {
    $rows=@()
    foreach ($record in (Get-Content 'C:\Lab\expected-h03-files.json' -Raw | ConvertFrom-Json)) {
        $file=Join-Path 'R:\' $record.name
        $hash=(Get-FileHash $file -Algorithm SHA256).Hash
        if ($hash -ne $record.sha256 -or (Get-Item $file).Length -ne $record.bytes) { throw "restored file differs: $($record.name)" }
        $rows+=@{name=$record.name; bytes=$record.bytes; sha256=$hash}
    }
    if (Test-Path 'R:\excluded-secret.bin') { throw 'excluded file reappeared' }
    return $rows
}
function CheckNtfs([string]$Name) {
    $output=cmd.exe /d /c 'chkdsk.exe R: /scan 2>&1' | Out-String
    $code=$LASTEXITCODE
    [IO.File]::WriteAllText("C:\Lab\h03-results\chkdsk-$Name.log",$output)
    Say "H03-CHKDSK phase=$Name exit=$code"
    if ($code -ne 0) { throw "NTFS scan failed: $Name exit=$code" }
    return @{phase=$Name; exit=$code}
}
$ErrorActionPreference='Stop'
$root='C:\Lab\h03-results'
try {
    New-Item -ItemType Directory -Force $root | Out-Null
    Stop-Service OwnBackup -ErrorAction SilentlyContinue
    $startup=[Environment]::GetFolderPath('CommonStartup')
    Get-ChildItem $startup -Filter 'OwnBackup*.cmd' | Remove-Item -Force
    if (Test-Path "$root\reboot.ready") {
        Say 'H03-BOUNDED-SECOND-BOOT'
        TargetVolume
        $files=FilesCheck
        $expected=Get-Content "$root\write-before-reboot.json" -Raw | ConvertFrom-Json
        $written='R:\h03-persisted-write.bin'
        if ((Get-FileHash $written -Algorithm SHA256).Hash -ne $expected.sha256 -or (Get-Item $written).Length -ne $expected.bytes) { throw 'written file did not survive reboot' }
        $scan=CheckNtfs 'after-reboot'
        SaveJson "$root\filesystem-result.json" @{pass=$true; restored_files=$files; excluded_absent=$true; write=$expected; write_survived_reboot=$true; scan_after_reboot=$scan}
        Say 'H03-BOUNDED-PASS ntfs_scan_before_after_reboot=true files_matched=4 write_persisted=true'
        Say 'H03-BOUNDED-DONE'; shutdown.exe /s /t 5 | Out-Null; return
    }
    Say "H03-BOUNDED-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    $disk=Get-Disk -Number 1
    if ($disk.IsBoot -or $disk.IsSystem -or $disk.Size -ne 1GB -or $disk.PartitionStyle -ne 'RAW') { throw 'new blank 1 GiB target required' }
    $media=Get-Volume | Where-Object { $_.DriveLetter -and (Test-Path "$($_.DriveLetter):\h03-media.marker") } | Select-Object -First 1
    if (-not $media) { throw 'saved H03 archive CD missing' }
    $image="$($media.DriveLetter):\lab_disk0_full_1.obk"
    Say "H03-ARCHIVE sha256=$((Get-FileHash $image -Algorithm SHA256).Hash)"
    Set-Disk -Number 1 -IsOffline $true
    $output=cmd.exe /d /c "C:\Lab\obk_live_probe.exe h03-restore $image 1 2>&1" | Out-String
    $code=$LASTEXITCODE; [IO.File]::WriteAllText("$root\restore.log",$output)
    foreach ($line in ($output -split "`r?`n")) { if ($line) { Say $line } }
    if ($code -ne 0) { throw "restore failed exit=$code" }
    Set-Disk -Number 1 -IsOffline $false
    Update-HostStorageCache
    TargetVolume
    $files=FilesCheck
    $scan=CheckNtfs 'after-restore'
    SaveJson "$root\filesystem-before-reboot.json" @{restored_files=$files; scan=$scan; excluded_absent=$true}
    $scales=@(65536,262144,1048576)
    if ((Test-Path 'C:\Lab\h03-bounded-mode.txt') -and (Get-Content 'C:\Lab\h03-bounded-mode.txt' -Raw).Trim() -eq 'filesystem-only') { $scales=@() }
    foreach ($count in $scales) {
        Say "H03-INDEX-BEGIN entries=$count"
        $folder="C:\Lab\h03-index-$count"
        $info=New-Object Diagnostics.ProcessStartInfo
        $info.FileName='C:\Lab\audit_h03_index_growth.exe'; $info.Arguments="$folder $count"
        $info.UseShellExecute=$false; $info.CreateNoWindow=$true
        $info.RedirectStandardOutput=$true; $info.RedirectStandardError=$true
        $process=New-Object Diagnostics.Process; $process.StartInfo=$info
        if (-not $process.Start()) { throw 'native index process did not start' }
        $stdout=$process.StandardOutput.ReadToEndAsync(); $stderr=$process.StandardError.ReadToEndAsync()
        $samples=@(); $deadline=[DateTime]::UtcNow.AddMinutes(5)
        while (-not $process.HasExited) {
            $process.Refresh()
            $samples+=@{utc=[DateTime]::UtcNow.ToString('o'); working_set=$process.WorkingSet64; private_bytes=$process.PrivateMemorySize64; peak_working_set=$process.PeakWorkingSet64; peak_pagefile=$process.PeakPagedMemorySize64}
            if ([DateTime]::UtcNow -gt $deadline) { throw "index fixture timeout: $count" }
            Start-Sleep -Milliseconds 100
        }
        $process.WaitForExit(); $process.Refresh()
        [IO.File]::WriteAllText("$root\index-$count.log",$stdout.GetAwaiter().GetResult())
        [IO.File]::WriteAllText("$root\index-$count.stderr",$stderr.GetAwaiter().GetResult())
        SaveJson "$root\memory-$count.json" @{pid=$process.Id; entry_count=$count; exit=$process.ExitCode; samples=$samples}
        if ($process.ExitCode -ne 0) { throw "index fixture failed: $count exit=$($process.ExitCode)" }
        Copy-Item "$folder\result.json" "$root\index-$count.json"
        $report=Get-Content "$root\index-$count.json" -Raw | ConvertFrom-Json
        if (-not $report.pass -or $report.index_entries -ne $count) { throw 'index count not verified' }
        Say "H03-INDEX-PASS entries=$count initial_bytes=$($report.initial_image_bytes) final_bytes=$($report.final_image_bytes) recovered_generation=$($report.recovered_generation)"
        # Fixtures are test-only; keep the measurements and remove them before the next scale.
        Remove-Item $folder -Recurse -Force
    }
    $bytes=New-Object byte[] (1MB); (New-Object Random 30303).NextBytes($bytes)
    $stream=[IO.File]::Open('R:\h03-persisted-write.bin',[IO.FileMode]::CreateNew,[IO.FileAccess]::Write)
    try { $stream.Write($bytes,0,$bytes.Length); $stream.Flush($true) } finally { $stream.Dispose() }
    SaveJson "$root\write-before-reboot.json" @{bytes=$bytes.Length; sha256=(Get-FileHash 'R:\h03-persisted-write.bin' -Algorithm SHA256).Hash}
    [IO.File]::WriteAllText("$root\reboot.ready",'ready')
    Say 'H03-BOUNDED-REBOOT'; shutdown.exe /r /t 5 | Out-Null
} catch {
    $failure="H03-BOUNDED-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    Say $failure; [IO.File]::WriteAllText("$root\failure.txt",$failure)
    shutdown.exe /s /t 5 | Out-Null
}
