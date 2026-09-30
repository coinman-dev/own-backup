# Run the real encrypted NTFS full/increment/consolidation/restore probe in a
# disposable Windows VM. lab-obk.ps1 writes detailed evidence to COM1.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

$ErrorActionPreference = 'Stop'
try {
    $probe = 'C:\Lab\obk_live_probe.exe'
    Say "H03-V2-FS-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    Say "H03-V2-FS-PROBE sha256=$((Get-FileHash -LiteralPath $probe -Algorithm SHA256).Hash)"
    & 'C:\Lab\lab-obk.ps1'
    Say 'H03-V2-FS-SCRIPT-DONE'
} catch {
    Say "H03-V2-FS-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    shutdown.exe /s /t 5 | Out-Null
}
