# A02: delete a real full + incremental job while one child denies deletion
# and another chain has an unfinished-consolidation marker.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}
function InvokeBackupCli([string[]]$Arguments) {
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = (& 'C:\Lab\own-backup-cli.exe' @Arguments 2>&1 | Out-String).Trim()
        return [pscustomobject]@{ Code = [int]$LASTEXITCODE; Output = [string]$output }
    } finally { $ErrorActionPreference = $old }
}

$ErrorActionPreference = 'Stop'
$held = $null
$marker = $null
try {
    Say "A02-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    $parsed = ConvertFrom-Json -InputObject ([IO.File]::ReadAllText('C:\ProgramData\OwnBackup\jobs.json'))
    $job = $null
    foreach ($candidate in $parsed) {
        if ($candidate.name -ne 'AuditSameLabelRolling') { continue }
        Say "A02-CANDIDATE id=$($candidate.id) images=$(@($candidate.last_run.images).Count) status=$($candidate.last_run.status)"
        $job = $candidate
    }
    if (-not $job) { throw 'A03 source job missing' }
    $images = @($job.last_run.images)
    if ($images.Count -ne 2) { throw "expected two source chains, got $($images.Count)" }
    $paths = @()
    foreach ($image in $images) {
        $base = [string]$image.base_image_path
        $child = [string]$image.image_path
        if (-not $base -or -not $child -or $base -eq $child) { throw 'last run is not full + increment' }
        if (-not (Test-Path -LiteralPath $base) -or -not (Test-Path -LiteralPath $child)) {
            throw 'one of the chain files is absent'
        }
        $paths += $base, $child
    }
    if (@($paths | Sort-Object -Unique).Count -ne 4) { throw 'chains overlap' }
    $hashes = @{}
    foreach ($path in $paths) {
        $hashes[$path] = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
        Say "A02-BEFORE file=$([IO.Path]::GetFileName($path)) sha256=$($hashes[$path])"
    }

    $held = [IO.FileStream]::new([string]$images[0].image_path,
        [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    $marker = ([string]$images[1].base_image_path) + '.consolidating.json'
    [IO.File]::WriteAllText($marker, '{}')
    Say "A02-GUARDS held=$([IO.Path]::GetFileName([string]$images[0].image_path)) marker=$([IO.Path]::GetFileName($marker))"

    $deleted = InvokeBackupCli -Arguments @('jobs', 'delete', [string]$job.id, '--delete-files')
    Say "A02-DELETE exit=$($deleted.Code)"
    if ($deleted.Code -ne 0) { throw "delete RPC failed: $($deleted.Output)" }
    foreach ($path in $paths) {
        if (-not $deleted.Output.Contains($path)) { throw "report did not name kept file $path" }
        if (-not (Test-Path -LiteralPath $path)) { throw "a protected file was deleted: $path" }
    }
    Say "A02-REPORT kept_paths=$($paths.Count)"
    if (-not (Test-Path -LiteralPath $marker)) { throw 'consolidation marker was deleted' }

    $held.Dispose(); $held = $null
    Remove-Item -LiteralPath $marker -Force; $marker = $null
    foreach ($path in $paths) {
        $after = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
        if ($after -ne $hashes[$path]) { throw "file changed during delete: $path" }
        Say "A02-AFTER file=$([IO.Path]::GetFileName($path)) sha256=$after"
    }
    $found = @{}
    foreach ($image in $images) {
        $path = [string]$image.image_path
        $mount = InvokeBackupCli -Arguments @('mount', $path)
        if ($mount.Code -ne 0) { throw "mount failed: $($mount.Output)" }
        try {
            Start-Sleep -Seconds 2
            $match = [regex]::Match($mount.Output, 'PhysicalDrive(\d+)')
            if (-not $match.Success) { throw 'mounted disk number absent' }
            $number = [int]$match.Groups[1].Value
            foreach ($letter in @(Get-Partition -DiskNumber $number -ErrorAction Stop |
                Where-Object { $_.DriveLetter } | ForEach-Object { $_.DriveLetter })) {
                foreach ($name in @('volume-A.txt', 'volume-B.txt')) {
                    $file = "${letter}:\$name"
                    if (Test-Path -LiteralPath $file) { $found[$name] = [IO.File]::ReadAllText($file) }
                }
            }
        } finally {
            $unmount = InvokeBackupCli -Arguments @('unmount', $path)
            if ($unmount.Code -ne 0) { throw "unmount failed: $($unmount.Output)" }
        }
    }
    if ($found['volume-A.txt'] -ne 'AUDIT-VOLUME-A' -or
        $found['volume-B.txt'] -ne 'AUDIT-VOLUME-B') {
        throw 'one of the retained points lost its file content'
    }
    Say 'A02-PASS retained=4 mounted=2 contents=2'
} catch {
    Say "A02-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    if ($held) { $held.Dispose() }
    if ($marker -and (Test-Path -LiteralPath $marker)) { Remove-Item -LiteralPath $marker -Force }
    Say 'A02-DONE'
    shutdown.exe /s /t 5 | Out-Null
}
