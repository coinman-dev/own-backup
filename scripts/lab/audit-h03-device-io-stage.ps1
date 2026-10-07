function Say([string]$Text) {
    try {$p=New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One';$p.Open();$p.WriteLine($Text);$p.Close()} catch { }
}
$ErrorActionPreference='Stop'
try {
    $case=(Get-Content 'C:\Lab\io-case.txt' -Raw).Trim()
    $phase=(Get-Content 'C:\Lab\io-phase.txt' -Raw).Trim()
    if ($case -notin @('write-eio','flush-eio','inflight-cut')) {throw 'invalid case'}
    if ($phase -notin @('prepare','io','verify')) {throw 'invalid phase'}
    if ($phase -eq 'prepare') {
        $disk=Get-Disk -Number 1
        if ($disk.Size -ne 536870912 -or $disk.PartitionStyle -ne 'RAW' -or (Test-Path 'T:\')) {throw 'target is not the isolated blank 512 MiB fixture'}
        $null=Initialize-Disk -Number 1 -PartitionStyle MBR -PassThru
        $part=New-Partition -DiskNumber 1 -UseMaximumSize -DriveLetter T
        $null=$part | Format-Volume -FileSystem NTFS -NewFileSystemLabel H03IO -Confirm:$false
        New-Item -ItemType Directory 'C:\Lab\h03-device-io' | Out-Null
    }
    $volume=Get-Volume -DriveLetter T
    if ($volume.FileSystemLabel -ne 'H03IO' -or $volume.FileSystem -ne 'NTFS') {throw 'target marker filesystem differs'}
    $env:OWN_BACKUP_H03_IO_ROOT='T:\h03-device-io'
    $env:OWN_BACKUP_H03_IO_EVIDENCE='C:\Lab\h03-device-io'
    $env:OWN_BACKUP_H03_IO_CASE=$case
    $env:OWN_BACKUP_H03_IO_PHASE=$phase
    $exe='C:\Lab\obk_io_tests.exe'
    $test='seal::obk::io_faults::vm_device_io_errors_and_inflight_cut'
    Say "H03-IO-BEGIN case=$case phase=$phase sha256=$((Get-FileHash $exe -Algorithm SHA256).Hash)"
    $output=cmd.exe /d /c "`"$exe`" --ignored --exact $test --nocapture 2>&1" | Out-String
    [IO.File]::WriteAllText("C:\Lab\io-$phase.log",$output)
    if ($LASTEXITCODE -ne 0 -or $output -notmatch '1 passed') {throw "native test failed: $output"}
    Say "H03-IO-PHASE-PASS phase=$phase"
    shutdown.exe /s /t 5 | Out-Null
} catch {
    Say "H03-IO-FAIL case=$case phase=$phase line=$($_.InvocationInfo.ScriptLineNumber) $($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
