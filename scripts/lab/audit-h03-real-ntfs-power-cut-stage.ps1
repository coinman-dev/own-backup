# Tear the active header of a real encrypted NTFS disk archive, cut QEMU
# power, then verify and restore that same archive on the next boot.
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

function VolumeForVhd([string]$Vhd, [char]$Letter) {
    $disk = Get-Disk | Where-Object { $_.Location -eq $Vhd } | Select-Object -First 1
    if (-not $disk) { throw "VHDX disk not found: $Vhd" }
    $part = Get-Partition -DiskNumber $disk.Number |
        Where-Object { $_.Size -gt 100MB } | Select-Object -First 1
    if (-not $part) { throw "NTFS partition not found: $Vhd" }
    $vol = Get-Volume -Partition $part
    if (-not $vol -or $vol.FileSystemLabel -ne 'OBKSRC') { throw "OBKSRC volume missing: $Vhd" }
    if (-not $vol.DriveLetter) {
        Set-Partition -DiskNumber $disk.Number -PartitionNumber $part.PartitionNumber -NewDriveLetter $Letter
        $vol = Get-Volume -Partition $part
    }
    return $vol
}

$ErrorActionPreference = 'Stop'
$work = 'C:\Lab\work'
$image = Join-Path $work 'lab_disk0_full_1.obk'
$marker = Join-Path $work 'h03-real-torn.ready'
$sourceVhd = Join-Path $work 'src.vhdx'
$targetVhd = Join-Path $work 'tgt.vhdx'
$probe = 'C:\Lab\obk_live_probe.exe'
$waitingForHost = $false
try {
    Say "H03-REAL-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    Say "H03-REAL-PROBE sha256=$((Get-FileHash -LiteralPath $probe -Algorithm SHA256).Hash)"
    if (-not (Test-Path -LiteralPath $image)) { throw 'real .obk archive missing' }
    if (-not (Test-Path -LiteralPath $marker)) {
        $process = Start-Process -FilePath $probe -ArgumentList @('h03-tear', $image, $marker) -PassThru -WindowStyle Hidden
        for ($tick = 0; $tick -lt 600; $tick++) {
            if (Test-Path -LiteralPath $marker) {
                Say 'H03-REAL-CUT-WINDOW active_header_torn=true'
                $waitingForHost = $true
                while ($true) { Start-Sleep -Seconds 1 }
            }
            if ($process.HasExited) { throw "tear probe exited=$($process.ExitCode)" }
            Start-Sleep -Milliseconds 250
        }
        throw 'tear marker timeout'
    }

    Say 'H03-REAL-RECOVERY-BOOT'
    Diskpart "select vdisk file=`"$targetVhd`"`nattach vdisk`n"
    Start-Sleep -Seconds 3
    $targetDisk = Get-Disk | Where-Object { $_.Location -eq $targetVhd } | Select-Object -First 1
    if (-not $targetDisk) { throw 'target disk missing' }
    Set-Disk -Number $targetDisk.Number -IsOffline $true
    $output = cmd.exe /d /c "`"$probe`" h03-restore `"$image`" $($targetDisk.Number) 2>&1" | Out-String
    foreach ($line in ($output -split "`r?`n")) {
        if ($line -match 'PROBE-H03|error|FAIL') { Say "H03-REAL-PROBE-LINE $line" }
    }
    if ($LASTEXITCODE -ne 0) { throw "restore probe exit=$LASTEXITCODE" }
    Set-Disk -Number $targetDisk.Number -IsOffline $false
    Diskpart "select vdisk file=`"$targetVhd`"`ndetach vdisk`nattach vdisk`n"
    Diskpart "select vdisk file=`"$sourceVhd`"`nattach vdisk`n"
    Start-Sleep -Seconds 5
    $source = VolumeForVhd $sourceVhd 'S'
    $target = VolumeForVhd $targetVhd 'R'
    $matched = 0
    foreach ($relative in @('keep-1.bin', 'docs\keep-2.bin', 'gen-1.bin', 'gen-2.bin')) {
        $src = "$($source.DriveLetter):\$relative"
        $dst = "$($target.DriveLetter):\$relative"
        if ((Get-FileHash -LiteralPath $src -Algorithm SHA256).Hash -ne
            (Get-FileHash -LiteralPath $dst -Algorithm SHA256).Hash) {
            throw "restored file differs: $relative"
        }
        $matched++
    }
    if (Test-Path -LiteralPath "$($target.DriveLetter):\excluded-secret.bin") {
        throw 'excluded file returned after restore'
    }
    Say "H03-REAL-FILES matched=$matched excluded_absent=true"

    $write = "$($target.DriveLetter):\h03-after-cut-write.txt"
    [IO.File]::WriteAllText($write, 'written after H03 recovery')
    if ([IO.File]::ReadAllText($write) -ne 'written after H03 recovery') {
        throw 'restored NTFS volume rejected the write'
    }
    Say 'H03-REAL-PASS ntfs_read=true ntfs_write=true'
} catch {
    Say "H03-REAL-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    if (-not $waitingForHost) {
        foreach ($vhd in @($sourceVhd, $targetVhd)) {
            try { Diskpart "select vdisk file=`"$vhd`"`ndetach vdisk`n" } catch { }
        }
        shutdown.exe /s /t 5 | Out-Null
    }
}
