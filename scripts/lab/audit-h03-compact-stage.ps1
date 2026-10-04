function Say([string]$Text) {
    try { $p=New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One';$p.Open();$p.WriteLine($Text);$p.Close() } catch { }
}
function Probe([string[]]$Arguments,[string]$Name,[int]$Expected=0) {
    $info=New-Object Diagnostics.ProcessStartInfo
    $info.FileName='C:\Lab\audit_h03_compact.exe';$info.Arguments=$Arguments -join ' '
    $info.UseShellExecute=$false;$info.CreateNoWindow=$true;$info.RedirectStandardOutput=$true;$info.RedirectStandardError=$true
    $p=New-Object Diagnostics.Process;$p.StartInfo=$info
    if (-not $p.Start()) { throw 'probe did not start' }
    $out=$p.StandardOutput.ReadToEndAsync();$err=$p.StandardError.ReadToEndAsync()
    if (-not $p.WaitForExit(300000)) { throw "probe timeout: $Name" }
    $text=$out.GetAwaiter().GetResult()+$err.GetAwaiter().GetResult()
    [IO.File]::WriteAllText("C:\Lab\compact-results\$Name.log",$text)
    foreach ($line in ($text -split "`r?`n")) { if ($line) { Say $line } }
    if ($p.ExitCode -ne $Expected) { throw "probe $Name exit=$($p.ExitCode) expected=$Expected" }
}
function MountTarget {
    $d=Get-Disk -Number 1
    if ($d.IsBoot -or $d.IsSystem -or $d.Size -ne 1GB) { throw 'wrong owned target' }
    if ($d.IsOffline) { Set-Disk -Number 1 -IsOffline $false }
    Update-HostStorageCache
    $p=Get-Partition -DiskNumber 1 | Where-Object { $_.Size -gt 100MB } | Select-Object -First 1
    if (-not $p) { throw 'NTFS partition missing' }
    if ($p.DriveLetter -ne 'R') { Set-Partition -DiskNumber 1 -PartitionNumber $p.PartitionNumber -NewDriveLetter R }
    for ($n=0;$n -lt 120;$n++) {
        $v=Get-Volume -Partition (Get-Partition -DiskNumber 1 -PartitionNumber $p.PartitionNumber) -ErrorAction SilentlyContinue
        if ($v.FileSystem -eq 'NTFS' -and $v.FileSystemLabel -eq 'OBKSRC') { return }
        Start-Sleep -Milliseconds 500
    }
    throw 'NTFS not ready'
}
function FilesCheck {
    $rows=@()
    foreach ($e in (Get-Content 'C:\Lab\expected-h03-files.json' -Raw | ConvertFrom-Json)) {
        $p=Join-Path 'R:\' $e.name;$h=(Get-FileHash $p -Algorithm SHA256).Hash
        if ($h -ne $e.sha256 -or (Get-Item $p).Length -ne $e.bytes) { throw "file differs $($e.name)" }
        $rows+=@{name=$e.name;sha256=$h;bytes=$e.bytes}
    }
    if (Test-Path 'R:\excluded-secret.bin') { throw 'excluded file returned' }
    return $rows
}
function NtfsCheck([string]$Phase) {
    $out=cmd.exe /d /c 'chkdsk.exe R: /scan 2>&1' | Out-String;$code=$LASTEXITCODE
    [IO.File]::WriteAllText("C:\Lab\compact-results\chkdsk-$Phase.log",$out)
    if ($code -ne 0) { throw "chkdsk $Phase exit=$code" }
    Say "H03-COMPACT-CHKDSK phase=$Phase exit=0"
}
$ErrorActionPreference='Stop'
$root='C:\Lab\compact-results'
try {
    New-Item -ItemType Directory -Force $root | Out-Null
    Stop-Service OwnBackup -ErrorAction SilentlyContinue
    Get-ChildItem ([Environment]::GetFolderPath('CommonStartup')) -Filter 'OwnBackup*.cmd' | Remove-Item -Force
    if (Test-Path "$root\reboot.ready") {
        MountTarget;NtfsCheck 'after-reboot';$files=FilesCheck
        $write=Get-Content "$root\write.json" -Raw | ConvertFrom-Json
        if ((Get-FileHash 'R:\compact-write.bin' -Algorithm SHA256).Hash -ne $write.sha256) { throw 'new write lost after reboot' }
        [IO.File]::WriteAllText("$root\filesystem-result.json",(@{pass=$true;files=$files;write=$write;ntfs_scan_exit=0;persisted=$true} | ConvertTo-Json -Depth 8))
        Say 'H03-COMPACT-ALL-PASS ntfs_files=4 persisted_write=true'
        shutdown.exe /s /t 5 | Out-Null;return
    }
    Say 'H03-COMPACT-STAGE-BEGIN'
    $d=Get-Disk -Number 1
    if ($d.IsBoot -or $d.IsSystem -or $d.Size -ne 1GB -or $d.PartitionStyle -ne 'RAW') { throw 'fresh blank target required' }
    $filesystemOnly=(Test-Path 'C:\Lab\compact-mode.txt') -and (Get-Content 'C:\Lab\compact-mode.txt' -Raw).Trim() -eq 'filesystem-only'
    if (-not $filesystemOnly) {
    Probe @('prepare','C:\Lab\compact-main') 'prepare-main'
    $base='C:\Lab\compact-main\base.obk'
    $null=& icacls.exe $base /inheritance:r /grant:r '*S-1-5-18:F' '*S-1-5-32-544:F' '*S-1-5-4:R'
    if ($LASTEXITCODE -ne 0) { throw 'fixture protected DACL failed' }
    $aclBefore=(Get-Acl $base).Sddl
    Probe @('run','C:\Lab\compact-main') 'run-main'
    $aclAfter=(Get-Acl $base).Sddl
    [IO.File]::WriteAllText("$root\dacl.json",(@{before=$aclBefore;after=$aclAfter;protected=(Get-Acl $base).AreAccessRulesProtected;pass=$true} | ConvertTo-Json))
    if ($aclAfter -ne $aclBefore) { throw 'compaction changed the protected DACL' }
    Copy-Item 'C:\Lab\compact-main\result.json' "$root\synthetic-result.json"
    Remove-Item 'C:\Lab\compact-main' -Recurse -Force
    Probe @('prepare','C:\Lab\compact-crash') 'prepare-crash'
    $reports=@()
    foreach ($phase in @('copy','verify','publish','published')) {
        Probe @('crash','C:\Lab\compact-crash',$phase) "crash-$phase" 23
        $check=@('check','C:\Lab\compact-crash');if ($phase -eq 'published') { $check+=@('published') }
        Probe $check "recover-$phase"
        $reports+=@{phase=$phase;crash_exit=23;recovery_pass=$true}
        Get-ChildItem 'C:\Lab\compact-crash' -Filter '.own-backup-obk-*.tmp' -Force | Remove-Item -Force
    }
    [IO.File]::WriteAllText("$root\crashes.json",($reports | ConvertTo-Json))
    Remove-Item 'C:\Lab\compact-crash' -Recurse -Force
    }
    $media=Get-Volume | Where-Object { $_.DriveLetter -and (Test-Path "$($_.DriveLetter):\h03-media.marker") } | Select-Object -First 1
    if (-not $media) { throw 'archive CD missing' }
    Copy-Item "$($media.DriveLetter):\lab_disk0_full_1.obk" 'C:\Lab\compact-ntfs.obk'
    Set-ItemProperty -LiteralPath 'C:\Lab\compact-ntfs.obk' -Name IsReadOnly -Value $false
    Set-Disk -Number 1 -IsOffline $true
    Probe @('ntfs','C:\Lab\compact-ntfs.obk','1') 'ntfs-restore'
    MountTarget;NtfsCheck 'after-restore';$files=FilesCheck
    [IO.File]::WriteAllText("$root\files-before-reboot.json",($files | ConvertTo-Json))
    $bytes=New-Object byte[] (1MB);(New-Object Random 704).NextBytes($bytes)
    $s=[IO.File]::OpenWrite('R:\compact-write.bin');try{$s.Write($bytes,0,$bytes.Length);$s.Flush($true)}finally{$s.Dispose()}
    [IO.File]::WriteAllText("$root\write.json",(@{bytes=$bytes.Length;sha256=(Get-FileHash 'R:\compact-write.bin' -Algorithm SHA256).Hash} | ConvertTo-Json))
    [IO.File]::WriteAllText("$root\reboot.ready",'ready')
    Say 'H03-COMPACT-REBOOT';shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "H03-COMPACT-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
