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
    if ($point -notin @('namespace-existing-directory','namespace-new-directory','namespace-new-directory-compact')) {throw 'invalid namespace case'}
    if ($phase -notin @('prepare','cut','verify')) {throw 'invalid phase'}
    $env:OWN_BACKUP_H03_NAMESPACE_ROOT='C:\Lab\h03-new-names'
    $env:OWN_BACKUP_H03_NAMESPACE_POINT=$point
    $env:OWN_BACKUP_H03_NAMESPACE_PHASE=$phase
    $test='engine_win::new_image_namespace_tests::vm_power_cut_after_new_images_finalize'
    $exe='C:\Lab\obk_boundary_tests.exe'
    Say "H03-NAMESPACE-BEGIN point=$point phase=$phase binary_sha256=$((Get-FileHash $exe -Algorithm SHA256).Hash)"
    $output=cmd.exe /d /c "`"$exe`" --ignored --exact $test --nocapture 2>&1" | Out-String
    [IO.File]::WriteAllText("C:\Lab\boundary-$phase.log",$output)
    if ($phase -eq 'cut') {throw "cut worker returned unexpectedly: $output"}
    if ($LASTEXITCODE -ne 0 -or $output -notmatch '1 passed') {throw "native namespace test failed: $output"}
    if ($phase -eq 'prepare') {Say "H03-BOUNDARY-PREPARED point=$point"}
    else {Say "H03-BOUNDARY-CASE-PASS point=$point"}
    shutdown.exe /s /t 5 | Out-Null
} catch {
    Say "H03-BOUNDARY-FAIL point=$point phase=$phase line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
