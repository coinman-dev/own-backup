# Deterministic crash-state injection for the legacy password migration.
# Runs as SYSTEM on a disposable QEMU overlay; no real backup data is touched.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

function RunNative([string]$Command) {
    $output = (cmd.exe /d /c "$Command 2>&1" | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) { throw "exit=$LASTEXITCODE command=$Command output=$output" }
    return $output
}

function WaitService {
    for ($tick = 0; $tick -lt 30; $tick++) {
        if ((Get-Service -Name OwnBackup).Status -eq 'Running') { break }
        Start-Sleep -Seconds 2
    }
    if ((Get-Service -Name OwnBackup).Status -ne 'Running') { throw 'OwnBackup service did not start' }
    Start-Sleep -Seconds 2
}

function Check([string]$Label, [bool]$Scan) {
    Say (RunNative 'C:\Lab\audit_h01.exe check')
    if ($Scan) { Say (RunNative 'C:\Lab\audit_h01.exe scan') }
    Say "H01-FAULT-$Label-PASS"
}

$ErrorActionPreference = 'Stop'
$ready = 'C:\Lab\audit-h01-fault.ready'
try {
    if (Test-Path -LiteralPath $ready) { Say 'H01-FAULT-SECOND-BOOT'; return }
    Say "H01-FAULT-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    $installed = 'C:\Lab\own-backup-service.exe'
    $staged = 'C:\Lab\own-backup-service-h01.exe'
    $probe = 'C:\Lab\audit_h01.exe'

    # State 1: the old public row exists, the private insert never committed.
    RunNative "$installed /stop" | Out-Null
    Say (RunNative "$probe seed")
    RunNative "$staged /update" | Out-Null
    $hash = (Get-FileHash -LiteralPath $installed -Algorithm SHA256).Hash
    Say "H01-FAULT-SERVICE sha256=$hash"
    if ($hash -ne 'BB06982EBE7B68130E7326F204F1A1F54B92228310DB682AF2B4AE17DD116903') {
        throw 'installed service differs from H01 migration build'
    }
    WaitService
    Check 'BEFORE-PRIVATE-COMMIT' $false

    # State 2: private row committed, public row not yet deleted.
    RunNative "$installed /stop" | Out-Null
    Say (RunNative "$probe seed-dual")
    RunNative "$installed /start" | Out-Null
    WaitService
    Check 'AFTER-PRIVATE-COMMIT' $true

    # State 3: public row deleted, its old WAL/pages not yet cleaned.
    RunNative "$installed /stop" | Out-Null
    Say (RunNative "$probe seed-deleted-pending")
    RunNative "$installed /start" | Out-Null
    WaitService
    Check 'BEFORE-PUBLIC-CLEANUP' $true

    [IO.File]::WriteAllText($ready, 'pass')
    Say 'H01-FAULT-PASS'
} catch {
    Say "H01-FAULT-FAIL id=$($_.FullyQualifiedErrorId) line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    $log = Get-ChildItem 'C:\Lab\Logs' -Filter 'own-backup-service-*.log' -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if ($log) {
        foreach ($line in Get-Content -LiteralPath $log.FullName | Select-Object -Last 35) {
            $plain = $line -replace '[^\x20-\x7E]', '?'
            Say "H01-FAULT-SERVICE-LINE $($plain.Substring(0, [Math]::Min(450, $plain.Length)))"
        }
    }
    Say 'H01-FAULT-DONE'
    shutdown.exe /s /t 5 | Out-Null
}
