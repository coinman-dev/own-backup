# Final native checks for the dual-header .obk reader and writer.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

$ErrorActionPreference = 'Stop'
$exe = 'C:\Lab\obk_h03_tests.exe'
try {
    Say "H03-V2-SMOKE-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    Say "H03-V2-SMOKE-EXE sha256=$((Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash)"
    foreach ($test in @(
        'seal::obk::rebuild::tests::a_torn_new_header_recovers_the_previous_generation',
        'seal::obk::fold::tests::folding_an_increment_leaves_the_leaf_reading_exactly_what_it_did',
        'seal::obk::metadata::tests::a_reserved_sidecar_is_appended_and_the_data_is_untouched',
        'seal::obk::container_tests::a_legacy_single_header_image_still_opens_and_updates',
        'seal::obk::listing::tests::a_listing_uses_the_newest_surviving_header_slot',
        'seal::obk::fold::tests::a_sparse_eight_tib_chain_folds_only_its_defined_clusters'
    )) {
        $output = cmd.exe /d /c "`"$exe`" --exact $test 2>&1" | Out-String
        if ($LASTEXITCODE -ne 0) { throw "test failed: $test output=$output" }
        Say "H03-V2-SMOKE-PASS $test"
    }
    Say 'H03-V2-SMOKE-DONE'
} catch {
    Say "H03-V2-SMOKE-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    shutdown.exe /s /t 5 | Out-Null
}
