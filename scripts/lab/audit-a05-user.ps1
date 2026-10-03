# Common Startup runs this under the console user and the actual RDP user.
function Say([string]$Text) {
    try {
        $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $p.Open(); $p.WriteLine($Text); $p.Close()
    } catch { }
}
function Probe([string]$Expected, [string]$Name) {
    $path = "C:\Lab\a05-results\$Name.json"
    $output = cmd.exe /d /c "C:\Lab\audit_a05_client.exe $Expected $path 2>&1" | Out-String
    $code = $LASTEXITCODE
    if (Test-Path $path) { Say "A05-PROBE name=$Name json=$(Get-Content $path -Raw)" }
    if ($code -ne 0) { throw "probe failed: $Name $output" }
    $report = Get-Content $path -Raw | ConvertFrom-Json
    if (-not $report.pass) { throw "incorrect result: $Name" }
    return $report
}
$ErrorActionPreference = 'Stop'
$session = [Diagnostics.Process]::GetCurrentProcess().SessionId
try {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $admin = [Security.Principal.WindowsPrincipal]::new($identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    Say "A05-USER name=$($identity.Name) sid=$($identity.User.Value) session=$session admin=$admin"
    if ($admin) { throw 'test must run as a standard user' }
    if ($identity.Name -like '*\AuditA05Rdp') {
        $report = Probe 'deny' 'rdp'
        if ($report.session -eq $report.active_console) { throw 'RDP test did not enter a different session' }
        [IO.File]::WriteAllText('C:\Lab\a05-results\rdp-transport.txt', $env:SESSIONNAME)
        Say "A05-RDP-RESULT session=$session active=$($report.active_console) transport=$env:SESSIONNAME"
        # SYSTEM watcher moves this actual RDP session onto the physical console.
        for ($tick = 0; $tick -lt 120; $tick++) {
            if (Test-Path 'C:\Lab\a05-results\console-transferred.ready') { break }
            Start-Sleep -Seconds 1
        }
        if (-not (Test-Path 'C:\Lab\a05-results\console-transferred.ready')) { throw 'console transfer timeout' }
        $report = Probe 'allow' 'console-after-switch'
        if ($report.session -ne $report.active_console) { throw 'transferred user is not the active console' }
        return
    }
    if ($identity.Name -notlike '*\lab') { throw 'unexpected console user' }
    $report = Probe 'allow' 'console'
    if ($report.session -ne $report.active_console) { throw 'initial client is outside active console' }
    $answer = New-Object System.Xml.XmlDocument
    $answer.Load('C:\Lab\autounattend.xml')
    $password = $answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    $same = [Management.Automation.PSCredential]::new($identity.Name, (ConvertTo-SecureString $password -AsPlainText -Force))
    $other = [Management.Automation.PSCredential]::new("$env:COMPUTERNAME\AuditA05Rdp", (ConvertTo-SecureString 'A05-Disposable-20261003!' -AsPlainText -Force))
    foreach ($case in @(@('runas-same', 'allow', $same), @('runas-other', 'deny', $other))) {
        $path = "C:\Lab\a05-results\$($case[0]).json"
        Start-Process -FilePath 'C:\Lab\audit_a05_client.exe' -Credential $case[2] -LoadUserProfile `
            -WorkingDirectory 'C:\Windows\System32' -ArgumentList @($case[1], $path) -WindowStyle Hidden
        # Start-Process -Wait can query a different user's process and fail with
        # access denied even after its successful RPC. Wait on the shared result.
        $r = $null
        for ($tick = 0; $tick -lt 60; $tick++) {
            if (Test-Path $path) {
                try { $r = Get-Content $path -Raw | ConvertFrom-Json } catch { }
                if ($r) { break }
            }
            Start-Sleep -Milliseconds 500
        }
        if (-not $r) { throw "RunAs result timeout: $($case[0])" }
        if (-not $r.pass -or $r.session -ne $session) { throw "RunAs identity/session result invalid: $($case[0])" }
        Say "A05-PROBE name=$($case[0]) json=$(Get-Content $path -Raw)"
    }
    [IO.File]::WriteAllText('C:\Lab\a05-results\console-ready.marker', 'ready')
    Say 'A05-CONSOLE-READY HOST-RDP-READY'
    # Windows client editions admit one active desktop. Finish the disposable
    # console logon before the standard RDP user signs in; keep the service running.
    shutdown.exe /l | Out-Null
} catch {
    $failure = "A05-USER-FAIL session=$session line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    Say $failure
    [IO.File]::WriteAllText("C:\Lab\a05-results\failure-$session.txt", $failure)
}
