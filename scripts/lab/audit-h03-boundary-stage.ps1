function Say([string]$Text) {
    try {
        $port=New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One'
        $port.Open();$port.WriteLine($Text);$port.Close()
    } catch { }
}
$ErrorActionPreference='Stop'
try {
    $root='C:\Lab\h03-boundary'
    $point=(Get-Content 'C:\Lab\boundary-point.txt' -Raw).Trim()
    if ($point -notin @('before-rename','after-rename','header-before-sync')) {throw 'invalid cut point'}
    $env:OWN_BACKUP_H03_BOUNDARY_ROOT=$root
    $env:OWN_BACKUP_H03_BOUNDARY_POINT=$point
    $test='seal::obk::publication_faults::vm_power_cut_at_publication_boundary'
    $exe='C:\Lab\obk_boundary_tests.exe'
    $phase=(Get-Content 'C:\Lab\boundary-phase.txt' -Raw).Trim()
    if ($phase -notin @('prepare','cut','verify')) {throw 'invalid phase'}
    $env:OWN_BACKUP_H03_BOUNDARY_PHASE=$phase
    Say "H03-BOUNDARY-BEGIN point=$point binary_sha256=$((Get-FileHash $exe -Algorithm SHA256).Hash)"
    if ($phase -eq 'verify') {
        $output=cmd.exe /d /c "`"$exe`" --ignored --exact $test --nocapture 2>&1" | Out-String
        [IO.File]::WriteAllText('C:\Lab\boundary-verify.log',$output)
        if ($LASTEXITCODE -ne 0 -or $output -notmatch '1 passed') {throw "native verification failed: $output"}
        Say "H03-BOUNDARY-CASE-PASS point=$point"
        shutdown.exe /s /t 5 | Out-Null
    } elseif ($phase -eq 'prepare') {
        # Exercise a real destination sharing conflict before the planned crash.
        if ((Get-Content 'C:\Lab\sharing-check.txt' -Raw).Trim() -eq 'yes') {
            $check='seal::obk::publication_faults::an_open_handle_blocks_publication_and_retry_succeeds'
            $output=cmd.exe /d /c "`"$exe`" --exact $check --nocapture 2>&1" | Out-String
            [IO.File]::WriteAllText('C:\Lab\boundary-sharing.log',$output)
            if ($LASTEXITCODE -ne 0 -or $output -notmatch '1 passed') {throw "sharing violation check failed: $output"}
            Say 'H03-BOUNDARY-SHARING-PASS'
        }
        $output=cmd.exe /d /c "`"$exe`" --ignored --exact $test --nocapture 2>&1" | Out-String
        [IO.File]::WriteAllText('C:\Lab\boundary-prepare.log',$output)
        if ($LASTEXITCODE -ne 0 -or $output -notmatch '1 passed') {throw "native preparation failed: $output"}
        Say "H03-BOUNDARY-PREPARED point=$point"
        shutdown.exe /s /t 5 | Out-Null
    } else {
        $output=cmd.exe /d /c "`"$exe`" --ignored --exact $test --nocapture 2>&1" | Out-String
        throw "cut worker returned unexpectedly: $output"
    }
} catch {
    Say "H03-BOUNDARY-FAIL point=$point line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
