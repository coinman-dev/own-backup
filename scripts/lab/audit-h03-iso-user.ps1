function Say([string]$Text) {
    try { $p=New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One';$p.Open();$p.WriteLine($Text);$p.Close() } catch { }
}
$ErrorActionPreference='Stop'
try {
    $identity=[Security.Principal.WindowsIdentity]::GetCurrent()
    $admin=[Security.Principal.WindowsPrincipal]::new($identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if ($admin) { throw 'builder client must be ordinary user' }
    Say "H03-ISO-USER name=$($identity.Name) admin=$admin"
    $base='C:\Recovery\WindowsRE\Winre.wim'
    $before=Get-Content 'C:\Lab\iso-results\base-before.json' -Raw | ConvertFrom-Json
    $info=New-Object Diagnostics.ProcessStartInfo
    $info.FileName='C:\Lab\own-backup-cli.exe';$info.Arguments="media build --out C:\Lab\iso-results\own-backup.iso --base $base"
    $info.UseShellExecute=$false;$info.CreateNoWindow=$true;$info.RedirectStandardOutput=$true;$info.RedirectStandardError=$true
    $process=New-Object Diagnostics.Process;$process.StartInfo=$info
    if (-not $process.Start()) { throw 'CLI did not start' }
    $stdout=$process.StandardOutput.ReadToEndAsync();$stderr=$process.StandardError.ReadToEndAsync()
    if (-not $process.WaitForExit(1200000)) { throw 'build timeout' }
    $output=$stdout.GetAwaiter().GetResult()+$stderr.GetAwaiter().GetResult()
    [IO.File]::WriteAllText('C:\Lab\iso-results\build-cli.log',$output)
    if ($process.ExitCode -ne 0) { Say "H03-ISO-BUILD-DETAIL $output";throw "build CLI exit=$($process.ExitCode)" }
    $iso='C:\Lab\iso-results\own-backup.iso'
    $report=@{pass=$true;client_admin=$admin;base=$base;base_sha256=$before.sha256;iso_bytes=(Get-Item $iso).Length;iso_sha256=(Get-FileHash $iso -Algorithm SHA256).Hash;cli_exit=$process.ExitCode}
    [IO.File]::WriteAllText('C:\Lab\iso-results\build-result.json',($report | ConvertTo-Json))
    Say "H03-ISO-BUILD-PASS $($report | ConvertTo-Json -Compress)"
} catch {
    $failure="H03-ISO-USER-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    Say $failure;[IO.File]::WriteAllText('C:\Lab\iso-results\failure.txt',$failure)
} finally {
    [IO.File]::WriteAllText('C:\Lab\iso-results\client.done','done')
    Say 'H03-ISO-BUILD-DONE'
}
