# Measure the cost of replacing metadata in a large encrypted image on Windows.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

$ErrorActionPreference = 'Stop'
$exe = 'C:\Lab\obk_h03_tests.exe'
$env:OWN_BACKUP_H03_LARGE_DIR = 'C:\Lab\h03-large'
$env:OWN_BACKUP_H03_LARGE_MIB = '512'
$test = 'seal::obk::transaction::tests::measure_staged_metadata_update_on_a_large_image'
try {
    Say "H03-LARGE-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    Say "H03-LARGE-EXE sha256=$((Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash)"
    $result = cmd.exe /d /c "`"$exe`" --exact $test --nocapture 2>&1" | Out-String
    foreach ($line in ($result -split "`r?`n")) {
        if ($line -match 'H03-LARGE|test result:|FAILED|panicked') {
            Say "H03-LARGE-OUTPUT $line"
        }
    }
    if ($LASTEXITCODE -ne 0) { throw "test exit=$LASTEXITCODE" }
    Say 'H03-LARGE-PASS'
} catch {
    Say "H03-LARGE-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    shutdown.exe /s /t 5 | Out-Null
}
