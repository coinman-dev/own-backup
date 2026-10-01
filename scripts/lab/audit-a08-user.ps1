function Say([string]$Text) {
    try {
        $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $p.Open(); $p.WriteLine($Text); $p.Close()
    } catch { }
}
$ErrorActionPreference = 'Stop'
try {
    Say "A08-USER-BEGIN at=$([DateTime]::UtcNow.ToString('o'))"
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & 'C:\Lab\audit_a08_restore_cancel.exe' 'C:\Lab\a08\config.json' 2>&1 |
            ForEach-Object { Say ([string]$_) }
        $code = $LASTEXITCODE
    } finally { $ErrorActionPreference = $old }
    if ($code -ne 0) { throw "restore client exit=$code" }
    [IO.File]::WriteAllText('C:\Lab\a08\client.done', 'done')
    $deadline = [DateTime]::UtcNow.AddMinutes(2)
    while (-not (Test-Path -LiteralPath 'C:\Lab\a08\restart.ready')) {
        if ([DateTime]::UtcNow -gt $deadline) { throw 'restart watcher timeout' }
        Start-Sleep -Seconds 1
    }
    & 'C:\Lab\audit_a08_restore_cancel.exe' 'C:\Lab\a08\config.json' '--check-history' 2>&1 |
        ForEach-Object { Say ([string]$_) }
    if ($LASTEXITCODE -ne 0) { throw 'reopened history check failed' }
    $mode = (Get-Content 'C:\Lab\a08-mode.txt' -Raw).Trim()
    if ($mode -in @('letter', 'mutations')) {
        $part = Get-Partition -DiskNumber 2 -PartitionNumber 1
        Say "A09-LETTER target=$($part.DriveLetter)"
        if ($part.DriveLetter -ne 'T') { throw 'requested T: was not assigned to the restored partition' }
        $file = 'T:\a07-data\large.bin'
        if ((Get-Item -LiteralPath $file).Length -ne 1100MB) { throw 'restored control file missing or short' }
        $buffer = New-Object byte[] (1MB)
        ([Random]::new(20260927)).NextBytes($buffer)
        $expected = [BitConverter]::ToString(([Security.Cryptography.SHA256]::Create()).ComputeHash($buffer)).Replace('-', '')
        $stream = [IO.File]::OpenRead($file)
        try {
            foreach ($offset in @(0L, ($stream.Length - 1MB))) {
                $null = $stream.Seek($offset, [IO.SeekOrigin]::Begin)
                $readBuffer = New-Object byte[] (1MB)
                $count = $stream.Read($readBuffer,0,$readBuffer.Length)
                if ($count -ne 1MB) { throw 'control block read short' }
                $hash = [BitConverter]::ToString(([Security.Cryptography.SHA256]::Create()).ComputeHash($readBuffer)).Replace('-', '')
                if ($hash -ne $expected) { throw "restored control block differs at $offset" }
            }
        } finally { $stream.Dispose() }
        Say "A09-CONTENTS bytes=1153433600 checked_blocks=2 sha256=$expected"
        if ($mode -eq 'mutations') {
            $sourceDll = 'E:\a07-data\kernel32.dll'
            $targetDll = 'T:\a07-data\kernel32.dll'
            $dllHash = (Get-FileHash -LiteralPath $sourceDll -Algorithm SHA256).Hash
            if ((Get-FileHash -LiteralPath $targetDll -Algorithm SHA256).Hash -ne $dllHash) { throw 'restored system file differs' }
            if ((Get-Content -LiteralPath $targetDll -Stream a09 -Raw) -ne
                (Get-Content -LiteralPath $sourceDll -Stream a09 -Raw)) { throw 'restored system file named stream differs' }
            $sourceAds = Get-Content -LiteralPath 'E:\a07-data\large.bin' -Stream a09 -Raw
            $targetAds = Get-Content -LiteralPath 'T:\a07-data\large.bin' -Stream a09 -Raw
            if ($targetAds -ne $sourceAds) { throw 'restored named stream differs' }
            $sourceLink = Get-Item -LiteralPath 'E:\a07-data\a09-link' -Force
            $targetLink = Get-Item -LiteralPath 'T:\a07-data\a09-link' -Force
            if (-not ($targetLink.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
                ([string]$sourceLink.Target) -eq ([string]$targetLink.Target)) { throw 'final reparse mutation did not persist' }
            Say "A09-SPECIAL-PASS system_sha256=$dllHash ads=true reparse_mutated=true"
        }
    }
    $result = [IO.File]::ReadAllBytes('C:\Lab\a08\results.json')
    Say "A08-RESULTS-BASE64 $([Convert]::ToBase64String($result))"
    $sessions = (& iscsicli.exe SessionList 2>&1 | Out-String).ToLowerInvariant()
    Say "A08-ISCSI orphan=$($sessions.Contains('ownbackup'))"
    if ($sessions.Contains('ownbackup')) { throw 'orphan iSCSI session remains' }
    Say 'A08-USER-PASS'
} catch {
    Say "A08-USER-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    if ((Get-Content 'C:\Lab\a08-mode.txt' -Raw).Trim() -in @('letter', 'mutations')) {
        $part = Get-Partition -DiskNumber 2 -PartitionNumber 1 -ErrorAction SilentlyContinue
        $taken = Get-Partition -DriveLetter T -ErrorAction SilentlyContinue
        Say "A09-LETTER-AFTER target_letter=$($part.DriveLetter) T_taken=$([bool]$taken)"
    }
} finally {
    if (Test-Path -LiteralPath 'C:\Lab\a09-watch.log') {
        Say "A09-WATCH-LOG $(Get-Content -LiteralPath 'C:\Lab\a09-watch.log' -Raw)"
    }
    if (Test-Path -LiteralPath 'C:\Lab\a09-mutation.log') {
        Say "A09-MUTATION-LOG $(Get-Content -LiteralPath 'C:\Lab\a09-mutation.log' -Raw)"
    }
    $log = 'C:\Lab\Logs\own-backup-service-' + [DateTime]::UtcNow.ToString('yyyy-MM-dd') + '.log'
    foreach ($line in @(Get-Content -LiteralPath $log -ErrorAction SilentlyContinue)) {
        if ($line -match 'restore|verification|verify|Restore|letter') { Say "A08-SERVICE-LOG $line" }
    }
    Say 'A08-USER-DONE'
    shutdown.exe /s /t 5 | Out-Null
}
