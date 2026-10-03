# SYSTEM: inspect final native results, transfer the actual RDP session, cleanly stop.
function Say([string]$Text) {
    try {
        $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $p.Open(); $p.WriteLine($Text); $p.Close()
    } catch { }
}
$ErrorActionPreference = 'Stop'
try {
    $root = 'C:\Lab\a05-results'
    $transferred = $false
    for ($tick = 0; $tick -lt 600; $tick++) {
        if (@(Get-ChildItem "$root\failure-*.txt" -ErrorAction SilentlyContinue).Count) { throw 'user probe reported failure' }
        if (-not $transferred -and (Test-Path "$root\rdp.json")) {
            $rdp = Get-Content "$root\rdp.json" -Raw | ConvertFrom-Json
            if (-not $rdp.pass -or $rdp.rpc_allowed -or $rdp.session -eq $rdp.active_console) { throw 'RDP denial/session invalid' }
            Say "A05-RDP-TRANSFER session=$($rdp.session) previous_console=$($rdp.active_console)"
            $output = & tscon.exe $rdp.session /dest:console 2>&1 | Out-String
            if ($LASTEXITCODE -ne 0) { throw "tscon failed: $output" }
            Start-Sleep -Seconds 3
            [IO.File]::WriteAllText("$root\console-transferred.ready", 'ready')
            $transferred = $true
        }
        if ($transferred -and (Test-Path "$root\console-after-switch.json")) { break }
        Start-Sleep -Seconds 1
    }
    $cases = @('console', 'runas-same', 'runas-other', 'rdp', 'console-after-switch')
    $reports = @{}
    foreach ($case in $cases) {
        $path = "$root\$case.json"
        if (-not (Test-Path $path)) { throw "missing result: $case" }
        $r = Get-Content $path -Raw | ConvertFrom-Json
        if (-not $r.pass -or $r.rpc_timeout -or -not $r.token_read) { throw "case not passed: $case" }
        $reports[$case] = $r
        Say "A05-RESULT name=$case json=$((Get-Content $path -Raw) -replace '\s+', ' ')"
    }
    if ($reports['runas-same'].identity -ne $reports['console'].identity) { throw 'same-user RunAs changed principal' }
    if ($reports['runas-other'].identity -eq $reports['console'].identity) { throw 'other-user RunAs did not change principal' }
    if ($reports['rdp'].identity -ne $reports['console-after-switch'].identity) { throw 'RDP-to-console changed principal' }
    $acl = Get-Acl 'C:\ProgramData\OwnBackup\service.token'
    if (-not $acl.AreAccessRulesProtected) { throw 'installed token lost protected DACL' }
    Say "A05-INSTALLED-TOKEN protected=$($acl.AreAccessRulesProtected) bytes=$((Get-Item 'C:\ProgramData\OwnBackup\service.token').Length)"
    $log = Get-ChildItem 'C:\Lab\Logs\own-backup-service-*.log' | Sort-Object LastWriteTime | Select-Object -Last 1
    if ($log) {
        $lines = @(Get-Content $log.FullName | Where-Object { $_ -like '*named-pipe client outside the active console session rejected*' })
        Say "A05-SERVICE-REJECTIONS count=$($lines.Count)"
        foreach ($line in ($lines | Select-Object -Last 8)) { Say "A05-SERVICE $line" }
    }
    Say 'A05-PASS native_token_creation=true runas_same=true runas_other_refused=true rdp_refused=true transferred_console_allowed=true'
} catch {
    Say "A05-WATCH-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    foreach ($p in (Get-ChildItem 'C:\Lab\a05-results\failure-*.txt' -ErrorAction SilentlyContinue)) { Say (Get-Content $p.FullName -Raw) }
} finally {
    Say 'A05-DONE'
    shutdown.exe /s /t 5 | Out-Null
}
