function Say([string]$Text) {
    try { $p=New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One'; $p.Open(); $p.WriteLine($Text); $p.Close() } catch { }
}
$ErrorActionPreference='Stop'
try {
    for ($n=0; $n -lt 180; $n++) {
        if (Test-Path 'C:\Lab\rustsec-results\missing.done') { break }
        if (Test-Path 'C:\Lab\rustsec-results\failure.txt') { throw 'missing-runtime check failed' }
        Start-Sleep -Seconds 1
    }
    if (-not (Test-Path 'C:\Lab\rustsec-results\missing.done')) { throw 'missing-runtime check timeout' }
    $installer='C:\Lab\WebView2Runtime-x64.exe'
    if ((Get-FileHash $installer -Algorithm SHA256).Hash -ne 'FA07BC67C2A68763E127F6AABF5622B40A0220228899E78DAF02CE2CC3971A50') { throw 'installer hash differs' }
    $signature=Get-AuthenticodeSignature $installer
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'Microsoft') { throw 'installer signature invalid' }
    $process=Start-Process $installer -ArgumentList '/silent /install' -PassThru -Wait
    if ($process.ExitCode -ne 0) { throw "installer exit=$($process.ExitCode)" }
    $reg='HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
    $version=(Get-ItemProperty $reg -Name pv).pv
    if (-not $version -or $version -eq '0.0.0.0') { throw 'runtime registry missing after install' }
    [IO.File]::WriteAllText('C:\Lab\rustsec-results\runtime.ready',$version)
    Say "RUSTSEC-RUNTIME-READY version=$version"
} catch {
    $failure="RUSTSEC-RUNTIME-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    Say $failure; [IO.File]::WriteAllText('C:\Lab\rustsec-results\failure.txt',$failure)
    shutdown.exe /s /t 5 | Out-Null
}
