# Kill QEMU after the published image is synced and renamed, then boot again.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

$ErrorActionPreference = 'Stop'
$root = 'C:\Lab\h03-published'
$exe = 'C:\Lab\obk_h03_tests.exe'
$test = 'seal::obk::transaction::tests::vm_power_cut_after_publication_keeps_the_new_chain'
$waitingForHost = $false
try {
    Say "H03-PUBLISHED-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    Say "H03-PUBLISHED-EXE sha256=$((Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash)"
    $env:OWN_BACKUP_H03_PUBLISHED_DIR = $root
    if (Test-Path -LiteralPath (Join-Path $root 'full.obk')) {
        $env:OWN_BACKUP_H03_PUBLISHED_PHASE = 'verify'
        $output = cmd.exe /d /c "`"$exe`" --exact $test --nocapture 2>&1" | Out-String
        if ($LASTEXITCODE -ne 0) { throw "verify failed: $output" }
        Say 'H03-PUBLISHED-PASS chain_len=2 clusters=2'
    } else {
        $env:OWN_BACKUP_H03_PUBLISHED_PHASE = 'publish'
        $process = Start-Process -FilePath $exe -ArgumentList @('--exact', $test, '--nocapture') -PassThru -WindowStyle Hidden
        for ($tick = 0; $tick -lt 600; $tick++) {
            if (Test-Path -LiteralPath (Join-Path $root 'published-ready')) {
                Say 'H03-PUBLISHED-WINDOW new_image_synced=true'
                $waitingForHost = $true
                while ($true) { Start-Sleep -Seconds 1 }
            }
            if ($process.HasExited) { throw "publish test exited=$($process.ExitCode)" }
            Start-Sleep -Milliseconds 250
        }
        throw 'published marker timeout'
    }
} catch {
    Say "H03-PUBLISHED-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    if (-not $waitingForHost) { shutdown.exe /s /t 5 | Out-Null }
}
