# Disposable Windows VM acceptance of the manually started iSCSI target.
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
$target = $null
try {
    Say "A04-MANUAL-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    $image = 'C:\Lab\manual-probe.qcow2'
    $imageHash = (Get-FileHash -LiteralPath $image -Algorithm SHA256).Hash
    Say "A04-MANUAL-IMAGE bytes=$((Get-Item $image).Length) sha256=$imageHash"
    $before = @(Get-IscsiSession -ErrorAction SilentlyContinue).Count
    Say "A04-MANUAL-SESSIONS-BEFORE count=$before"
    if ($before -ne 0) { throw 'the test VM has a pre-existing iSCSI session' }

    $secretFile = 'C:\Lab\manual-chap-input.txt'
    [IO.File]::WriteAllText($secretFile, "0123456789abcdef`r`n")
    $target = Start-Process -FilePath 'C:\Lab\own-backup-cli.exe' `
        -ArgumentList @('iscsi-serve', $image, '--listen', '127.0.0.1:3260') `
        -RedirectStandardInput $secretFile `
        -RedirectStandardOutput 'C:\Lab\manual-target.out' `
        -RedirectStandardError 'C:\Lab\manual-target.err' `
        -PassThru -WindowStyle Hidden
    for ($tick = 0; $tick -lt 60; $tick++) {
        $target.Refresh()
        if ($target.HasExited) { throw "manual target exited early with $($target.ExitCode)" }
        if (Get-NetTCPConnection -LocalPort 3260 -State Listen -ErrorAction SilentlyContinue) { break }
        Start-Sleep -Seconds 1
    }
    if (-not (Get-NetTCPConnection -LocalPort 3260 -State Listen -ErrorAction SilentlyContinue)) {
        throw 'manual iSCSI target did not listen on loopback'
    }
    Say 'A04-MANUAL-TARGET listen=127.0.0.1:3260'
    Say (RunNative "C:\Lab\audit_manual_iscsi.exe $image")
    $after = @(Get-IscsiSession -ErrorAction SilentlyContinue).Count
    Say "A04-MANUAL-SESSIONS-AFTER count=$after"
    if ($after -ne 0) { throw 'manual iSCSI session remained after logout' }
    Say 'A04-MANUAL-PASS'
} catch {
    Say "A04-MANUAL-FAIL id=$($_.FullyQualifiedErrorId) line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    if ($target) { Stop-Process -Id $target.Id -Force -ErrorAction SilentlyContinue }
    if (Test-Path 'C:\Lab\manual-target.out') {
        foreach ($line in Get-Content 'C:\Lab\manual-target.out' | Select-Object -Last 8) {
            Say "A04-MANUAL-TARGET-OUT $line"
        }
    }
    if (Test-Path 'C:\Lab\manual-target.err') {
        foreach ($line in Get-Content 'C:\Lab\manual-target.err' | Select-Object -Last 8) {
            Say "A04-MANUAL-TARGET-ERR $line"
        }
    }
    Say 'A04-MANUAL-DONE'
    shutdown.exe /s /t 5 | Out-Null
}
