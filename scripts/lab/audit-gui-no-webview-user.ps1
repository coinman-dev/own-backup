function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

try {
    Say "AUDIT29-NOWEBVIEW-GUI-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    $gui = Start-Process -FilePath 'C:\Lab\own-backup-gui.exe' -PassThru
    Start-Sleep -Seconds 15
    $gui.Refresh()
    Say "AUDIT29-NOWEBVIEW-GUI-WINDOW pid=$($gui.Id) exited=$($gui.HasExited) title=$($gui.MainWindowTitle) handle=$($gui.MainWindowHandle)"
    $closed = $gui.WaitForExit(120000)
    $gui.Refresh()
    if ($closed) {
        Say "AUDIT29-NOWEBVIEW-GUI-EXIT closed=True code=$($gui.ExitCode)"
    } else {
        Say 'AUDIT29-NOWEBVIEW-GUI-EXIT closed=False'
    }
} catch {
    Say "AUDIT29-NOWEBVIEW-GUI-FAIL $($_.Exception.Message)"
}
