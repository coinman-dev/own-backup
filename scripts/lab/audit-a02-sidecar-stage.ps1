# A02: native Windows regression for legacy external .obkm sidecars.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}
$ErrorActionPreference = 'Stop'
try {
    $exe = 'C:\Lab\a02_chains_windows.exe'
    if (-not (Test-Path -LiteralPath $exe)) { throw 'native chain test binary missing' }
    Say "A02-SIDECAR-BEGIN sha256=$((Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash)"
    $tests = @(
        'chains::tests::retention_deletes_files',
        'chains::tests::a_child_that_cannot_be_deleted_keeps_its_base',
        'chains::tests::the_sidecar_never_goes_before_its_image',
        'chains::tests::a_missing_sidecar_is_not_a_failure'
    )
    foreach ($name in $tests) {
        $old = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            $output = (& $exe --exact $name --nocapture 2>&1 | Out-String).Trim()
            $code = [int]$LASTEXITCODE
        } finally { $ErrorActionPreference = $old }
        foreach ($line in ($output -split "`r?`n")) { if ($line) { Say "A02-SIDECAR-TEST $line" } }
        if ($code -ne 0 -or $output -notmatch '1 passed; 0 failed') { throw "native test failed: $name exit=$code" }
    }
    Say 'A02-SIDECAR-PASS tests=4'
} catch {
    Say "A02-SIDECAR-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    Say 'A02-SIDECAR-DONE'
    shutdown.exe /s /t 5 | Out-Null
}
