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
$watchPassed = $false
$log = 'C:\Lab\Logs\own-backup-service-' + [DateTime]::UtcNow.ToString('yyyy-MM-dd') + '.log'
$logLinesBefore = @(Get-Content -LiteralPath $log -ErrorAction SilentlyContinue).Count
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
    $action = 'stop'
    if (Test-Path -LiteralPath 'C:\Lab\audit-h02-verify.action') {
        $action = (Get-Content -LiteralPath 'C:\Lab\audit-h02-verify.action' -Raw).Trim()
    }
    if ($action -notin @('stop', 'update')) { throw 'invalid H02 action' }
    Say "H02-VERIFY-STOP-REQUEST pid=$pidBefore at=$([DateTime]::UtcNow.ToString('o'))"
    if ($action -eq 'update') {
        $output = cmd.exe /d /c 'C:\Lab\own-backup-service-h02.exe /update 2>&1' | Out-String
        if ($LASTEXITCODE -ne 0) { throw "service update failed: $output" }
        Say "H02-VERIFY-UPDATE exit=0 output=$($output.Trim() -replace "`r?`n", ' | ')"
    } else {
        Stop-Service OwnBackup -ErrorAction Stop
        (Get-Service OwnBackup).WaitForStatus('Stopped', [TimeSpan]::FromMinutes(5))
    }
    $stopLine = Get-Content -LiteralPath $log | Select-Object -Skip $logLinesBefore |
        Where-Object { $_ -match 'Service stopped$' } | Select-Object -Last 1
    if (-not $stopLine) { throw 'service did not log Stopped' }
    $stoppedAt = [DateTimeOffset]::Parse(($stopLine -split '\s+')[0]).ToUnixTimeMilliseconds()
    $still = Get-Process -Id $pidBefore -ErrorAction SilentlyContinue
    Say "H02-VERIFY-STOPPED old_process_alive=$([bool]$still) stopped_at_ms=$stoppedAt"
    if ($still) { throw 'service process survived Stopped' }
    if ($action -eq 'stop') { Start-Service OwnBackup -ErrorAction Stop }
    (Get-Service OwnBackup).WaitForStatus('Running', [TimeSpan]::FromSeconds(30))
    Say "H02-VERIFY-RESTART state=$((Get-Service OwnBackup).Status)"
    $want = (Get-FileHash -LiteralPath 'C:\Lab\own-backup-service-h02.exe' -Algorithm SHA256).Hash
    $have = (Get-FileHash -LiteralPath 'C:\Lab\own-backup-service.exe' -Algorithm SHA256).Hash
    if ($want -ne $have) { throw 'installed service differs after restart/update' }
    Say "H02-VERIFY-INSTALLED action=$action sha256=$have"
    [IO.File]::WriteAllText('C:\Lab\audit-h02-verify-watch.result.json',
        (@{ stopped_at_ms = $stoppedAt; old_pid = $pidBefore } | ConvertTo-Json -Compress))
    $watchPassed = $true
    Say 'H02-VERIFY-WATCH-PASS'
} catch {
    Say "H02-VERIFY-WATCH-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    if ($listener) { $listener.Stop() }
    foreach ($line in @(Get-Content -LiteralPath $log -ErrorAction SilentlyContinue |
        Select-Object -Skip $logLinesBefore)) {
        Say "H02-SERVICE-LOG $line"
    }
    [IO.File]::WriteAllText($done, $(if ($watchPassed) { 'pass' } else { 'fail' }))
}
