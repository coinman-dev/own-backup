function Say([string]$Text) {
    try { $p=New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One';$p.Open();$p.WriteLine($Text);$p.Close() } catch { }
}
$ErrorActionPreference='Stop'
try {
    for ($n=0;$n -lt 1500;$n++) {
        if (Test-Path 'C:\Lab\iso-results\failure.txt') {throw 'client failed; see its saved log'}
        if (Test-Path 'C:\Lab\iso-results\client.done') {break}
        Start-Sleep -Seconds 1
    }
    if (-not (Test-Path 'C:\Lab\iso-results\client.done')) {throw 'client timeout'}
    if (Test-Path 'C:\Lab\iso-results\failure.txt') {throw 'client failed; see its saved log'}
    $before=Get-Content 'C:\Lab\iso-results\base-before.json' -Raw | ConvertFrom-Json
    if ((Get-FileHash $before.base -Algorithm SHA256).Hash -ne $before.sha256) {throw 'base WinRE changed'}
    if (-not (Test-Path 'C:\Lab\iso-results\build-result.json')) {throw 'build result missing'}
    Say 'H03-ISO-BASE-PRESERVED'
} catch {Say "H03-ISO-WATCH-FAIL message=$($_.Exception.Message)"}
finally {shutdown.exe /s /t 5 | Out-Null}
