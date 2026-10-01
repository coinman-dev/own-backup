# SYSTEM controls only disposable VM SMB server and the local DATA partition.
function Say([string]$Text) {
    try { $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'; $p.Open(); $p.WriteLine($Text); $p.Close() } catch { }
}
$ErrorActionPreference = 'Stop'
$listener = $null
try {
    $part = Get-Content 'C:\Lab\a12\parts.json' -Raw | ConvertFrom-Json
    Stop-Service LanmanServer -Force -ErrorAction Stop
    (Get-Service LanmanServer).WaitForStatus('Stopped', [TimeSpan]::FromSeconds(45))
    Say 'A12-SMB-OFFLINE service=Stopped'
    $threads = Start-Process powershell.exe -ArgumentList '-NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-a12-threads.ps1' -WindowStyle Hidden -PassThru
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 43995)
    $listener.Start()
    [IO.File]::WriteAllText('C:\Lab\a12\watch.ready', 'ready')
    for ($commandIndex=0; $commandIndex -lt 4; $commandIndex++) {
        $tcp = $listener.AcceptTcpClient()
        try {
            $stream = $tcp.GetStream()
            $reader = [IO.StreamReader]::new($stream, [Text.Encoding]::ASCII)
            $command = $reader.ReadLine()
            switch ($command) {
                'local_on' {
                    $offline = @(Import-Csv 'C:\Lab\a12\threads.csv')
                    if ($offline.Count -lt 18) { throw 'less than three minutes of thread samples' }
                    $start = [DateTimeOffset]::Parse($offline[0].utc)
                    $end = [DateTimeOffset]::Parse($offline[-1].utc)
                    if (($end - $start).TotalSeconds -lt 180) { throw 'offline observation too short' }
                    $counts = @($offline | ForEach-Object { [int]$_.threads })
                    $min = ($counts | Measure-Object -Minimum).Minimum
                    $max = ($counts | Measure-Object -Maximum).Maximum
                    Say "A12-THREAD-BOUNDS samples=$($offline.Count) min=$min max=$max"
                    if ($max -gt $min + 8) { throw 'service thread count grew during offline interval' }
                    $log = 'C:\Lab\Logs\own-backup-service-' + [DateTime]::UtcNow.ToString('yyyy-MM-dd') + '.log'
                    $metrics = @(Get-Content -LiteralPath $log | Where-Object { $_ -match 'destination probe slots' })
                    if ($metrics.Count -lt 18) { throw 'scheduler probe metrics missing' }
                    $probeCounts = @($metrics | ForEach-Object { if ($_ -match 'probes=(\d+)') { [int]$Matches[1] } })
                    $maxSlots = ($probeCounts | Measure-Object -Maximum).Maximum
                    Say "A12-PROBE-BOUNDS samples=$($metrics.Count) max_slots=$maxSlots"
                    if ($maxSlots -gt 3) { throw 'probe slots exceeded the number of jobs' }
                    Add-PartitionAccessPath -DiskNumber 1 -PartitionNumber ([int]$part.destination_part) -AccessPath 'R:' -ErrorAction Stop
                    Say 'A12-LOCAL-ON letter=R'
                }
                'share_on' {
                    Start-Service LanmanServer -ErrorAction Stop
                    (Get-Service LanmanServer).WaitForStatus('Running', [TimeSpan]::FromSeconds(45))
                    $ok = $false
                    for ($tick=0; $tick -lt 60; $tick++) {
                        if (Test-Path -LiteralPath $part.unc) { $ok=$true; break }
                        Start-Sleep -Seconds 1
                    }
                    if (-not $ok) { throw 'SYSTEM cannot read SMB share after restart' }
                    Say 'A12-SMB-ON service=Running readable=True'
                }
                'share_off' {
                    Stop-Service LanmanServer -Force -ErrorAction Stop
                    (Get-Service LanmanServer).WaitForStatus('Stopped', [TimeSpan]::FromSeconds(45))
                    Say 'A12-SMB-OFFLINE service=Stopped'
                }
                default { throw "unexpected command $command" }
            }
            $bytes = [Text.Encoding]::ASCII.GetBytes("OK`n")
            $stream.Write($bytes, 0, $bytes.Length)
        } finally { $tcp.Dispose() }
    }
    [IO.File]::WriteAllText('C:\Lab\a12\threads.stop', 'stop')
    [IO.File]::WriteAllText('C:\Lab\a12\watch.pass', 'pass')
    Say 'A12-WATCH-PASS'
} catch { Say "A12-WATCH-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)" }
finally { if ($listener) { $listener.Stop() } }
