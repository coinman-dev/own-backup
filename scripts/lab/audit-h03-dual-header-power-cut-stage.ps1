# Kill QEMU after tearing the inactive header of a dual-header .obk image.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

$ErrorActionPreference = 'Stop'
$root = 'C:\Lab\h03-v2-torn'
$exe = 'C:\Lab\obk_h03_tests.exe'
$test = 'seal::obk::transaction::tests::vm_power_cut_during_second_header_write_recovers_old_point'
$waitingForHost = $false
try {
    Say "H03-V2-TORN-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    Say "H03-V2-TORN-EXE sha256=$((Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash)"
    $env:OWN_BACKUP_H03_V2_POWER_CUT_DIR = $root
    if (Test-Path -LiteralPath (Join-Path $root 'full.obk')) {
        $env:OWN_BACKUP_H03_V2_POWER_CUT_PHASE = 'verify'
        $output = cmd.exe /d /c "`"$exe`" --exact $test --nocapture 2>&1" | Out-String
        if ($LASTEXITCODE -ne 0) { throw "verify failed: $output" }
        Say 'H03-V2-TORN-PASS chain_len=2 clusters=2'
    } else {
        foreach ($check in @(
            'seal::obk::rebuild::tests::a_torn_new_header_recovers_the_previous_generation',
            'seal::obk::fold::tests::folding_an_increment_leaves_the_leaf_reading_exactly_what_it_did',
            'seal::obk::metadata::tests::a_reserved_sidecar_is_appended_and_the_data_is_untouched',
            'seal::obk::container_tests::a_legacy_single_header_image_still_opens_and_updates'
        )) {
            $output = cmd.exe /d /c "`"$exe`" --exact $check 2>&1" | Out-String
            if ($LASTEXITCODE -ne 0) { throw "native test failed: $check output=$output" }
            Say "H03-V2-NATIVE-PASS $check"
        }
        $env:OWN_BACKUP_H03_V2_POWER_CUT_PHASE = 'tear'
        $process = Start-Process -FilePath $exe -ArgumentList @('--exact', $test, '--nocapture') -PassThru -WindowStyle Hidden
        for ($tick = 0; $tick -lt 600; $tick++) {
            if (Test-Path -LiteralPath (Join-Path $root 'torn-ready')) {
                Say 'H03-V2-TORN-WINDOW second_header_torn=true'
                $waitingForHost = $true
                while ($true) { Start-Sleep -Seconds 1 }
            }
            if ($process.HasExited) { throw "test exited=$($process.ExitCode)" }
            Start-Sleep -Milliseconds 250
        }
        throw 'torn marker timeout'
    }
} catch {
    Say "H03-V2-TORN-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    if (-not $waitingForHost) { shutdown.exe /s /t 5 | Out-Null }
}
