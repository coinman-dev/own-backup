# Active-console lab user starts service VerifyImage; SYSTEM watcher stops SCM.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

function RunCli([string[]]$Arguments) {
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = (& 'C:\Lab\own-backup-cli.exe' @Arguments 2>&1 | Out-String).Trim()
        if ($LASTEXITCODE -ne 0) { throw "cli exit=$LASTEXITCODE output=$output" }
        return $output
    } finally { $ErrorActionPreference = $old }
}

$ErrorActionPreference = 'Stop'
$done = 'C:\Lab\audit-h02-verify.done'
try {
    Say "H02-VERIFY-USER-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    for ($tick = 0; $tick -lt 120; $tick++) {
        if (Test-Path -LiteralPath 'C:\Lab\audit-h02-verify-watch.ready') { break }
        Start-Sleep -Seconds 1
    }
    if (-not (Test-Path -LiteralPath 'C:\Lab\audit-h02-verify-watch.ready')) {
        throw 'SYSTEM watcher did not start'
    }
    $image = Get-ChildItem 'C:\Lab\audit-2026-09-27\a07-images' -Filter '*.qcow2' -File |
        Sort-Object Length -Descending | Select-Object -First 1
    if (-not $image -or $image.Length -lt 500MB) { throw 'large A07 image missing' }
    # Hashing before verify would fill Windows' file cache and make the RPC
    # finish before SCM can stop it. This reference was measured on the
    # untouched base overlay in the earlier A07/H02 runs.
    $expected = '1A929016D9EFD47B1A06E398295AA51F023AC2AD3888ADE8491E1C420BF70DC8'
    Say "H02-VERIFY-IMAGE bytes=$($image.Length) expected_sha256=$expected"
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = (& 'C:\Lab\audit_h02_verify_stop.exe' $image.FullName 43992 2>&1 | Out-String).Trim()
        $code = [int]$LASTEXITCODE
    } finally { $ErrorActionPreference = $old }
    foreach ($line in ($output -split "`r?`n")) {
        if ($line -match 'H02-VERIFY-') { Say $line }
    }
    if ($code -ne 0) { throw "verify client exit=$code output=$output" }

    $deadline = [DateTime]::UtcNow.AddMinutes(6)
    while (-not (Test-Path -LiteralPath $done) -and [DateTime]::UtcNow -lt $deadline) {
        Start-Sleep -Seconds 1
    }
    if (-not (Test-Path -LiteralPath $done) -or
        (Get-Content -LiteralPath $done -Raw).Trim() -ne 'pass') {
        throw 'service watcher did not complete'
    }
    $same = (Get-FileHash -LiteralPath $image.FullName -Algorithm SHA256).Hash -eq $expected
    Say "H02-VERIFY-IMAGE-AFTER sha256_unchanged=$same"
    if (-not $same) { throw 'verify stop changed the image' }
    $null = RunCli @('mount', $image.FullName)
    $null = RunCli @('unmount', $image.FullName)
    Start-Sleep -Seconds 2
    $sessions = (& iscsicli.exe SessionList 2>&1 | Out-String)
    $orphan = $sessions.ToLowerInvariant().Contains('ownbackup')
    Say "H02-VERIFY-ISCSI-ORPHAN present=$orphan"
    if ($orphan) { throw 'iSCSI session remained after mount/unmount' }
    Say 'H02-VERIFY-USER-PASS'
} catch {
    Say "H02-VERIFY-USER-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    Say 'H02-VERIFY-USER-DONE'
    shutdown.exe /s /t 5 | Out-Null
}
