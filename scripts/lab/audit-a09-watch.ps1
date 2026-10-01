function Say([string]$Text) {
    try {
        $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $p.Open(); $p.WriteLine($Text); $p.Close()
    } catch { }
}
$ErrorActionPreference = 'Stop'
$listener = $null
$client = $null
$writer = $null
try {
    $listener = New-Object System.Net.Sockets.TcpListener ([System.Net.IPAddress]::Loopback), 43996
    $listener.Start()
    Say 'A09-WATCH-READY port=43996'
    $client = $listener.AcceptTcpClient()
    $stream = $client.GetStream()
    $reader = [IO.StreamReader]::new($stream, [Text.Encoding]::ASCII)
    $writer = [IO.StreamWriter]::new($stream, [Text.Encoding]::ASCII)
    $writer.NewLine = "`n"
    $writer.AutoFlush = $true
    if ($reader.ReadLine() -ne 'reparse') { throw 'unknown mutation request' }
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    $item = $null
    do {
        $part = Get-Partition -DiskNumber 2 -PartitionNumber 1 -ErrorAction SilentlyContinue
        if ($part -and $part.DriveLetter) {
            $root = '{0}:\a07-data' -f $part.DriveLetter
            $link = "$root\a09-link"
            $item = Get-Item -LiteralPath $link -Force -ErrorAction SilentlyContinue
        }
        if ($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { break }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    if (-not $item -or -not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'restored link was not ready' }
    $before = [string]$item.Target
    Remove-Item -LiteralPath $link -Force
    New-Item -ItemType SymbolicLink -Path $link -Target "$root\kernel32.dll" | Out-Null
    $after = [string](Get-Item -LiteralPath $link -Force).Target
    if ($before -eq $after) { throw 'reparse target did not change' }
    $writer.WriteLine('OK')
    $when = [DateTime]::UtcNow.ToString('o')
    [IO.File]::WriteAllText('C:\Lab\a09-watch.log', "PASS at=$when letter=$($part.DriveLetter) before=$before after=$after")
    Say "A09-WATCH-PASS at=$when letter=$($part.DriveLetter) before=$before after=$after"
} catch {
    $message = $_.Exception.Message
    [IO.File]::WriteAllText('C:\Lab\a09-watch.log', "FAIL $message")
    if ($writer) { try { $writer.WriteLine("ERR $message") } catch { } }
    Say "A09-WATCH-FAIL message=$message"
} finally {
    if ($client) { $client.Dispose() }
    if ($listener) { $listener.Stop() }
}
