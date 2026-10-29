# Active-console client; mutate only the first 128 MiB of the disposable file.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}
function RunBackupCli([string[]]$Arguments) {
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = (& 'C:\Lab\own-backup-cli.exe' @Arguments 2>&1 | Out-String).Trim()
        if ($LASTEXITCODE -ne 0) { throw "cli exit=$LASTEXITCODE output=$output" }
        return $output
    } finally { $ErrorActionPreference = $old }
}
function WaitRun([string]$Previous) {
    $deadline = [DateTime]::UtcNow.AddMinutes(8)
    do {
        $job = (RunBackupCli @('jobs', 'show', $script:jobId)) | ConvertFrom-Json
        if ($job.last_run -and $job.last_run.id -ne $Previous) {
            if ($job.last_run.status -ne 'success') { throw "backup failed: $($job.last_run.error)" }
            Say "H02-CONSOL-RUN status=$($job.last_run.status) kind=$($job.last_run.backup_kind) folded=$($job.last_run.consolidated_increments) image=$($job.last_run.image_path)"
            return $job.last_run
        }
        Start-Sleep -Seconds 1
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'backup timeout'
}
function WritePrefix([int]$Seed) {
    $buffer = New-Object byte[] (1MB)
    ([Random]::new($Seed)).NextBytes($buffer)
    $stream = [IO.File]::Open($script:sourceFile, [IO.FileMode]::Open, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    try {
        for ($i = 0; $i -lt 128; $i++) { $stream.Write($buffer, 0, $buffer.Length) }
        $stream.Flush($true)
    } finally { $stream.Dispose() }
    Say "H02-CONSOL-SOURCE prefix_mib=128 seed=$Seed"
}
function ExpectedHash([int]$Seed) {
    $prefix = New-Object byte[] (1MB)
    $tail = New-Object byte[] (1MB)
    ([Random]::new($Seed)).NextBytes($prefix)
    ([Random]::new(20260927)).NextBytes($tail)
    $hash = [Security.Cryptography.SHA256]::Create()
    try {
        for ($i = 0; $i -lt 1100; $i++) {
            if ($i -lt 128) { $block = $prefix } else { $block = $tail }
            $null = $hash.TransformBlock($block, 0, $block.Length, $block, 0)
        }
        $null = $hash.TransformFinalBlock([byte[]]@(), 0, 0)
        return [BitConverter]::ToString($hash.Hash).Replace('-', '')
    } finally { $hash.Dispose() }
}
function Arm([string]$Phase) {
    $path = "C:\Lab\h02-consolidation-$Phase.arm.json"
    $base = Get-ChildItem -LiteralPath $script:folder -Filter '*full_*.qcow2' -File | Select-Object -First 1
    if (-not $base) { throw 'base absent before arming' }
    [IO.File]::WriteAllText($path + '.tmp', (@{ job_id = $script:jobId; folder = $script:folder; base_bytes = $base.Length } | ConvertTo-Json))
    Move-Item -LiteralPath ($path + '.tmp') -Destination $path -Force
    Say "H02-CONSOL-ARM phase=$Phase"
    # Allow the host to throttle the system disk before starting the run.
    Start-Sleep -Seconds 3
}
function WaitWatcher([string]$Phase) {
    $path = "C:\Lab\h02-consolidation-$Phase.result.json"
    $deadline = [DateTime]::UtcNow.AddMinutes(8)
    while (-not (Test-Path -LiteralPath $path)) {
        if ([DateTime]::UtcNow -gt $deadline) { throw "watcher timeout: $Phase" }
        Start-Sleep -Seconds 1
    }
    $result = Get-Content -LiteralPath $path -Raw | ConvertFrom-Json
    if (-not $result.passed) { throw "watcher failed: $($result.error)" }
    return $result
}
function CheckPoint([string]$Image, [string]$Expected) {
    $verify = RunBackupCli @('verify', $Image)
    if ($verify -notmatch 'Result: VALID') { throw "verify did not report VALID: $verify" }
    Say "H02-CONSOL-VERIFY image=$([IO.Path]::GetFileName($Image)) valid=True"
    $mounted = RunBackupCli @('mount', $Image)
    try {
        $disk = [regex]::Match($mounted, 'PhysicalDrive(\d+)')
        if (-not $disk.Success) { throw 'mounted disk number absent' }
        $letters = @(Get-Partition -DiskNumber ([int]$disk.Groups[1].Value) |
            Where-Object { $_.DriveLetter } | ForEach-Object { $_.DriveLetter })
        $file = $letters | ForEach-Object { "${_}:\a07-data\large.bin" } |
            Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
        if (-not $file -or (Get-Item -LiteralPath $file).Length -ne 1100MB) { throw 'control file missing or length changed' }
        $actual = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash
        Say "H02-CONSOL-CONTENTS image=$([IO.Path]::GetFileName($Image)) bytes=1153433600 sha256=$actual expected=$Expected"
        if ($actual -ne $Expected) { throw 'control file differs' }
    } finally { $null = RunBackupCli @('unmount', $Image) }
    Say "H02-CONSOL-IMAGE image=$([IO.Path]::GetFileName($Image)) bytes=$((Get-Item -LiteralPath $Image).Length) sha256=$((Get-FileHash -LiteralPath $Image -Algorithm SHA256).Hash)"
}
$ErrorActionPreference = 'Stop'
$action = 'stop'
if (Test-Path -LiteralPath 'C:\Lab\h02-consolidation.action') {
    $action = (Get-Content -LiteralPath 'C:\Lab\h02-consolidation.action' -Raw).Trim()
}
if ($action -notin @('stop', 'update')) { throw 'invalid consolidation action' }
try {
    Say "H02-CONSOL-USER-BEGIN at=$([DateTime]::UtcNow.ToString('o'))"
    $deadline = [DateTime]::UtcNow.AddMinutes(2)
    while (-not (Test-Path -LiteralPath 'C:\Lab\h02-consolidation-watch.ready')) {
        if ([DateTime]::UtcNow -gt $deadline) { throw 'watcher not ready' }
        Start-Sleep -Seconds 1
    }
    $devices = Get-Content 'C:\Lab\audit-2026-09-27\devices.json' -Raw | ConvertFrom-Json
    $source = "$($devices.source_letter):"
    $script:sourceFile = "$source\a07-data\large.bin"
    if ((Get-Item -LiteralPath $script:sourceFile).Length -ne 1100MB) { throw 'source fixture missing' }
    $script:folder = 'C:\Lab\h02-consolidation\images'
    New-Item -ItemType Directory -Path $script:folder -Force | Out-Null
    $created = RunBackupCli @('jobs', 'new', '--name', 'AuditH02Consolidation', '--source', $source,
        '--dest', $script:folder, '--format', 'qcow2', '--method', 'incremental',
        '--full-every-n', '0', '--rolling-increments', '1', '--keep-chains', '2',
        '--schedule', 'none', '--no-exclusions', '--validate-after=false')
    $script:jobId = [regex]::Match($created, '[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}').Value
    if (-not $script:jobId) { throw 'new job ID missing' }
    $expected1 = ExpectedHash 20260931
    $expected2 = ExpectedHash 20260932
    $expected3 = ExpectedHash 20260933
    $null = RunBackupCli @('jobs', 'run', $script:jobId)
    $run = WaitRun ''
    if ($run.backup_kind -ne 'full') { throw 'first run was not full' }
    WritePrefix 20260931
    $previous = $run.id
    $null = RunBackupCli @('jobs', 'run', $script:jobId)
    $run = WaitRun $previous
    if ($run.backup_kind -ne 'incremental') { throw 'first increment missing' }
    WritePrefix 20260932
    Arm 'stop'
    $previous = $run.id
    $null = RunBackupCli @('jobs', 'run', $script:jobId)
    $stop = WaitWatcher 'stop'
    $run = WaitRun $previous
    $images = @(Get-ChildItem -LiteralPath $script:folder -Filter '*.qcow2' -File)
    if ($images.Count -ne 2) { throw 'graceful consolidation did not leave two points' }
    foreach ($image in $images) {
        CheckPoint $image.FullName $(if ($image.Name -match '[_-]full_') { $expected1 } else { $expected2 })
    }
    Say "H02-CONSOL-CONTROL-PASS action=$action points=2 folded=1 marker_absent=True"
    if ($action -eq 'stop') {
        WritePrefix 20260933
        Arm 'kill'
        $null = RunBackupCli @('jobs', 'run', $script:jobId)
        $kill = WaitWatcher 'kill'
        if (-not (Test-Path -LiteralPath (Join-Path $script:folder $kill.marker))) { throw 'marker lost after restart' }
        # Recovery must run even when further consolidation has been switched off.
        $null = RunBackupCli @('jobs', 'set', $script:jobId, '--rolling-increments', '0')
        $previous = ((RunBackupCli @('jobs', 'show', $script:jobId)) | ConvertFrom-Json).last_run.id
        $null = RunBackupCli @('jobs', 'run', $script:jobId)
        $null = WaitRun $previous
        if (@(Get-ChildItem -LiteralPath $script:folder -Filter '*.consolidating.json' -File).Count -ne 0) {
            throw 'pending marker survived recovery'
        }
        $images = @(Get-ChildItem -LiteralPath $script:folder -Filter '*.qcow2' -File)
        if ($images.Count -ne 3) { throw 'recovered chain does not contain three expected points' }
        foreach ($image in $images) {
            CheckPoint $image.FullName $(if ($image.Name -match '[_-]full_') { $expected2 } else { $expected3 })
        }
        Say 'H02-CONSOL-RECOVERY-PASS points=3 marker_absent=True policy_consolidation=False'
    }
    $orphan = ((& iscsicli.exe SessionList 2>&1 | Out-String).ToLowerInvariant()).Contains('ownbackup')
    Say "H02-CONSOL-ISCSI orphan=$orphan"
    if ($orphan) { throw 'orphan iSCSI session remains' }
    Say 'H02-CONSOL-USER-PASS'
} catch {
    Say "H02-CONSOL-USER-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    # Preserve the recovery log without extracting a Windows disk on the host.
    $log = 'C:\Lab\Logs\own-backup-service-' + [DateTime]::UtcNow.ToString('yyyy-MM-dd') + '.log'
    foreach ($line in @(Get-Content -LiteralPath $log -ErrorAction SilentlyContinue)) {
        if ($line -match 'Consolidation|consolidations interrupted|AuditH02Consolidation') {
            Say "H02-CONSOL-FINAL-LOG $line"
        }
    }
    Say 'H02-CONSOL-USER-DONE'
    shutdown.exe /s /t 5 | Out-Null
}
