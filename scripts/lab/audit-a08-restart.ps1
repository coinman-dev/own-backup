function Say([string]$Text) {
    try {
        $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $p.Open(); $p.WriteLine($Text); $p.Close()
    } catch { }
}
$ErrorActionPreference = 'Stop'
try {
    while (-not (Test-Path -LiteralPath 'C:\Lab\a08\client.done')) { Start-Sleep -Seconds 1 }
    Stop-Service OwnBackup -ErrorAction Stop
    (Get-Service OwnBackup).WaitForStatus('Stopped', [TimeSpan]::FromMinutes(5))
    Start-Service OwnBackup -ErrorAction Stop
    (Get-Service OwnBackup).WaitForStatus('Running', [TimeSpan]::FromSeconds(30))
    Start-Sleep -Seconds 1
    [IO.File]::WriteAllText('C:\Lab\a08\restart.ready', 'ready')
    Say 'A08-RESTART-PASS state=Running'
} catch { Say "A08-RESTART-FAIL message=$($_.Exception.Message)" }
