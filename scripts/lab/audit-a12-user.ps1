# Active-console RPC client; SYSTEM watcher changes SMB and R: availability.
function Say([string]$Text) {
    try { $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'; $p.Open(); $p.WriteLine($Text); $p.Close() } catch { }
}
function RunBackupCli([string[]]$Arguments) {
    $old = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    try {
        $output = (& 'C:\Lab\own-backup-cli.exe' @Arguments 2>&1 | Out-String).Trim()
        if ($LASTEXITCODE -ne 0) { throw "cli exit=$LASTEXITCODE output=$output" }
        return $output
    } finally { $ErrorActionPreference = $old }
}
function GetAuditJob([string]$Id) { return (RunBackupCli @('jobs', 'show', $Id)) | ConvertFrom-Json }
function AwaitRun([string]$Id, [string]$OldId) {
    $deadline=[DateTime]::UtcNow.AddMinutes(3)
    do {
        $job=GetAuditJob $Id
        if ($job.last_run -and $job.last_run.id -ne $OldId) {
            if ($job.last_run.status -ne 'success') { throw "job $Id failed: $($job.last_run.error)" }
            return $job.last_run
        }
        Start-Sleep -Seconds 2
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "job $Id timeout"
}
function SendControl([string]$Command) {
    $tcp=[Net.Sockets.TcpClient]::new('127.0.0.1',43995)
    try {
        $stream=$tcp.GetStream()
        $bytes=[Text.Encoding]::ASCII.GetBytes("$Command`n")
        $stream.Write($bytes,0,$bytes.Length)
        $reader=[IO.StreamReader]::new($stream,[Text.Encoding]::ASCII)
        $result=$reader.ReadLine()
        if ($result -ne 'OK') { throw "control $Command failed: $result" }
    } finally { $tcp.Dispose() }
}
function CreateAuditJob([string]$Name,[string]$Source,[string]$Dest) {
    $output=RunBackupCli @('jobs','new','--name',$Name,'--source',$Source,'--dest',$Dest,
        '--format','qcow2','--method','full','--schedule','event','--event','dest_connected',
        '--no-exclusions','--validate-after=false')
    $id=[regex]::Match($output,'[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}').Value
    if (-not $id) { throw "new job ID missing: $Name" }
    return $id
}
$ErrorActionPreference='Stop'
try {
    $deadline=[DateTime]::UtcNow.AddMinutes(2)
    while (-not (Test-Path 'C:\Lab\a12\watch.ready')) {
        if ([DateTime]::UtcNow -gt $deadline) { throw 'SYSTEM watcher not ready' }
        Start-Sleep -Seconds 1
    }
    $part=Get-Content 'C:\Lab\a12\parts.json' -Raw | ConvertFrom-Json
    $once=CreateAuditJob 'AuditA12Once' $part.source ("$($part.unc)\once")
    $repeat=CreateAuditJob 'AuditA12Repeat' $part.source ("$($part.unc)\repeat")
    $local=CreateAuditJob 'AuditA12Local' $part.source 'R:\a12-local'
    $null=RunBackupCli @('jobs','set',$repeat,'--schedule','event','--event','dest_connected')
    $repeatJob=GetAuditJob $repeat
    $repeatJob.schedule.event_once_per_day=$false
    $json='C:\Lab\a12\repeat.json'
    [IO.File]::WriteAllText($json,($repeatJob | ConvertTo-Json -Depth 30))
    $null=RunBackupCli @('jobs','save','--file',$json)
    [IO.File]::WriteAllText('C:\Lab\a12\jobs.json',(@{once=$once;repeat=$repeat;local=$local} | ConvertTo-Json))
    Say "A12-JOBS once=$once repeat=$repeat local=$local"
    # A full three minutes with both the share and R: absent. Probe/thread
    # counters are sampled by SYSTEM throughout this interval.
    Start-Sleep -Seconds 215
    if ((GetAuditJob $once).last_run -or (GetAuditJob $repeat).last_run -or (GetAuditJob $local).last_run) {
        throw 'an unavailable destination ran a job'
    }
    Say 'A12-OFFLINE-PASS seconds=215 runs=0'
    SendControl 'local_on'
    $localRun=AwaitRun $local ''
    if ((GetAuditJob $once).last_run -or (GetAuditJob $repeat).last_run) {
        throw 'network job ran while SMB server was offline'
    }
    $verified=RunBackupCli @('verify',[string]$localRun.image_path)
    if ($verified -notmatch 'Result: VALID') { throw 'local backup verification failed' }
    Say "A12-LOCAL-PASS image=$($localRun.image_path) status=$($localRun.status) smb_offline=True"
    SendControl 'share_on'
    $first=AwaitRun $once ''
    $firstRepeat=AwaitRun $repeat ''
    Say "A12-FIRST-CONNECT once=$($first.id) repeat=$($firstRepeat.id) status=success"
    SendControl 'share_off'
    Start-Sleep -Seconds 35
    SendControl 'share_on'
    $secondRepeat=AwaitRun $repeat $firstRepeat.id
    $sameOnce=(GetAuditJob $once).last_run.id -eq $first.id
    Say "A12-SECOND-CONNECT once_unchanged=$sameOnce repeat_new=$($secondRepeat.id -ne $firstRepeat.id)"
    if (-not $sameOnce -or $secondRepeat.id -eq $firstRepeat.id) { throw 'once-per-day / repeat policy failed' }
    $deadline=[DateTime]::UtcNow.AddMinutes(1)
    while (-not (Test-Path 'C:\Lab\a12\watch.pass')) {
        if ([DateTime]::UtcNow -gt $deadline) { throw 'SYSTEM watcher did not pass' }
        Start-Sleep -Seconds 1
    }
    Say 'A12-USER-PASS'
} catch { Say "A12-USER-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)" }
finally {
    $log='C:\Lab\Logs\own-backup-service-'+[DateTime]::UtcNow.ToString('yyyy-MM-dd')+'.log'
    foreach ($line in @(Get-Content -LiteralPath $log -ErrorAction SilentlyContinue)) {
        if ($line -match 'destination probe|destination drive|AuditA12|Scheduler: enqueued') { Say "A12-SERVICE-LOG $line" }
    }
    Say 'A12-USER-DONE'
    shutdown.exe /s /t 5 | Out-Null
}
