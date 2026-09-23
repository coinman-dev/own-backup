# Метаданные BitLocker в разных состояниях конверсии — чтобы найти поле,
# которое говорит «шифрование ещё идёт».
#
# Зачем: проверка «зашифрована ли часть тома» опирается на encrypted_size, а
# живой прогон 2026-09-22 показал, что это поле равно ПОЛНОМУ размеру тома с
# первой секунды конверсии. Порог 90 % поэтому не ловит незавершённое
# шифрование в принципе, а копия такого тома молча несёт мусор там, где
# BitLocker ещё не дошёл. Где на самом деле записан ход конверсии — не знает
# никто из тех, кого мы читали; узнать можно только сравнением живых блоков.
#
# Что делает: шифрует том, ставит конверсию на ПАУЗУ в нескольких точках и в
# каждой снимает блоки метаданных нашим `bitlocker --dump` вместе с тем, что
# говорит сама Windows (процент и состояние). Потом то же для расшифровки.
# Сравнение делается снаружи, из Linux.
#
# Пишет только на рабочий диск (метка OBKWORK) и на пустой диск, который сам
# размечает. Что бы ни случилось, машина в конце выключается.

$ErrorActionPreference = 'Continue'

function Say {
    param([string]$Text)
    if ($null -eq $Text) { $Text = '' }
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}
$report = New-Object System.Collections.ArrayList
function Note { param([string]$T) [void]$report.Add($T); Say $T }
function Run { param([string]$Cmd) try { (cmd.exe /c "$Cmd 2>&1" | Out-String).Trim() } catch { "не запустилось: $($_.Exception.Message)" } }

$work = $null
try {
    Note "LAB-BLS-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    $wv = Get-Volume | Where-Object { $_.FileSystemLabel -eq 'OBKWORK' -and $_.DriveLetter } | Select-Object -First 1
    if (-not $wv) { throw 'рабочий диск (OBKWORK) не найден' }
    $work = "$($wv.DriveLetter):"
    $cli = Join-Path $work 'own-backup-cli.exe'
    $out = Join-Path $work 'states'
    New-Item -ItemType Directory -Path $out -Force | Out-Null

    $workDisk = (Get-Partition -DriveLetter $wv.DriveLetter).DiskNumber
    $blank = Get-Disk | Where-Object {
        -not $_.IsSystem -and -not $_.IsBoot -and $_.Number -ne $workDisk -and $_.Size -ge 6GB
    } | Sort-Object Number | Select-Object -First 1
    if (-not $blank) { throw 'пустого диска не нашлось' }
    Initialize-Disk -Number $blank.Number -PartitionStyle GPT -ErrorAction SilentlyContinue | Out-Null

    # Восемь гигабайт и без UsedSpaceOnly: шифруется весь том, и конверсия
    # идёт достаточно долго, чтобы её можно было поймать несколько раз.
    $p = New-Partition -DiskNumber $blank.Number -Size 8GB -AssignDriveLetter
    Start-Sleep -Seconds 2
    Format-Volume -Partition $p -FileSystem NTFS -NewFileSystemLabel BLSTATES -Confirm:$false | Out-Null
    $v = "$((Get-Partition -DiskNumber $blank.Number -PartitionNumber $p.PartitionNumber).DriveLetter):"
    Note "LAB-BLS-VOLUME $v disk=$($blank.Number) offset=$($p.Offset) size=$($p.Size)"
    New-Item -ItemType Directory -Path "$v\data" -Force | Out-Null
    1..50 | ForEach-Object { Set-Content -LiteralPath "$v\data\f$_.txt" -Value ("state" * 4000) }

    function Capture {
        param([string]$Name)
        $bv = Get-BitLockerVolume -MountPoint $v
        Note ("LAB-BLS-STATE {0} status={1} percent={2} protection={3} lock={4}" -f `
            $Name, $bv.VolumeStatus, $bv.EncryptionPercentage, $bv.ProtectionStatus, $bv.LockStatus)
        Set-Content -LiteralPath "$out\$Name.manage-bde.txt" -Value (Run "manage-bde -status $v") -Encoding UTF8
        $d = Run "`"$cli`" bitlocker --volume $v --dump `"$out\$Name`""
        Set-Content -LiteralPath "$out\$Name.cli.txt" -Value $d -Encoding UTF8
        $files = @(Get-ChildItem -LiteralPath $out -Filter "$Name.*.bin").Count
        Note "  снято файлов: $files"
    }

    function PauseAt {
        param([string]$Name)
        $null = Run "manage-bde -pause $v"
        # Ждём, пока Windows ДЕЙСТВИТЕЛЬНО встанет: пауза асинхронна, и снимок
        # во время движения поймал бы метаданные на полпути записи.
        # Пауза у Windows называется «Suspended», а не «Paused»: первая версия
        # ждала не то слово и простаивала по минуте на каждой точке.
        $deadline = [DateTime]::UtcNow.AddSeconds(30)
        while ([DateTime]::UtcNow -lt $deadline) {
            $s = "$((Get-BitLockerVolume -MountPoint $v).VolumeStatus)"
            if ($s -match 'Suspended|Paused|FullyEncrypted|FullyDecrypted') { break }
            Start-Sleep -Milliseconds 300
        }
        Capture $Name
    }

    Capture 'S0-plain'

    $sec = ConvertTo-SecureString 'Str0ng-Lab-Passphrase-2026' -AsPlainText -Force
    Enable-BitLocker -MountPoint $v -PasswordProtector -Password $sec `
        -EncryptionMethod XtsAes128 -SkipHardwareTest -ErrorAction Stop | Out-Null

    # Несколько точек на РАЗНЫХ процентах. Одна точка не отличает флаг
    # «конверсия идёт» от счётчика прогресса; три — отличают. Окна короткие:
    # в этой машине 4 ГиБ шифруются секунд за двадцать, и прошлая версия
    # проскочила всё, кроме первой точки.
    PauseAt 'S1-enc-a'
    foreach ($name in @('S2-enc-b', 'S3-enc-c')) {
        $null = Run "manage-bde -resume $v"
        Start-Sleep -Milliseconds 1500
        PauseAt $name
    }

    $null = Run "manage-bde -resume $v"
    $deadline = [DateTime]::UtcNow.AddMinutes(30)
    while ([DateTime]::UtcNow -lt $deadline -and
           "$((Get-BitLockerVolume -MountPoint $v).VolumeStatus)" -ne 'FullyEncrypted') {
        Start-Sleep -Seconds 2
    }
    Capture 'S4-enc-full'

    # И обратный путь: расшифровка тоже конверсия, и копия тома в процессе
    # расшифровки так же несёт мусор в ещё не расшифрованной части. Пауза —
    # сразу, без ожидания: прошлая версия ждала три секунды, и расшифровка
    # успела закончиться целиком.
    Disable-BitLocker -MountPoint $v -ErrorAction SilentlyContinue | Out-Null
    PauseAt 'S5-dec-a'
    $null = Run "manage-bde -resume $v"
    Start-Sleep -Milliseconds 1500
    PauseAt 'S6-dec-b'

    Note 'LAB-BLS-OK'
} catch {
    Note "LAB-BLS-FAIL $($_.Exception.Message)"
} finally {
    if ($work) {
        try { Set-Content -LiteralPath "$work\states-report.txt" -Value ($report -join "`r`n") -Encoding UTF8 } catch { }
    }
    Say 'LAB-BLS-POWEROFF'
    Start-Sleep -Seconds 3
    shutdown /s /t 5
}
