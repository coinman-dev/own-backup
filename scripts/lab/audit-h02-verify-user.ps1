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
    $rpcPassed = $code -eq 0
    if (-not $rpcPassed) { Say "H02-VERIFY-RPC-FAIL exit=$code output=$output" }

    $deadline = [DateTime]::UtcNow.AddMinutes(6)
    while (-not (Test-Path -LiteralPath $done) -and [DateTime]::UtcNow -lt $deadline) {
        Start-Sleep -Seconds 1
    }
    if (-not (Test-Path -LiteralPath $done) -or
        (Get-Content -LiteralPath $done -Raw).Trim() -ne 'pass') {
        throw 'service watcher did not complete'
    }
    $result = Get-Content -LiteralPath 'C:\Lab\audit-h02-verify-watch.result.json' -Raw | ConvertFrom-Json
    $cancelled = [regex]::Match($output, 'code=Cancelled at=(\d+)')
    $beforeStopped = $cancelled.Success -and
        [long]$cancelled.Groups[1].Value -le [long]$result.stopped_at_ms
    Say "H02-VERIFY-ORDER cancelled_before_stopped=$beforeStopped"
    $same = (Get-FileHash -LiteralPath $image.FullName -Algorithm SHA256).Hash -eq $expected
    Say "H02-VERIFY-IMAGE-AFTER sha256_unchanged=$same"
    if (-not $same) { throw 'verify stop changed the image' }
    $mount = RunCli @('mount', $image.FullName)
    try {
        $disk = [regex]::Match($mount, 'PhysicalDrive(\d+)')
        if (-not $disk.Success) { throw 'mounted disk number missing' }
        $letters = @(Get-Partition -DiskNumber ([int]$disk.Groups[1].Value) |
            Where-Object { $_.DriveLetter } | ForEach-Object { $_.DriveLetter })
        $file = $letters | ForEach-Object { "${_}:\a07-data\large.bin" } |
            Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
        if (-not $file) { throw 'control file absent in mounted point' }
        $expectedBlock = New-Object byte[] (1MB)
        ([Random]::new(20260927)).NextBytes($expectedBlock)
        $hash = [Security.Cryptography.SHA256]::Create()
        $stream = [IO.File]::OpenRead($file)
        try {
            if ($stream.Length -ne 1100MB) { throw 'control file length changed' }
            $expectedHash = [BitConverter]::ToString($hash.ComputeHash($expectedBlock))
            foreach ($offset in @(0L, ($stream.Length - 1MB))) {
                $null = $stream.Seek($offset, [IO.SeekOrigin]::Begin)
                $block = New-Object byte[] (1MB)
                $read = 0
                while ($read -lt $block.Length) {
                    $got = $stream.Read($block, $read, $block.Length - $read)
                    if ($got -eq 0) { throw 'control file read ended early' }
                    $read += $got
                }
                if ([BitConverter]::ToString($hash.ComputeHash($block)) -ne $expectedHash) {
                    throw "control file differs at $offset"
                }
            }
            Say "H02-VERIFY-CONTENTS blocks=2 bytes_checked=2097152 block_sha256=$($expectedHash.Replace('-', ''))"
        } finally { $stream.Dispose(); $hash.Dispose() }
    } finally { $null = RunCli @('unmount', $image.FullName) }
    Start-Sleep -Seconds 2
    $sessions = (& iscsicli.exe SessionList 2>&1 | Out-String)
    $orphan = $sessions.ToLowerInvariant().Contains('ownbackup')
    Say "H02-VERIFY-ISCSI-ORPHAN present=$orphan"
    if ($orphan) { throw 'iSCSI session remained after mount/unmount' }
    if (-not $rpcPassed -or -not $beforeStopped) { throw 'VerifyImage did not return Cancelled before Stopped' }
    Say 'H02-VERIFY-USER-PASS'
} catch {
    Say "H02-VERIFY-USER-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    Say 'H02-VERIFY-USER-DONE'
    shutdown.exe /s /t 5 | Out-Null
}
