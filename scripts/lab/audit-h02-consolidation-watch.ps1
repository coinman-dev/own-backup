# SYSTEM: stop on the first fold, forcibly terminate on the second fold.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}
$ErrorActionPreference = 'Stop'
$log = 'C:\Lab\Logs\own-backup-service-' + [DateTime]::UtcNow.ToString('yyyy-MM-dd') + '.log'
$action = 'stop'
if (Test-Path -LiteralPath 'C:\Lab\h02-consolidation.action') {
    $action = (Get-Content -LiteralPath 'C:\Lab\h02-consolidation.action' -Raw).Trim()
}
if ($action -notin @('stop', 'update')) { throw 'invalid consolidation action' }
$phases = $(if ($action -eq 'update') { @('stop') } else { @('stop', 'kill') })
[IO.File]::WriteAllText('C:\Lab\h02-consolidation-watch.ready', 'ready')
foreach ($phase in $phases) {
    $resultPath = "C:\Lab\h02-consolidation-$phase.result.json"
    $result = @{ passed = $false; phase = $phase; action = $action }
    $linesBefore = @(Get-Content -LiteralPath $log -ErrorAction SilentlyContinue).Count
    try {
        $armPath = "C:\Lab\h02-consolidation-$phase.arm.json"
        $deadline = [DateTime]::UtcNow.AddMinutes(10)
        while (-not (Test-Path -LiteralPath $armPath)) {
            if ([DateTime]::UtcNow -gt $deadline) { throw 'arm timeout' }
            Start-Sleep -Milliseconds 200
        }
        $arm = Get-Content -LiteralPath $armPath -Raw | ConvertFrom-Json
        $servicePid = [int](Get-CimInstance Win32_Service -Filter "Name='OwnBackup'").ProcessId
        if (-not $servicePid) { throw 'service not running' }
        $marker = $null
        while (-not $marker) {
            $marker = Get-ChildItem -LiteralPath $arm.folder -Filter '*.consolidating.json' -File |
                Select-Object -First 1
            if ([DateTime]::UtcNow -gt $deadline) { throw 'marker timeout' }
            if (-not $marker) { Start-Sleep -Milliseconds 50 }
        }
        $record = Get-Content -LiteralPath $marker.FullName -Raw | ConvertFrom-Json
        if ($record.merged) { throw 'fold already completed before observation' }
        $base = Join-Path $arm.folder $record.base
        Start-Sleep -Seconds 3
        if (-not (Test-Path -LiteralPath $marker.FullName)) { throw 'fold finished before action' }
        $record = Get-Content -LiteralPath $marker.FullName -Raw | ConvertFrom-Json
        if ($record.merged) { throw 'fold finished before action' }
        $result.marker = $marker.Name
        $result.marker_before = $record
        $result.old_pid = $servicePid
        Say "H02-CONSOL-ACTIVE phase=$phase pid=$servicePid merged=False observed_seconds=3 at=$([DateTime]::UtcNow.ToString('o'))"
        if ($phase -eq 'stop') {
            $stopWatch = [Diagnostics.Stopwatch]::StartNew()
            if ($action -eq 'update') {
                $output = cmd.exe /d /c 'C:\Lab\own-backup-service-h02.exe /update 2>&1' | Out-String
            } else {
                $output = cmd.exe /d /c 'C:\Lab\own-backup-service-h02.exe /stop 2>&1' | Out-String
            }
            $stopWatch.Stop()
            if ($LASTEXITCODE -ne 0) { throw "stop command failed: $output" }
            $result.stop_wait_ms = $stopWatch.ElapsedMilliseconds
            $result.stop_command_state = [string](Get-Service OwnBackup).Status
            Say "H02-CONSOL-CONTROL action=$action exit=0 elapsed_ms=$($result.stop_wait_ms) state=$($result.stop_command_state)"
            $wantedState = $(if ($action -eq 'update') { 'Running' } else { 'Stopped' })
            if ($result.stop_command_state -ne $wantedState) { throw 'control command returned before the expected service state' }
        } else {
            # Planned process-failure injection; the Windows VM keeps running.
            Stop-Process -Id $servicePid -Force -ErrorAction Stop
            (Get-Service OwnBackup).WaitForStatus('Stopped', [TimeSpan]::FromSeconds(30))
        }
        if (Get-Process -Id $servicePid -ErrorAction SilentlyContinue) { throw 'old process is alive' }
        $result.marker_after = Test-Path -LiteralPath $marker.FullName
        $result.observed_stopped = (Get-Service OwnBackup).Status -eq 'Stopped'
        Say "H02-CONSOL-STOPPED phase=$phase marker_present=$($result.marker_after) at=$([DateTime]::UtcNow.ToString('o'))"
        if ($phase -eq 'stop') {
            if ($result.marker_after) { throw 'graceful stop left a pending marker' }
            $jobsPath = 'C:\ProgramData\OwnBackup\jobs.json'
            $jobs = Get-Content -LiteralPath $jobsPath -Raw | ConvertFrom-Json
            $matchedJobs = @($jobs | Where-Object { $_.id -eq $arm.job_id })
            if ($matchedJobs.Count -ne 1) { throw 'exactly one saved job expected' }
            $job = $matchedJobs[0]
            Say "H02-CONSOL-SAVED job_id=$($job.id) status=$($job.last_run.status) folded=$($job.last_run.consolidated_increments)"
            if ($job.last_run.status -ne 'success' -or $job.last_run.consolidated_increments -ne 1) {
                throw 'graceful stop did not save completed consolidation history'
            }
            $result.last_run = $job.last_run
            $stopLine = Get-Content -LiteralPath $log | Select-Object -Skip $linesBefore |
                Where-Object { $_ -match 'Service stopped$' } | Select-Object -Last 1
            if (-not $stopLine) { throw 'Stopped absent from service log' }
            $stoppedAt = [DateTimeOffset]::Parse(($stopLine -split '\s+')[0]).UtcDateTime
            $newLines = @(Get-Content -LiteralPath $log | Select-Object -Skip $linesBefore)
            $requestedLine = $newLines | Where-Object { $_ -match 'Service stop requested$' } | Select-Object -Last 1
            $completedLine = $newLines | Where-Object { $_ -match 'Consolidation complete ' } | Select-Object -Last 1
            if (-not $requestedLine -or -not $completedLine) { throw 'stop/fold ordering absent from log' }
            $requestedAt = [DateTimeOffset]::Parse(($requestedLine -split '\s+')[0]).UtcDateTime
            $completedAt = [DateTimeOffset]::Parse(($completedLine -split '\s+')[0]).UtcDateTime
            if ($requestedAt -ge $completedAt -or $completedAt -ge $stoppedAt) {
                throw 'stop did not overlap the active fold'
            }
            if ($result.stop_wait_ms -le 6000) { throw 'stop did not exercise a drain longer than six seconds' }
            $result.stop_requested_at = $requestedAt.ToString('o')
            $result.fold_completed_at = $completedAt.ToString('o')
            $result.stopped_at = $stoppedAt.ToString('o')
            $result.history_saved_before_stopped = (Get-Item -LiteralPath $jobsPath).LastWriteTimeUtc -le $stoppedAt
            if (-not $result.history_saved_before_stopped) { throw 'history was written after Stopped' }
            Say "H02-CONSOL-HISTORY phase=stop status=success folded=1 saved_before_stopped=True run_id=$($job.last_run.id)"
        } else {
            if (-not $result.marker_after) { throw 'forced termination did not leave a recovery marker' }
            $result.marker_after_record = Get-Content -LiteralPath $marker.FullName -Raw | ConvertFrom-Json
            if ($result.marker_after_record.merged) { throw 'forced termination missed the active fold' }
            Say "H02-CONSOL-MARKER phase=kill merged=False base=$($record.base) victim=$($record.victim) next=$($record.next)"
        }
        if ((Get-Service OwnBackup).Status -eq 'Stopped') { Start-Service OwnBackup -ErrorAction Stop }
        (Get-Service OwnBackup).WaitForStatus('Running', [TimeSpan]::FromSeconds(30))
        if ($action -eq 'update') {
            $want = (Get-FileHash -LiteralPath 'C:\Lab\own-backup-service-h02.exe' -Algorithm SHA256).Hash
            $have = (Get-FileHash -LiteralPath 'C:\Lab\own-backup-service.exe' -Algorithm SHA256).Hash
            if ($want -ne $have) { throw 'updated binary hash mismatch' }
            Say "H02-CONSOL-INSTALLED action=update sha256=$have"
        }
        $result.passed = $true
        Say "H02-CONSOL-RESTART phase=$phase state=Running"
    } catch {
        $result.error = $_.Exception.Message
        Say "H02-CONSOL-WATCH-FAIL phase=$phase line=$($_.InvocationInfo.ScriptLineNumber) message=$($result.error)"
    } finally {
        foreach ($line in @(Get-Content -LiteralPath $log -ErrorAction SilentlyContinue |
            Select-Object -Skip $linesBefore)) { Say "H02-CONSOL-SERVICE-LOG $line" }
        [IO.File]::WriteAllText($resultPath, ($result | ConvertTo-Json -Depth 12))
    }
    if (-not $result.passed) { break }
}
