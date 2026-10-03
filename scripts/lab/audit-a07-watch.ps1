function Say([string]$Text) {
    try { $p=New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One'; $p.Open(); $p.WriteLine($Text); $p.Close() } catch { }
}
$ErrorActionPreference='Stop'
try {
    $root='C:\Lab\a07'
    for ($n=0; $n -lt 600; $n++) {
        if (Test-Path "$root\failure.txt") { throw (Get-Content "$root\failure.txt" -Raw) }
        if (Test-Path "$root\client.done") { break }
        Start-Sleep -Seconds 1
    }
    if (-not (Test-Path "$root\client.done")) { throw 'client timeout' }
    $results=Get-Content "$root\results.json" -Raw | ConvertFrom-Json
    if (-not $results.pass -or $results.cases.Count -ne 4) { throw 'four RPC cases required' }
    $before=@(Get-ChildItem $root -Recurse -File | Where-Object { $_.Extension -in @('.qcow2','.json') -and $_.Name -ne 'results.json' } | ForEach-Object { @{path=$_.FullName; bytes=$_.Length; sha256=(Get-FileHash $_.FullName -Algorithm SHA256).Hash} })
    Start-Sleep -Seconds 30
    foreach ($record in $before) {
        if ((Get-Item $record.path).Length -ne $record.bytes -or (Get-FileHash $record.path -Algorithm SHA256).Hash -ne $record.sha256) { throw "residual image/sidecar writes: $($record.path)" }
    }
    [IO.File]::WriteAllText("$root\stable-after-30s.json", ($before | ConvertTo-Json -Depth 6))
    $output=cmd.exe /d /c 'C:\Lab\audit_a07_cancel.exe check-images 2>&1' | Out-String
    $code=$LASTEXITCODE; [IO.File]::WriteAllText("$root\images-checked.log",$output)
    foreach ($line in ($output -split "`r?`n")) { if ($line) { Say $line } }
    if ($code -ne 0) { throw 'independent full image readback failed' }
    $fixture=Get-Content "$root\fixture.json" -Raw | ConvertFrom-Json
    if ((Get-FileHash 'E:\control.bin' -Algorithm SHA256).Hash -ne $fixture.control_sha256) { throw 'source control file changed' }
    Stop-Service OwnBackup
    Copy-Item 'C:\ProgramData\OwnBackup\jobs.json' "$root\persisted-jobs.json"
    Copy-Item 'C:\ProgramData\OwnBackup\activity.json' "$root\persisted-activity.json"
    $jobs=Get-Content "$root\persisted-jobs.json" -Raw | ConvertFrom-Json
    $history=Get-Content "$root\persisted-activity.json" -Raw | ConvertFrom-Json
    foreach ($case in $results.cases) {
        $job=@($jobs | Where-Object { $_.id -eq $case.job })
        $run=@($history | Where-Object { $_.id -eq $case.run.id })
        if ($job.Count -ne 1 -or $run.Count -ne 1 -or $job[0].last_run.status -ne 'cancelled' -or $run[0].status -ne 'cancelled') { throw 'persisted history disagrees' }
    }
    Start-Service OwnBackup
    Say 'A07-PASS cases=4 scanner_reads_after_cancel=0 stable_30s=true images_full_readback=4 persisted_history=true'
} catch {
    Say "A07-WATCH-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally { Say 'A07-DONE'; shutdown.exe /s /t 5 | Out-Null }
