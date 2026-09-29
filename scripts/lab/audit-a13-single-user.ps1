function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

$ErrorActionPreference = 'Stop'
try {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    $admin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    Say "A13-SINGLE-USER name=$($identity.Name) session=$([Diagnostics.Process]::GetCurrentProcess().SessionId) admin=$admin"
    if ($admin) { throw 'lab is still an administrator' }
    $marker = Join-Path ([IO.Path]::GetTempPath()) 'own-backup-gui-up'
    Remove-Item -LiteralPath $marker -Force -ErrorAction SilentlyContinue
    $first = Start-Process -FilePath 'C:\Lab\own-backup-gui.exe' -PassThru
    for ($tick = 0; $tick -lt 60; $tick++) {
        $first.Refresh()
        if ($first.HasExited -or (Test-Path -LiteralPath $marker)) { break }
        Start-Sleep -Seconds 2
    }
    $first.Refresh()
    Say "A13-SINGLE-FIRST pid=$($first.Id) exited=$($first.HasExited) title=$($first.MainWindowTitle) rendered=$(Test-Path -LiteralPath $marker)"
    if ($first.HasExited -or $first.MainWindowTitle -ne 'Own Backup' -or -not (Test-Path -LiteralPath $marker)) {
        throw 'first GUI did not render'
    }
    $second = Start-Process -FilePath 'C:\Lab\own-backup-gui.exe' -PassThru
    Start-Sleep -Seconds 5
    $second.Refresh()
    Say "A13-SINGLE-SECOND pid=$($second.Id) exited=$($second.HasExited) title=$($second.MainWindowTitle) handle=$($second.MainWindowHandle)"
    Say 'A13-SINGLE-DIALOG SCREENSHOT-AND-DISMISS'
    $closed = $second.WaitForExit(120000)
    $second.Refresh()
    $first.Refresh()
    Say "A13-SINGLE-AFTER closed=$closed second_exit=$($second.ExitCode) first_running=$(-not $first.HasExited)"
    if (-not $closed -or $second.ExitCode -ne 0 -or $first.HasExited) {
        throw 'second GUI was not refused cleanly'
    }
    Say 'A13-SINGLE-PASS'
} catch {
    Say "A13-SINGLE-FAIL id=$($_.FullyQualifiedErrorId) line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    Say 'A13-SINGLE-DONE'
    shutdown.exe /s /t 5 | Out-Null
}
