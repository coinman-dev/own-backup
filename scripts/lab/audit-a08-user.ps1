function Say([string]$Text) {
    try {
        $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $p.Open(); $p.WriteLine($Text); $p.Close()
    } catch { }
}
$ErrorActionPreference = 'Stop'
try {
    Say "A08-USER-BEGIN at=$([DateTime]::UtcNow.ToString('o'))"
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & 'C:\Lab\audit_a08_restore_cancel.exe' 'C:\Lab\a08\config.json' 2>&1 |
            ForEach-Object { Say ([string]$_) }
        $code = $LASTEXITCODE
    } finally { $ErrorActionPreference = $old }
    if ($code -ne 0) { throw "restore client exit=$code" }
    [IO.File]::WriteAllText('C:\Lab\a08\client.done', 'done')
    $deadline = [DateTime]::UtcNow.AddMinutes(2)
    while (-not (Test-Path -LiteralPath 'C:\Lab\a08\restart.ready')) {
        if ([DateTime]::UtcNow -gt $deadline) { throw 'restart watcher timeout' }
        Start-Sleep -Seconds 1
    }
    & 'C:\Lab\audit_a08_restore_cancel.exe' 'C:\Lab\a08\config.json' '--check-history' 2>&1 |
        ForEach-Object { Say ([string]$_) }
    if ($LASTEXITCODE -ne 0) { throw 'reopened history check failed' }
    $result = [IO.File]::ReadAllBytes('C:\Lab\a08\results.json')
    Say "A08-RESULTS-BASE64 $([Convert]::ToBase64String($result))"
    $sessions = (& iscsicli.exe SessionList 2>&1 | Out-String).ToLowerInvariant()
    Say "A08-ISCSI orphan=$($sessions.Contains('ownbackup'))"
    if ($sessions.Contains('ownbackup')) { throw 'orphan iSCSI session remains' }
    Say 'A08-USER-PASS'
} catch {
    Say "A08-USER-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    $log = 'C:\Lab\Logs\own-backup-service-' + [DateTime]::UtcNow.ToString('yyyy-MM-dd') + '.log'
    foreach ($line in @(Get-Content -LiteralPath $log -ErrorAction SilentlyContinue)) {
        if ($line -match 'restore|verification|verify|Restore') { Say "A08-SERVICE-LOG $line" }
    }
    Say 'A08-USER-DONE'
    shutdown.exe /s /t 5 | Out-Null
}
