# Native .obk publication test on a disposable Windows QEMU overlay.
# The test executable is copied from the answer CD to C:\Lab at startup.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

$ErrorActionPreference = 'Stop'
try {
    $exe = 'C:\Lab\obk_h03_tests.exe'
    if (-not (Test-Path -LiteralPath $exe)) { throw 'native test executable missing' }
    Say "H03-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    Say "H03-EXE sha256=$((Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash)"
    $tests = @(
        'seal::obk::transaction::tests::a_torn_staged_header_cannot_replace_a_previous_restore_point',
        'seal::obk::transaction::tests::process_exit_during_staged_write_leaves_the_original_image',
        'seal::obk::fold::tests::folding_an_increment_leaves_the_leaf_reading_exactly_what_it_did',
        'seal::obk::metadata::tests::replacing_the_sidecar_twice_uses_a_new_nonce_every_time'
    )
    foreach ($test in $tests) {
        $result = cmd.exe /d /c "`"$exe`" --exact $test --nocapture 2>&1" | Out-String
        if ($LASTEXITCODE -ne 0) {
            foreach ($line in ($result -split "`r?`n")) {
                if ($line.Trim()) { Say "H03-TEST-OUTPUT $line" }
            }
            throw "test failed: $test exit=$LASTEXITCODE"
        }
        Say "H03-PASS $test"
    }
    Say 'H03-DONE'
} catch {
    Say "H03-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    shutdown.exe /s /t 5 | Out-Null
}
