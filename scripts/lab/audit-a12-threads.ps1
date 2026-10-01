# SYSTEM: sample only the Own Backup service process during the offline interval.
function Say([string]$Text) {
    try { $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'; $p.Open(); $p.WriteLine($Text); $p.Close() } catch { }
}
$ErrorActionPreference = 'Stop'
try {
    $id = [int](Get-CimInstance Win32_Service -Filter "Name='OwnBackup'").ProcessId
    if (-not $id) { throw 'service process missing' }
    $file = 'C:\Lab\a12\threads.csv'
    [IO.File]::WriteAllText($file, "utc,pid,threads,handles,private_bytes`r`n")
    for ($tick=0; $tick -lt 50; $tick++) {
        $process = Get-Process -Id $id -ErrorAction Stop
        $line = '{0},{1},{2},{3},{4}' -f [DateTime]::UtcNow.ToString('o'), $id, $process.Threads.Count, $process.HandleCount, $process.PrivateMemorySize64
        [IO.File]::AppendAllText($file, "$line`r`n")
        if ($tick % 6 -eq 0) { Say "A12-THREADS $line" }
        if (Test-Path -LiteralPath 'C:\Lab\a12\threads.stop') { break }
        Start-Sleep -Seconds 10
    }
} catch { Say "A12-THREADS-FAIL message=$($_.Exception.Message)" }
