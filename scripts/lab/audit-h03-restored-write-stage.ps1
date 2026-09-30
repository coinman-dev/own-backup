# Verify that the NTFS volume restored from a dual-header .obk remains writable
# across detaching and reattaching its disposable target VHDX.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

function Diskpart([string]$Body) {
    $script = [IO.Path]::GetTempFileName()
    try {
        [IO.File]::WriteAllText($script, $Body, [Text.Encoding]::ASCII)
        $output = (cmd.exe /d /c "diskpart.exe /s `"$script`" 2>&1" | Out-String).Trim()
        if ($LASTEXITCODE -ne 0) { throw "diskpart exit=$LASTEXITCODE output=$output" }
    } finally {
        Remove-Item -LiteralPath $script -Force -ErrorAction SilentlyContinue
    }
}

function RestoredVolume([string]$Vhd) {
    $disk = Get-Disk | Where-Object { $_.Location -eq $Vhd } | Select-Object -First 1
    if (-not $disk) { throw 'target VHDX disk not found' }
    $partition = Get-Partition -DiskNumber $disk.Number |
        Where-Object { $_.Size -gt 100MB } | Select-Object -First 1
    if (-not $partition) { throw 'restored NTFS partition not found' }
    $volume = Get-Volume -Partition $partition
    if (-not $volume -or $volume.FileSystemLabel -ne 'OBKSRC') {
        throw 'restored volume label is not OBKSRC'
    }
    if (-not $volume.DriveLetter) {
        Set-Partition -DiskNumber $disk.Number -PartitionNumber $partition.PartitionNumber -NewDriveLetter R
        $volume = Get-Volume -Partition $partition
    }
    return $volume
}

$ErrorActionPreference = 'Stop'
$vhd = 'C:\Lab\work\tgt.vhdx'
try {
    Say "H03-V2-RW-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    if (-not (Test-Path -LiteralPath $vhd)) { throw 'restored target VHDX missing' }
    Diskpart "select vdisk file=`"$vhd`"`nattach vdisk`n"
    Start-Sleep -Seconds 3
    $volume = RestoredVolume $vhd
    $file = "$($volume.DriveLetter):\h03-write-after-restore.bin"
    $bytes = [byte[]]::new(1MB)
    (New-Object Random(303)).NextBytes($bytes)
    $stream = [IO.File]::Open($file, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write)
    try { $stream.Write($bytes, 0, $bytes.Length); $stream.Flush($true) }
    finally { $stream.Dispose() }
    $hash = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash
    Say "H03-V2-RW-WRITTEN bytes=$($bytes.Length) sha256=$hash"

    Diskpart "select vdisk file=`"$vhd`"`ndetach vdisk`n"
    Diskpart "select vdisk file=`"$vhd`"`nattach vdisk`n"
    Start-Sleep -Seconds 3
    $volume = RestoredVolume $vhd
    $again = "$($volume.DriveLetter):\h03-write-after-restore.bin"
    if ((Get-FileHash -LiteralPath $again -Algorithm SHA256).Hash -ne $hash) {
        throw 'written file changed after VHDX reattach'
    }
    Say 'H03-V2-RW-PASS filesystem=NTFS persisted=true'
} catch {
    Say "H03-V2-RW-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    try { Diskpart "select vdisk file=`"$vhd`"`ndetach vdisk`n" } catch { }
    shutdown.exe /s /t 5 | Out-Null
}
