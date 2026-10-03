# Ordinary console user: strict gRPC refusals, followed by real permitted writes.
function Say([string]$Text) {
    try { $p=New-Object System.IO.Ports.SerialPort 'COM1',115200,'None',8,'One';$p.Open();$p.WriteLine($Text);$p.Close() } catch { }
}
function Native([string]$Text) {
    $output=cmd.exe /d /c "$Text 2>&1" | Out-String
    if ($LASTEXITCODE -ne 0) { throw "native failed: $Text $output" }
}
function WaitMarker([string]$Name) {
    for ($tick=0;$tick -lt 240;$tick++) {
        if (Test-Path "C:\Lab\a06\$Name") { return }
        Start-Sleep -Seconds 1
    }
    throw "marker timeout: $Name"
}
$ErrorActionPreference='Stop'
try {
    $identity=[Security.Principal.WindowsIdentity]::GetCurrent()
    $admin=[Security.Principal.WindowsPrincipal]::new($identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    Say "A06-USER name=$($identity.Name) session=$([Diagnostics.Process]::GetCurrentProcess().SessionId) admin=$admin"
    if ($admin) { throw 'RPC probe must run as a standard console user' }
    if ((Get-Content 'C:\Lab\a06\phase.txt' -Raw).Trim() -eq 'reboot-proof') { Say 'A06-USER-REBOOT-PROOF';return }
    $phase=(Get-Content 'C:\Lab\a06\phase.txt' -Raw).Trim()
    if ($phase -eq 'rpc') {
        WaitMarker 'rpc-ready.marker'
        Native 'C:\Lab\audit_a06_guard.exe rpc C:\Lab\a06\rpc-results.json'
        Say 'A06-RPC-REFUSALS-PASS count=6'
        WaitMarker 'rpc-hashes-passed.marker'
    } else { WaitMarker 'positive-ready.marker' }
    $wanted=(Get-Content 'C:\Lab\a06\control.sha256' -Raw).Trim()
    foreach ($kind in @('volume','disk')) {
        $data=[IO.File]::ReadAllBytes('T:\a06-control.bin');$data[0]=$data[0] -bxor 0x5a;[IO.File]::WriteAllBytes('T:\a06-control.bin',$data)
        if ((Get-FileHash 'T:\a06-control.bin' -Algorithm SHA256).Hash -eq $wanted) { throw 'RPC target mutation failed' }
        Native "C:\Lab\audit_a06_guard.exe positive-$kind C:\Lab\a06\rpc-positive-$kind.json"
        WaitMarker "remounted-$kind.marker"
        $actual=(Get-FileHash 'T:\a06-control.bin' -Algorithm SHA256).Hash
        if ($actual -ne $wanted) { throw "RPC positive file differs: $kind" }
        [IO.File]::WriteAllText("C:\Lab\a06\rpc-file-$kind.json",(@{pass=$true;sha256=$actual}|ConvertTo-Json),([Text.UTF8Encoding]::new($false)))
        Say "A06-RPC-POSITIVE-PASS kind=$kind sha256=$actual"
    }
    [IO.File]::WriteAllText('C:\Lab\a06\rpc-all-done.marker','ready')
} catch {
    $text="A06-USER-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    Say $text
    [IO.File]::WriteAllText('C:\Lab\a06\user-failure.txt',$text)
}
