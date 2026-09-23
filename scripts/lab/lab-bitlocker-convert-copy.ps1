# Копия заблокированного тома BitLocker посреди шифрования: старая сборка
# против новой, на одном и том же томе.
#
# Реальный случай: машина сломалась, пока шифровала C:, и человек грузится с
# носителя, чтобы снять копию. Том там заблокирован и конверсия стоит на месте.
# На 4 % на диске лежит зашифрованной лишь малая часть, остальное — открыто, а
# своя расшифровка прежней сборки расшифровывала ВСЁ как шифротекст: поле, по
# которому она судила о полноте (encrypted_size), с первой секунды равно
# полному размеру.
#
# Доказательство — по данным, не по журналу: большой файл, хеш до шифрования,
# хеш после копии и восстановления. Отдельно для каждой сборки.
#
# Пишет только на рабочий диск (OBKWORK) и на пустой диск. В конце выключает.

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
function Lines { param([string]$Tag, [string]$Text) foreach ($l in ($Text -split "`r?`n")) { if ($l.Trim()) { Note "  $Tag| $($l.Trim())" } } }

$pw = 'Str0ng-Lab-Passphrase-2026'
$work = $null
try {
    Note "LAB-BLC-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    $wv = Get-Volume | Where-Object { $_.FileSystemLabel -eq 'OBKWORK' -and $_.DriveLetter } | Select-Object -First 1
    if (-not $wv) { throw 'рабочий диск (OBKWORK) не найден' }
    $work = "$($wv.DriveLetter):"
    $cliNew = Join-Path $work 'own-backup-cli.exe'
    $cliOld = Join-Path $work 'cli-old.exe'
    foreach ($c in @($cliNew, $cliOld)) { if (-not (Test-Path -LiteralPath $c)) { throw "нет $c" } }

    $workDisk = (Get-Partition -DriveLetter $wv.DriveLetter).DiskNumber
    $blank = Get-Disk | Where-Object {
        -not $_.IsSystem -and -not $_.IsBoot -and $_.Number -ne $workDisk -and $_.Size -ge 12GB
    } | Sort-Object Number | Select-Object -First 1
    if (-not $blank) { throw 'пустого диска от 12 ГБ не нашлось' }
    Initialize-Disk -Number $blank.Number -PartitionStyle GPT -ErrorAction SilentlyContinue | Out-Null

    function NewVol { param([uint64]$Size, [string]$Label)
        $p = New-Partition -DiskNumber $blank.Number -Size $Size -AssignDriveLetter
        Start-Sleep -Seconds 2
        Format-Volume -Partition $p -FileSystem NTFS -NewFileSystemLabel $Label -Confirm:$false | Out-Null
        "$((Get-Partition -DiskNumber $blank.Number -PartitionNumber $p.PartitionNumber).DriveLetter):"
    }

    $src = NewVol -Size 4GB -Label CONVSRC
    $tgt = NewVol -Size 4GB -Label CONVTGT
    Note "LAB-BLC-VOLUMES источник=$src цель=$tgt"

    # Файл на полтора гигабайта: он ляжет далеко за ту долю тома, которую
    # шифрование успеет пройти до паузы. Каждый мегабайт свой — иначе
    # повторяющийся мусор мог бы совпасть сам с собой.
    $big = "$src\big.bin"
    $fs = [IO.File]::Create($big)
    $buf = New-Object byte[] 1048576
    $rng = New-Object Random 20260923
    for ($i = 0; $i -lt 1536; $i++) { $rng.NextBytes($buf); $fs.Write($buf, 0, $buf.Length) }
    $fs.Close()
    $control = (Get-FileHash -LiteralPath $big -Algorithm SHA256).Hash
    Note "LAB-BLC-CONTROL big.bin 1536 МиБ sha256=$control"

    $sec = ConvertTo-SecureString $pw -AsPlainText -Force
    Enable-BitLocker -MountPoint $src -PasswordProtector -Password $sec `
        -EncryptionMethod XtsAes128 -SkipHardwareTest -ErrorAction Stop | Out-Null

    # Пауза ПОСЛЕ того, как шифрование прошло копию загрузочных секторов (она
    # лежит около четверти тома). Первый прогон остановился на 4 %, и старую
    # сборку спасла случайность: сектор 0 не расшифровался, и сработала чужая
    # проверка — с объяснением «не тот ключ». Опасное окно — от ~25 % до 100 %:
    # там сектор 0 уже в порядке, а хвост тома ещё открыт.
    $minPercent = 40
    $deadline = [DateTime]::UtcNow.AddMinutes(5)
    while ([DateTime]::UtcNow -lt $deadline -and
           [double](Get-BitLockerVolume -MountPoint $src).EncryptionPercentage -lt $minPercent) {
        Start-Sleep -Milliseconds 200
    }
    $null = Run "manage-bde -pause $src"
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    while ([DateTime]::UtcNow -lt $deadline -and
           "$((Get-BitLockerVolume -MountPoint $src).VolumeStatus)" -notmatch 'Suspended') {
        Start-Sleep -Milliseconds 300
    }
    $bv = Get-BitLockerVolume -MountPoint $src
    Note "LAB-BLC-PAUSED status=$($bv.VolumeStatus) percent=$($bv.EncryptionPercentage)"

    Lock-BitLocker -MountPoint $src -ForceDismount -ErrorAction Stop | Out-Null
    Note "LAB-BLC-LOCKED $((Get-BitLockerVolume -MountPoint $src).LockStatus)"

    Lines 'probe' (Run "`"$cliNew`" bitlocker --volume $src")

    foreach ($build in @(@{ Name = 'СТАРАЯ'; Cli = $cliOld }, @{ Name = 'НОВАЯ'; Cli = $cliNew })) {
        $name = $build.Name
        $img = "$work\conv-$($name.ToLower()).qcow2"
        Remove-Item -LiteralPath $img -Force -ErrorAction SilentlyContinue
        Note "LAB-BLC-BACKUP $name"
        # Том после попытки может остаться открытым Windows (так устроен
        # обходной путь). Для чистоты каждой сборки — снова закрыть.
        if ("$((Get-BitLockerVolume -MountPoint $src).LockStatus)" -ne 'Locked') {
            Lock-BitLocker -MountPoint $src -ForceDismount -ErrorAction SilentlyContinue | Out-Null
        }
        Lines 'bk' (Run "`"$($build.Cli)`" backup $src --output `"$img`" --format qcow2 --password $pw")
        if (-not (Test-Path -LiteralPath $img)) {
            Note "LAB-BLC-RESULT $name копия не создана"
            continue
        }
        Lines 'rs' (Run "`"$cliNew`" restore `"$img`" --target $tgt --yes")
        Start-Sleep -Seconds 2
        $got = "$tgt\big.bin"
        if (Test-Path -LiteralPath $got) {
            $h = (Get-FileHash -LiteralPath $got -Algorithm SHA256).Hash
            Note ("LAB-BLC-RESULT {0} {1} sha256={2}" -f $name, $(if ($h -eq $control) { 'СОВПАЛ' } else { 'НЕ СОВПАЛ' }), $h)
        } else {
            $root = @(Get-ChildItem -LiteralPath "$tgt\" -Force -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Name) -join ', '
            Note "LAB-BLC-RESULT $name файла на цели нет (корень: $root)"
        }
    }
    Note 'LAB-BLC-OK'
} catch {
    Note "LAB-BLC-FAIL $($_.Exception.Message)"
} finally {
    if ($work) {
        try { Set-Content -LiteralPath "$work\convert-report.txt" -Value ($report -join "`r`n") -Encoding UTF8 } catch { }
    }
    Say 'LAB-BLC-POWEROFF'
    Start-Sleep -Seconds 3
    shutdown /s /t 5
}
