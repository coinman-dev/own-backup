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
    # A previous boot's images are a previous boot's problem: left in place they
    # join this run's listings as extra chains of the same job.
    if (Test-Path -LiteralPath $work) { Remove-Item -LiteralPath $work -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $work | Out-Null
    foreach ($f in @($srcVhd, $tgtVhd)) {
        if (Test-Path -LiteralPath $f) { Remove-Item -LiteralPath $f -Force }
    }

    # Two throwaway volumes of the same size: one to copy, one to restore onto.
    # GPT, not the MBR diskpart gives a fresh VHD by default: a whole-disk copy
    # is a GPT feature (the manifest IS the partition table), and the engine
    # refuses an MBR disk by name.
    Diskpart @"
create vdisk file="$srcVhd" maximum=512 type=expandable
select vdisk file="$srcVhd"
attach vdisk
convert gpt
create partition primary
format fs=ntfs quick label="OBKSRC"
assign letter=S
"@ | Out-Null
    Diskpart @"
create vdisk file="$tgtVhd" maximum=512 type=expandable
select vdisk file="$tgtVhd"
attach vdisk
convert gpt
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

    # The probe: full copy, checks, two increments, consolidation, restore.
    $out = & $probe 'volume' 'S:' $work $spec 2>&1 | Out-String
    Say $out.Trim()

    # Hashed AFTER the probe, not before: the probe writes files of its own onto
    # the source between copies, so that its increments have something to carry.
    # The restore reproduces the LAST copy, which is the source as it stands now.
    $hashes = @{}
    foreach ($file in (Get-ChildItem -LiteralPath 'S:\' -Recurse -File -ErrorAction SilentlyContinue)) {
        if ($file.Name -eq 'excluded-secret.bin') { continue }
        $hashes[$file.FullName] = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash
    }
    Say "LAB-OBK-SOURCE-AFTER files=$($hashes.Count)"

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

    $matched = 0
    foreach ($path in $hashes.Keys) {
        $there = $path -replace '^S:', "${letter}:"
        if (-not (Test-Path -LiteralPath $there)) {
            Say "LAB-OBK-FAIL missing after restore: $there"
            continue
        }
        $now = (Get-FileHash -LiteralPath $there -Algorithm SHA256).Hash
        if ($now -eq $hashes[$path]) {
            $matched++
            Say "LAB-OBK-FILE-OK $there"
        } else {
            Say "LAB-OBK-FAIL content differs after restore: $there"
        }
    }
    Say "LAB-OBK-FILES matched=$matched of=$($hashes.Count)"
    # The rule said to exclude this file: its data was never copied, and its
    # catalog entry was deleted inside the image. It must not be here.
    $excludedThere = "${letter}:\excluded-secret.bin"
    $stillThere = Test-Path -LiteralPath $excludedThere
    Say "LAB-OBK-EXCLUDED-PRESENT $stillThere (expected False)"
    if ($stillThere) { Say 'LAB-OBK-FAIL the excluded file is still in the restored volume' }

    # --- The bootable whole-disk copy, onto the same throwaway target ---
    # A different planner, a different composite reader and a different restore
    # than the volume path above, all of them Windows-only.
    $srcDisk = Get-Disk | Where-Object { $_.Location -eq $srcVhd } | Select-Object -First 1
    if (-not $srcDisk -or -not $tgtDisk) {
        Say 'LAB-OBK-DISK-SKIP could not find both virtual disks'
    } else {
        # Nothing may hold the target while a whole disk is written onto it.
        Set-Disk -Number $tgtDisk.Number -IsOffline $true
        Start-Sleep -Seconds 2
        $out = & $probe 'disk' $srcDisk.Number $work $tgtDisk.Number 2>&1 | Out-String
        $probeOk = ($LASTEXITCODE -eq 0)
        Say $out.Trim()
        Set-Disk -Number $tgtDisk.Number -IsOffline $false
        Start-Sleep -Seconds 3
        Diskpart @"
select vdisk file="$tgtVhd"
detach vdisk
attach vdisk
"@ | Out-Null
        Start-Sleep -Seconds 5

        # Comparing after a failed probe is worse than not comparing: the target
        # still holds the VOLUME restore from above, whose files match — a green
        # line for work that never ran.
        if (-not $probeOk) {
            Say 'LAB-OBK-DISK-FAIL the whole-disk probe failed — nothing compared'
            Say 'LAB-OBK-DONE'
            return
        }

        # The restored disk must carry the source's own volume, by its label.
        $back = Get-Disk | Where-Object { $_.Location -eq $tgtVhd } | Select-Object -First 1
        $vol = if ($back) {
            Get-Partition -DiskNumber $back.Number -ErrorAction SilentlyContinue |
                Get-Volume -ErrorAction SilentlyContinue |
                Where-Object { $_.FileSystemLabel -eq 'OBKSRC' } |
                Select-Object -First 1
        } else { $null }
        if (-not $vol) {
            Say 'LAB-OBK-DISK-FAIL the restored disk has no OBKSRC volume'
        } else {
            if (-not $vol.DriveLetter) {
                Diskpart @"
select vdisk file="$tgtVhd"
select partition 1
assign letter=Q
"@ | Out-Null
                Start-Sleep -Seconds 3
                $dl = 'Q'
            } else {
                $dl = $vol.DriveLetter
            }
            Say "LAB-OBK-DISK-VOLUME letter=$dl label=OBKSRC"
            $matched = 0
            foreach ($path in $hashes.Keys) {
                $there = $path -replace '^S:', "${dl}:"
                if (-not (Test-Path -LiteralPath $there)) {
                    Say "LAB-OBK-DISK-FAIL missing after whole-disk restore: $there"
                    continue
                }
                if ((Get-FileHash -LiteralPath $there -Algorithm SHA256).Hash -eq $hashes[$path]) {
                    $matched++
                } else {
                    Say "LAB-OBK-DISK-FAIL content differs: $there"
                }
            }
            Say "LAB-OBK-DISK-FILES matched=$matched of=$($hashes.Count)"
            $ghost = Test-Path -LiteralPath "${dl}:\excluded-secret.bin"
            Say "LAB-OBK-DISK-EXCLUDED-PRESENT $ghost (expected False)"
        }
    }
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
