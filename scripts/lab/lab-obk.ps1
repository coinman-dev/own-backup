# The encrypted backup path, against a real NTFS volume inside the laboratory
# VM. Everything here is disposable: two virtual disks created in the work
# folder, attached, used and detached again.
#
# What it proves that a cross-compiler cannot: that the Windows engine copies a
# live NTFS volume into an encrypted image, that the exclusion rules really
# removed a file, that the copy verifies with nothing but its own envelope and
# a password, and that a restore gives the files back byte for byte.

function Say {
    param([string]$Text)
    if ($null -eq $Text) { $Text = '' }
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open()
        $port.WriteLine($Text)
        $port.Close()
    } catch {
    }
}

function Diskpart {
    param([string]$Script)
    $file = [IO.Path]::GetTempFileName()
    Set-Content -LiteralPath $file -Value $Script -Encoding ASCII
    $out = (& diskpart.exe /s $file 2>&1 | Out-String)
    Remove-Item -LiteralPath $file -Force -ErrorAction SilentlyContinue
    return $out
}

$marker = 'own-backup-lab-marker--'
$work = 'C:\Lab\work'
$srcVhd = "$work\src.vhdx"
$tgtVhd = "$work\tgt.vhdx"
$probe = 'C:\Lab\obk_live_probe.exe'

try {
    Say "LAB-OBK-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    if (-not (Test-Path -LiteralPath $probe)) {
        Say 'LAB-OBK-SKIP no probe on the media'
        return
    }
    New-Item -ItemType Directory -Force -Path $work | Out-Null
    foreach ($f in @($srcVhd, $tgtVhd)) {
        if (Test-Path -LiteralPath $f) { Remove-Item -LiteralPath $f -Force }
    }

    # Two throwaway volumes of the same size: one to copy, one to restore onto.
    Diskpart @"
create vdisk file="$srcVhd" maximum=512 type=expandable
select vdisk file="$srcVhd"
attach vdisk
create partition primary
format fs=ntfs quick label="OBKSRC"
assign letter=S
"@ | Out-Null
    Diskpart @"
create vdisk file="$tgtVhd" maximum=512 type=expandable
select vdisk file="$tgtVhd"
attach vdisk
create partition primary
format fs=ntfs quick label="OBKTGT"
assign letter=R
"@ | Out-Null
    Say "LAB-OBK-VOLUMES source=$((Test-Path 'S:\')) target=$((Test-Path 'R:\'))"
    if (-not (Test-Path 'S:\') -or -not (Test-Path 'R:\')) {
        Say 'LAB-OBK-FAIL could not create the laboratory volumes'
        return
    }

    # Files to find again afterwards, one of them excluded by the job.
    New-Item -ItemType Directory -Force -Path 'S:\docs' | Out-Null
    $keep = @{
        'S:\keep-1.bin'        = 1
        'S:\docs\keep-2.bin'   = 2
    }
    foreach ($path in $keep.Keys) {
        $bytes = [byte[]]::new(3MB)
        (New-Object Random($keep[$path])).NextBytes($bytes)
        [IO.File]::WriteAllBytes($path, $bytes)
        # The marker goes in as text, so a grep of the image would find it if
        # anything were stored in the clear.
        Add-Content -LiteralPath $path -Value $marker -Encoding Ascii
    }
    [IO.File]::WriteAllText('S:\excluded-secret.bin', ($marker * 1000))
    $hashes = @{}
    foreach ($path in $keep.Keys) {
        $hashes[$path] = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
    }
    Say "LAB-OBK-SOURCE files=$($keep.Count + 1) excluded=excluded-secret.bin"

    # Windows refuses raw writes to sectors a mounted volume sits on, so a
    # restore takes the target disk down first — that is what the product's own
    # restore does with its dismount guard. Here the whole throwaway disk goes
    # offline, and the probe writes to the partition directly.
    $tgtDisk = Get-Disk | Where-Object { $_.Location -eq $tgtVhd } | Select-Object -First 1
    if (-not $tgtDisk) {
        Say 'LAB-OBK-FAIL cannot find the target virtual disk'
        return
    }
    $tgtPart = Get-Partition -DiskNumber $tgtDisk.Number |
        Where-Object { $_.Size -gt 100MB } |
        Select-Object -First 1
    $spec = "part:$($tgtDisk.Number):$($tgtPart.Offset):$($tgtPart.Size)"
    Say "LAB-OBK-TARGET $spec"
    Set-Disk -Number $tgtDisk.Number -IsOffline $true
    Start-Sleep -Seconds 2

    # The probe: copy, checks, restore.
    $out = & $probe 'S:' $work $spec 2>&1 | Out-String
    Say $out.Trim()

    Set-Disk -Number $tgtDisk.Number -IsOffline $false
    Start-Sleep -Seconds 3

    # The restored volume must be re-read by Windows before it can be compared.
    Diskpart @"
select vdisk file="$tgtVhd"
detach vdisk
attach vdisk
"@ | Out-Null
    Start-Sleep -Seconds 5
    $restored = Get-Volume -FileSystemLabel 'OBKSRC' -ErrorAction SilentlyContinue |
        Where-Object { $_.DriveLetter -and $_.DriveLetter -ne 'S' } |
        Select-Object -First 1
    if (-not $restored) {
        # The restored volume carries the SOURCE's label; if it did not get a
        # letter, give it one.
        Diskpart @"
select vdisk file="$tgtVhd"
select partition 1
assign letter=R
"@ | Out-Null
        Start-Sleep -Seconds 3
        $letter = 'R'
    } else {
        $letter = $restored.DriveLetter
    }
    Say "LAB-OBK-RESTORED-VOLUME letter=$letter label=$((Get-Volume -DriveLetter $letter -ErrorAction SilentlyContinue).FileSystemLabel)"

    foreach ($path in $keep.Keys) {
        $there = $path -replace '^S:', "${letter}:"
        if (-not (Test-Path -LiteralPath $there)) {
            Say "LAB-OBK-FAIL missing after restore: $there"
            continue
        }
        $now = (Get-FileHash -LiteralPath $there -Algorithm SHA256).Hash
        if ($now -eq $hashes[$path]) {
            Say "LAB-OBK-FILE-OK $there"
        } else {
            Say "LAB-OBK-FAIL content differs after restore: $there"
        }
    }
    # The rule said to exclude this file: its data was never copied, and its
    # catalog entry was deleted inside the image. It must not be here.
    $excludedThere = "${letter}:\excluded-secret.bin"
    $stillThere = Test-Path -LiteralPath $excludedThere
    Say "LAB-OBK-EXCLUDED-PRESENT $stillThere (expected False)"
    if ($stillThere) { Say 'LAB-OBK-FAIL the excluded file is still in the restored volume' }
    Say "LAB-OBK-DONE"
} catch {
    Say "LAB-OBK-CRASH $($_.Exception.Message)"
} finally {
    foreach ($f in @($srcVhd, $tgtVhd)) {
        if (Test-Path -LiteralPath $f) {
            Diskpart @"
select vdisk file="$f"
detach vdisk
"@ | Out-Null
        }
    }
}
