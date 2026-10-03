# Convert only the fresh BIOS VM overlay using Microsoft's own MBR2GPT tool.
function Say([string]$Text) {
    try {
        $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $p.Open(); $p.WriteLine($Text); $p.Close()
    } catch { }
}
$ErrorActionPreference='Stop'
try {
    $disk=Get-Disk -Number 0
    if ($disk.Size -ne 40GB -or $disk.PartitionStyle -ne 'MBR' -or -not $disk.IsBoot) { throw 'not the expected fresh 40-GiB MBR system fixture' }
    New-Item -ItemType Directory -Force 'C:\Lab\a06' | Out-Null
    $output=cmd.exe /d /c 'mbr2gpt.exe /validate /disk:0 /allowFullOS /logs:C:\Lab\a06 2>&1' | Out-String
    $code=$LASTEXITCODE
    foreach ($line in ($output -split "`r?`n")) { if ($line) { Say "A06-CONVERT-VALIDATE $line" } }
    if ($code -ne 0) { throw "MBR2GPT validation exit=$code" }
    $output=cmd.exe /d /c 'mbr2gpt.exe /convert /disk:0 /allowFullOS /logs:C:\Lab\a06 2>&1' | Out-String
    $code=$LASTEXITCODE
    foreach ($line in ($output -split "`r?`n")) { if ($line) { Say "A06-CONVERT $line" } }
    if ($code -ne 0) { throw "MBR2GPT conversion exit=$code" }
    $disk=Get-Disk -Number 0
    if ($disk.PartitionStyle -ne 'GPT') { throw 'conversion did not publish GPT' }
    $parts=@(Get-Partition -DiskNumber 0 | Select-Object PartitionNumber,Offset,Size,GptType,Guid,DriveLetter)
    $report=@{ pass=$true; disk=0; size=$disk.Size; windows_guid=[string]$disk.Guid; partitions=$parts }
    $json=$report | ConvertTo-Json -Depth 5 -Compress
    [IO.File]::WriteAllText('C:\Lab\a06\convert.json',$json,([Text.UTF8Encoding]::new($false)))
    Say "A06-CONVERT-RESULT $json"
    Say 'A06-CONVERT-PASS'
} catch {
    Say "A06-CONVERT-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    Say 'A06-CONVERT-DONE'
    shutdown.exe /s /t 5 | Out-Null
}
