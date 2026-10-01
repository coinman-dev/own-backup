# A02: exercise the real GUI delete button while two chains are protected.
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
    Say "A02-GUI-ACTION-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    $parsed = ConvertFrom-Json -InputObject ([IO.File]::ReadAllText('C:\ProgramData\OwnBackup\jobs.json'))
    $job = $null
    foreach ($candidate in $parsed) {
        if ($candidate.name -ne 'AuditSameLabelRolling') { continue }
        Say "A02-CANDIDATE id=$($candidate.id) images=$(@($candidate.last_run.images).Count) status=$($candidate.last_run.status)"
        $job = $candidate
    }
    if (-not $job) {
        $devices = Get-Content -LiteralPath 'C:\Lab\audit-2026-09-27\devices.json' -Raw | ConvertFrom-Json
        $dest = 'C:\Lab\a02-2026-10-01-images'
        New-Item -ItemType Directory -Force -Path $dest | Out-Null
        $templateReply = InvokeBackupCli -Arguments @('jobs', 'template')
        if ($templateReply.Code -ne 0) { throw "job template failed: $($templateReply.Output)" }
        $template = $templateReply.Output | ConvertFrom-Json
        $template.name = 'AuditSameLabelRolling'
        $template.source = [string]$devices.data_a
        $template.sources = @([string]$devices.data_a, [string]$devices.data_b)
        $template.source_label = 'DATA'
        $template.source_labels = @('DATA', 'DATA')
        $template.dest_folder = $dest
        $template.image_format = 'qcow2'
        $template.schedule.kind = 'none'
        $template.scheme.method = 'incremental'
        $template.scheme.keep_chains = 1
        $template.scheme.rolling_incrementals = 1
        $template.exclusions.patterns = @()
        $jobFile = 'C:\Lab\a02-job.json'
        [IO.File]::WriteAllText($jobFile, ($template | ConvertTo-Json -Depth 32), ([Text.UTF8Encoding]::new($false)))
        $saved = InvokeBackupCli -Arguments @('jobs', 'save', '--file', $jobFile)
        if ($saved.Code -ne 0) { throw "job save failed: $($saved.Output)" }
        $id = [regex]::Match($saved.Output, '[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}').Value
        if (-not $id) { throw 'saved job id absent' }
        $previous = ''
        for ($run = 1; $run -le 2; $run++) {
            $started = InvokeBackupCli -Arguments @('jobs', 'run', $id)
            if ($started.Code -ne 0) { throw "job run $run failed: $($started.Output)" }
            $finished = $false
            for ($tick = 0; $tick -lt 150; $tick++) {
                $shown = InvokeBackupCli -Arguments @('jobs', 'show', $id)
                if ($shown.Code -ne 0) { throw "job show failed: $($shown.Output)" }
                $job = $shown.Output | ConvertFrom-Json
                if ($job.last_run -and $job.last_run.finished_at -ne $previous) {
                    if ($job.last_run.status -ne 'success') { throw "job run $run status=$($job.last_run.status)" }
                    $previous = $job.last_run.finished_at
                    $finished = $true
                    break
                }
                Start-Sleep -Seconds 2
            }
            if (-not $finished) { throw "job run $run timed out" }
            Say "A02-FIXTURE run=$run kind=$($job.last_run.backup_kind) images=$(@($job.last_run.images).Count)"
        }
        if ($job.last_run.backup_kind -ne 'incremental') { throw 'fixture did not produce an increment' }
    }
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

    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $admin = [Security.Principal.WindowsPrincipal]::new($identity).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
    if ($admin) { throw 'GUI user is an administrator' }
    $guiMarker = Join-Path ([IO.Path]::GetTempPath()) 'own-backup-gui-up'
    Remove-Item -LiteralPath $guiMarker -Force -ErrorAction SilentlyContinue
    $gui = Start-Process -FilePath 'C:\Lab\own-backup-gui.exe' -PassThru
    for ($tick = 0; $tick -lt 60; $tick++) {
        $gui.Refresh()
        if ($gui.HasExited -or (Test-Path -LiteralPath $guiMarker)) { break }
        Start-Sleep -Seconds 2
    }
    $gui.Refresh()
    if ($gui.HasExited -or $gui.MainWindowTitle -ne 'Own Backup' -or -not (Test-Path -LiteralPath $guiMarker)) {
        throw 'ordinary-user GUI did not render'
    }
    Say "A02-GUI-ACTION-READY job=$($job.id) images=$($paths.Count) pid=$($gui.Id) admin=$admin"
    Add-Type -AssemblyName System.Windows.Forms
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class A02Mouse {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint x, uint y, uint data, UIntPtr extra);
}
'@
    $null = [A02Mouse]::SetForegroundWindow([IntPtr]$gui.MainWindowHandle)
    Start-Sleep -Milliseconds 500
    [Windows.Forms.SendKeys]::SendWait(('{TAB}' * 16) + '{ENTER}')
    Say 'A02-GUI-ACTION-MODAL inspect-selected-job'
    Start-Sleep -Seconds 15
    if (-not [A02Mouse]::SetCursorPos(295, 414)) { throw 'cannot move GUI cursor to delete-files checkbox' }
    Start-Sleep -Milliseconds 200
    [A02Mouse]::mouse_event(0x02, 0, 0, 0, [UIntPtr]::Zero)
    [A02Mouse]::mouse_event(0x04, 0, 0, 0, [UIntPtr]::Zero)
    Say 'A02-GUI-ACTION-CHECKED delete-files'
    Start-Sleep -Seconds 5
    if (-not [A02Mouse]::SetCursorPos(710, 502)) { throw 'cannot move GUI cursor to confirm button' }
    Start-Sleep -Milliseconds 200
    [A02Mouse]::mouse_event(0x02, 0, 0, 0, [UIntPtr]::Zero)
    [A02Mouse]::mouse_event(0x04, 0, 0, 0, [UIntPtr]::Zero)
    Say 'A02-GUI-ACTION-CLICKED-CONFIRM via-UI'
    $gone = $false
    for ($tick = 0; $tick -lt 30; $tick++) {
        $shown = InvokeBackupCli -Arguments @('jobs', 'show', [string]$job.id)
        if ($shown.Code -ne 0) { $gone = $true; break }
        Start-Sleep -Seconds 2
    }
    if (-not $gone) { throw 'GUI did not delete the selected job within one minute' }
    Say 'A02-GUI-ACTION-REMOVED job_absent=true'
    Start-Sleep -Seconds 30
    foreach ($path in $paths) {
        if (-not (Test-Path -LiteralPath $path)) { throw "a protected file was deleted: $path" }
    }
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
    Say 'A02-GUI-ACTION-PASS retained=4 mounted=2 contents=2'
} catch {
    Say "A02-GUI-ACTION-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    if ($held) { $held.Dispose() }
    if ($marker -and (Test-Path -LiteralPath $marker)) { Remove-Item -LiteralPath $marker -Force }
    Say 'A02-GUI-ACTION-DONE'
    shutdown.exe /s /t 5 | Out-Null
}
