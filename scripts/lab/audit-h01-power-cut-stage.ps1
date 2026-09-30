# Run as SYSTEM on a disposable Windows VM overlay. The host forcibly stops
# QEMU after H01-POWER-CUT-WINDOW and boots the same overlay again for recovery.
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

$ErrorActionPreference = 'Stop'
$probe = 'C:\Lab\audit_h01.exe'
$service = 'C:\Lab\own-backup-service.exe'
$marker = 'C:\Lab\audit-h01-power-cut.started'
$waitingForHost = $false
try {
    if (-not (Test-Path -LiteralPath $probe)) { throw 'audit_h01.exe missing' }
    Say "H01-POWER-CUT-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    Say "H01-POWER-CUT-SERVICE sha256=$((Get-FileHash -LiteralPath $service -Algorithm SHA256).Hash)"

    if (-not (Test-Path -LiteralPath $marker)) {
        RunNative "$service /stop" | Out-Null
        Say (RunNative "$probe seed-crash-batch")
        [IO.File]::WriteAllText($marker, 'seeded')
        RunNative "$service /start" | Out-Null
        for ($tick = 0; $tick -lt 600; $tick++) {
            $remaining = [int](RunNative "$probe count-crash-batch")
            if ($remaining -gt 0 -and $remaining -lt 4096) {
                Say "H01-POWER-CUT-WINDOW public_remaining=$remaining"
                $waitingForHost = $true
                while ($true) { Start-Sleep -Seconds 1 }
            }
            if ($remaining -eq 0) { throw 'migration finished before the power cut window' }
            Start-Sleep -Milliseconds 250
        }
        throw 'migration did not start'
    }

    Say 'H01-POWER-CUT-RECOVERY-BOOT'
    for ($tick = 0; $tick -lt 600; $tick++) {
        $remaining = [int](RunNative "$probe count-crash-batch")
        if ($remaining -eq 0) { break }
        Start-Sleep -Seconds 1
    }
    Say (RunNative "$probe check-crash-batch")
    Say 'H01-POWER-CUT-PASS'
} catch {
    Say "H01-POWER-CUT-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    if (-not $waitingForHost) { shutdown.exe /s /t 5 | Out-Null }
}
