# SYSTEM watcher: request SCM stop after the active-console client observes a
# live VerifyImage progress event on a disposable Windows VM.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

$ErrorActionPreference = 'Stop'
$done = 'C:\Lab\audit-h02-verify.done'
$listener = $null
try {
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 43992)
    $listener.Start()
    [IO.File]::WriteAllText('C:\Lab\audit-h02-verify-watch.ready', 'ready')
    Say 'H02-VERIFY-WATCH-LISTEN port=43992'
    $tcp = $listener.AcceptTcpClient()
    try {
        $reader = [IO.StreamReader]::new($tcp.GetStream(), [Text.Encoding]::ASCII)
        if ($reader.ReadLine() -ne 'VERIFY') { throw 'unexpected stop signal' }
    } finally { $tcp.Dispose() }
    $pidBefore = (Get-Process -Name 'own-backup-service' -ErrorAction Stop |
        Select-Object -First 1).Id
    Say "H02-VERIFY-STOP-REQUEST pid=$pidBefore at=$([DateTime]::UtcNow.ToString('o'))"
    Stop-Service OwnBackup -ErrorAction Stop
    (Get-Service OwnBackup).WaitForStatus('Stopped', [TimeSpan]::FromMinutes(5))
    $still = Get-Process -Id $pidBefore -ErrorAction SilentlyContinue
    Say "H02-VERIFY-STOPPED state=$((Get-Service OwnBackup).Status) old_process_alive=$([bool]$still) at=$([DateTime]::UtcNow.ToString('o'))"
    if ($still) { throw 'service process survived Stopped' }
    Start-Service OwnBackup -ErrorAction Stop
    (Get-Service OwnBackup).WaitForStatus('Running', [TimeSpan]::FromSeconds(30))
    Say "H02-VERIFY-RESTART state=$((Get-Service OwnBackup).Status)"
    [IO.File]::WriteAllText($done, 'pass')
    Say 'H02-VERIFY-WATCH-PASS'
} catch {
    Say "H02-VERIFY-WATCH-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    [IO.File]::WriteAllText($done, 'fail')
} finally {
    if ($listener) { $listener.Stop() }
}
