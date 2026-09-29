# Runs on the disposable lab desktop as the standard local user.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

$ErrorActionPreference = 'Stop'
try {
    Say "AUDIT29-GUI-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    $admin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    Say "AUDIT29-GUI-USER name=$($identity.Name) session=$([Diagnostics.Process]::GetCurrentProcess().SessionId) admin=$admin"
    if ($admin) { throw 'interactive user is an administrator' }
    if ((Get-CimInstance Win32_Service -Filter "Name='OwnBackup'").State -ne 'Running') {
        throw 'OwnBackup service is not running'
    }
    $marker = Join-Path ([IO.Path]::GetTempPath()) 'own-backup-gui-up'
    Remove-Item -LiteralPath $marker -Force -ErrorAction SilentlyContinue
    $gui = Start-Process -FilePath 'C:\Lab\own-backup-gui.exe' -PassThru
    for ($tick = 0; $tick -lt 60; $tick++) {
        $gui.Refresh()
        if ($gui.HasExited -or (Test-Path -LiteralPath $marker)) { break }
        Start-Sleep -Seconds 2
    }
    $gui.Refresh()
    $rendered = Test-Path -LiteralPath $marker
    Say "AUDIT29-GUI-WINDOW pid=$($gui.Id) exited=$($gui.HasExited) title=$($gui.MainWindowTitle) handle=$($gui.MainWindowHandle) rendered=$rendered"
    if ($gui.HasExited -or -not $rendered -or $gui.MainWindowTitle -ne 'Own Backup') {
        throw 'normal GUI did not render'
    }
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $status = (& 'C:\Lab\own-backup-cli.exe' jobs status 2>&1 | Out-String)
        $code = [int]$LASTEXITCODE
    } finally { $ErrorActionPreference = $old }
    $connected = $code -eq 0 -and $status.Contains('uptime_secs')
    Say "AUDIT29-GUI-PIPE exit=$code connected=$connected"
    if (-not $connected) { throw 'standard user cannot reach service' }
    Say 'AUDIT29-GUI-PASS'
} catch {
    Say "AUDIT29-GUI-FAIL id=$($_.FullyQualifiedErrorId) line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    Say 'AUDIT29-GUI-DONE'
}
