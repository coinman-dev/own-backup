function Say([string]$Text) {
    try {
        $port=New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One'
        $port.Open();$port.WriteLine($Text);$port.Close()
    } catch { }
}
$ErrorActionPreference='Stop'
try {
    $point=(Get-Content 'C:\Lab\boundary-point.txt' -Raw).Trim()
    $phase=(Get-Content 'C:\Lab\boundary-phase.txt' -Raw).Trim()
    if ($point -notin @('cleanup-staged-before-rename','cleanup-owner-after-rename','cleanup-between-stage-and-owner')) {throw 'invalid cleanup point'}
    if ($phase -notin @('prepare','cut','verify')) {throw 'invalid phase'}
    $env:OWN_BACKUP_H03_CLEAN_ROOT='C:\Lab\h03-cleanup'
    $env:OWN_BACKUP_H03_CLEAN_POINT=$point
    $env:OWN_BACKUP_H03_CLEAN_PHASE=$phase
    $test='seal::obk::staging::faults::vm_power_cut_staged_cleanup'
    $exe='C:\Lab\obk_boundary_tests.exe'
    Say "H03-CLEANUP-BEGIN point=$point phase=$phase binary_sha256=$((Get-FileHash $exe -Algorithm SHA256).Hash)"
    if ($phase -eq 'prepare' -and $point -eq 'cleanup-staged-before-rename') {
        $guards=cmd.exe /d /c "`"$exe`" seal::obk::staging::tests:: --nocapture 2>&1" | Out-String
        [IO.File]::WriteAllText('C:\Lab\boundary-guards.log',$guards)
        if ($LASTEXITCODE -ne 0 -or $guards -notmatch '7 passed') {throw "ownership guards failed: $guards"}
        $exitTest='seal::obk::transaction::tests::process_exit_during_staged_write_leaves_the_original_image'
        $crash=cmd.exe /d /c "`"$exe`" --exact $exitTest --nocapture 2>&1" | Out-String
        [IO.File]::WriteAllText('C:\Lab\boundary-process-exit.log',$crash)
        if ($LASTEXITCODE -ne 0 -or $crash -notmatch '1 passed') {throw "process-exit cleanup failed: $crash"}
        Say 'H03-CLEANUP-GUARDS-PASS native_tests=7 process_exit=1'
    }
    $output=cmd.exe /d /c "`"$exe`" --ignored --exact $test --nocapture 2>&1" | Out-String
    [IO.File]::WriteAllText("C:\Lab\boundary-$phase.log",$output)
    if ($phase -eq 'cut') {throw "cut worker returned unexpectedly: $output"}
    if ($LASTEXITCODE -ne 0 -or $output -notmatch '1 passed') {throw "native cleanup test failed: $output"}
    if ($phase -eq 'prepare') {Say "H03-BOUNDARY-PREPARED point=$point"}
    else {Say "H03-BOUNDARY-CASE-PASS point=$point"}
    shutdown.exe /s /t 5 | Out-Null
} catch {
    Say "H03-BOUNDARY-FAIL point=$point phase=$phase line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
