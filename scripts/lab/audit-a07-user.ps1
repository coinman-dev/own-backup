function Say([string]$Text) {
    try { $p=New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One'; $p.Open(); $p.WriteLine($Text); $p.Close() } catch { }
}
$ErrorActionPreference='Stop'
try {
    $identity=[Security.Principal.WindowsIdentity]::GetCurrent()
    $admin=[Security.Principal.WindowsPrincipal]::new($identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    Say "A07-USER name=$($identity.Name) session=$([Diagnostics.Process]::GetCurrentProcess().SessionId) admin=$admin"
    if ($admin) { throw 'requires standard console user' }
    $output=cmd.exe /d /c 'C:\Lab\audit_a07_cancel.exe 2>&1' | Out-String
    $code=$LASTEXITCODE; [IO.File]::WriteAllText('C:\Lab\a07\client.log',$output)
    foreach ($line in ($output -split "`r?`n")) { if ($line) { Say $line } }
    if ($code -ne 0) { throw "RPC cancellation cases failed exit=$code" }
    [IO.File]::WriteAllText('C:\Lab\a07\client.done','done')
} catch {
    $failure="A07-USER-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    Say $failure; [IO.File]::WriteAllText('C:\Lab\a07\failure.txt',$failure)
}
