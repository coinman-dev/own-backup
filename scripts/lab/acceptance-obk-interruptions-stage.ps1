# SYSTEM task inside a disposable VM; no owner drives are attached.
$ErrorActionPreference='Stop'
function Say([string]$text) {
    $port=New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One'
    try {$port.Open();$port.WriteLine($text)} finally {$port.Dispose()}
}
try {
    $root='C:\Lab\obk-interruptions-2026-10-08'
    if (Test-Path $root) {throw 'previous interruption fixture must be preserved first'}
    $service=Get-Service OwnBackup -ErrorAction SilentlyContinue
    if ($service) {Stop-Service OwnBackup -Force;Set-Service OwnBackup -StartupType Disabled}
    $disks=@(Get-Disk | Where-Object {$_.Size -eq 96MB -and $_.PartitionStyle -eq 'RAW' -and -not $_.IsSystem -and -not $_.IsBoot})
    if ($disks.Count -ne 1 -or (Test-Path 'U:\')) {throw 'dedicated empty 96 MiB disk is not unique'}
    $disk=$disks[0]
    Initialize-Disk -Number $disk.Number -PartitionStyle MBR | Out-Null
    $partition=New-Partition -DiskNumber $disk.Number -UseMaximumSize -DriveLetter U
    Format-Volume -Partition $partition -FileSystem NTFS -NewFileSystemLabel OBKFAIL -Confirm:$false | Out-Null
    $volume=Get-Volume -DriveLetter U
    if ($volume.FileSystem -ne 'NTFS' -or $volume.FileSystemLabel -ne 'OBKFAIL' -or (Get-Partition -DriveLetter U).DiskNumber -ne $disk.Number) {throw 'exhaustion target differs'}
    [IO.File]::WriteAllText('U:\obk-exhaustion.marker','Disposable 96 MiB NTFS exhaustion fixture.')
    $exe='C:\Lab\audit_obk_interruptions.exe'
    Say "OBK-FAULT-START target_disk=$($disk.Number) bytes=$($disk.Size) sha256=$((Get-FileHash $exe -Algorithm SHA256).Hash)"
    $process=Start-Process -FilePath $exe -ArgumentList @('run',$root,'U:\') -RedirectStandardOutput 'C:\Lab\obk-fault.out' -RedirectStandardError 'C:\Lab\obk-fault.err' -NoNewWindow -PassThru
    $null=$process.Handle
    if (-not $process.WaitForExit(600000)) {$process.Kill();throw 'native interruption acceptance timed out'}
    $process.Refresh()
    if ($process.ExitCode -ne 0) {throw "native exit=$($process.ExitCode) stdout=$([IO.File]::ReadAllText('C:\Lab\obk-fault.out')) stderr=$([IO.File]::ReadAllText('C:\Lab\obk-fault.err'))"}
    $result=Join-Path $root 'result.json'
    $summary=Get-Content $result -Raw -Encoding UTF8 | ConvertFrom-Json
    if (-not $summary.pass -or @($summary.cases).Count -ne 6 -or @($summary.cases | Where-Object {-not $_.pass}).Count -ne 0) {throw 'native result is incomplete'}
    Say "OBK-FAULT-REPORT $([Convert]::ToBase64String([IO.File]::ReadAllBytes($result)))"
    Say 'OBK-FAULT-PASS cases=6'
} catch {
    Say "OBK-FAULT-FAIL line=$($_.InvocationInfo.ScriptLineNumber) reason=$($_.Exception.Message)"
} finally {shutdown.exe /s /t 5 | Out-Null}
