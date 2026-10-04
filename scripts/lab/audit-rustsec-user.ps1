function Say([string]$Text) {
    try { $p=New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One'; $p.Open(); $p.WriteLine($Text); $p.Close() } catch { }
}
$ErrorActionPreference='Stop'
try {
    $identity=[Security.Principal.WindowsIdentity]::GetCurrent()
    $admin=[Security.Principal.WindowsPrincipal]::new($identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if ($admin) { throw 'GUI must run as standard user' }
    $marker=Join-Path ([IO.Path]::GetTempPath()) 'own-backup-gui-up'
    Remove-Item $marker -Force -ErrorAction SilentlyContinue
    Add-Type -AssemblyName UIAutomationClient
    $missing=Start-Process 'C:\Lab\own-backup-gui.exe' -PassThru
    $null=$missing.Handle
    for ($n=0; $n -lt 60; $n++) {
        $missing.Refresh()
        if ($missing.HasExited -or $missing.MainWindowHandle -ne [IntPtr]::Zero) { break }
        Start-Sleep -Seconds 1
    }
    if ($missing.HasExited -or (Test-Path $marker)) { throw 'missing-runtime diagnosis did not open' }
    $root=[Windows.Automation.AutomationElement]::FromHandle($missing.MainWindowHandle)
    $elements=$root.FindAll([Windows.Automation.TreeScope]::Descendants,[Windows.Automation.Condition]::TrueCondition)
    $text=(@($elements | ForEach-Object { $_.Current.Name }) -join "`n")
    if ($text -notmatch 'WebView2' -or $text -notmatch 'msedgewebview2.exe') { throw 'native diagnosis lacks searched paths' }
    [IO.File]::WriteAllText('C:\Lab\rustsec-results\missing-dialog.txt',$text)
    Say 'RUSTSEC-MISSING-DIALOG-READY'
    Start-Sleep -Seconds 5
    if (-not $missing.CloseMainWindow()) { throw 'native startup dialog did not accept close' }
    if (-not $missing.WaitForExit(30000) -or $missing.ExitCode -ne 1) { throw 'missing-runtime GUI did not exit 1 cleanly' }
    $log='C:\Lab\Logs\own-backup-gui-'+[DateTime]::UtcNow.ToString('yyyy-MM-dd')+'.log'
    if (Test-Path $log) {
        if ((Get-Content $log -Raw) -match 'PANIC|re-entrant') { throw 'startup dialog re-entered the event loop' }
    }
    [IO.File]::WriteAllText('C:\Lab\rustsec-results\missing.done','exit=1 no panic')
    Say 'RUSTSEC-MISSING-PASS exit=1 panic=false'
    for ($n=0; $n -lt 180; $n++) {
        if (Test-Path 'C:\Lab\rustsec-results\runtime.ready') { break }
        if (Test-Path 'C:\Lab\rustsec-results\failure.txt') { throw 'runtime installation failed' }
        Start-Sleep -Seconds 1
    }
    if (-not (Test-Path 'C:\Lab\rustsec-results\runtime.ready')) { throw 'runtime installation timeout' }
    $gui=Start-Process 'C:\Lab\own-backup-gui.exe' -PassThru
    for ($n=0; $n -lt 120; $n++) {
        $gui.Refresh()
        if ($gui.HasExited -or (Test-Path $marker)) { break }
        Start-Sleep -Seconds 1
    }
    $gui.Refresh()
    if ($gui.HasExited -or -not (Test-Path $marker) -or $gui.MainWindowTitle -ne 'Own Backup') { throw 'updated GUI did not render' }
    $status=cmd.exe /d /c 'C:\Lab\own-backup-cli.exe jobs status 2>&1' | Out-String
    $code=$LASTEXITCODE
    [IO.File]::WriteAllText('C:\Lab\rustsec-results\status.json',$status)
    if ($code -ne 0 -or -not $status.Contains('uptime_secs')) { throw 'standard-user pipe RPC failed' }
    $report=@{pass=$true; identity=$identity.Name; admin=$admin; session=[Diagnostics.Process]::GetCurrentProcess().SessionId; gui_pid=$gui.Id; title=$gui.MainWindowTitle; rendered=$true; rpc_exit=$code; gui_sha256=(Get-FileHash 'C:\Lab\own-backup-gui.exe' -Algorithm SHA256).Hash}
    [IO.File]::WriteAllText('C:\Lab\rustsec-results\gui-result.json',($report | ConvertTo-Json))
    Say "RUSTSEC-GUI-READY $($report | ConvertTo-Json -Compress)"
    # The host captures the actual desktop while the updated WebView stays alive.
    Start-Sleep -Seconds 30
    if ($gui.HasExited) { throw 'GUI exited after rendering' }
    Say 'RUSTSEC-GUI-PASS rendered=true standard_user=true pipe_rpc=true'
    $null=$gui.CloseMainWindow()
} catch {
    $failure="RUSTSEC-USER-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    Say $failure; [IO.File]::WriteAllText('C:\Lab\rustsec-results\failure.txt',$failure)
} finally { Say 'RUSTSEC-GUI-DONE'; shutdown.exe /s /t 5 | Out-Null }
