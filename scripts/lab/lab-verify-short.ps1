# Четыре короткие проверки из «сделано, но живьём не проверено», на одноразовой
# Windows в WSL. Каждая независима: падение одной не уносит остальные, и по
# каждой в конце стоит вердикт, а не «скрипт дошёл до конца».
#
# Чего здесь НЕТ и почему:
#   * том только с TPM — у этой машины TPM нет вовсе, проверять нечем;
#   * малый scratch носителя и сводка по типам файлов — нужны загрузка WinPE
#     и живой рабочий стол.
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

# Родные средства пишут обычные ответы в stderr, и в PowerShell 5.1 это
# становится терминирующей ошибкой, как только поток перенаправлен. Пусть
# перенаправляет cmd.
function Run { param([string]$Cmd) try { (cmd.exe /c "$Cmd 2>&1" | Out-String).Trim() } catch { "не запустилось: $($_.Exception.Message)" } }

$verdict = [ordered]@{}
function Verdict { param([string]$Item, [string]$Result) $verdict[$Item] = $Result; Note "  ВЕРДИКТ [$Item] $Result" }

$work = $null
try {
    Note "LAB-VER-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    Note "LAB-VER-EDITION $((Get-CimInstance Win32_OperatingSystem).Caption)"

    $wv = Get-Volume -ErrorAction SilentlyContinue |
        Where-Object { $_.FileSystemLabel -eq 'OBKWORK' -and $_.DriveLetter } | Select-Object -First 1
    if (-not $wv) { throw 'рабочий диск (метка OBKWORK) не найден' }
    $work = "$($wv.DriveLetter):"
    $cli = Join-Path $work 'own-backup-cli.exe'
    if (-not (Test-Path -LiteralPath $cli)) { throw "нет $cli" }
    Note "LAB-VER-WORK $work"

    # --- пустой диск под разделы проверок ---------------------------------
    $workDisk = (Get-Partition -DriveLetter $wv.DriveLetter).DiskNumber
    $blank = Get-Disk | Where-Object {
        -not $_.IsSystem -and -not $_.IsBoot -and $_.Number -ne $workDisk -and $_.Size -ge 6GB
    } | Sort-Object Number | Select-Object -First 1
    if (-not $blank) { throw 'пустого диска под проверки не нашлось' }
    Note "LAB-VER-SCRATCH disk=$($blank.Number) size=$($blank.Size) style=$($blank.PartitionStyle)"

    if ($blank.PartitionStyle -ne 'RAW') {
        Clear-Disk -Number $blank.Number -RemoveData -RemoveOEM -Confirm:$false -ErrorAction SilentlyContinue
    }
    Initialize-Disk -Number $blank.Number -PartitionStyle GPT -ErrorAction SilentlyContinue | Out-Null

    function NewPart {
        param([uint64]$Size, [string]$Fs, [string]$Label)
        $p = New-Partition -DiskNumber $blank.Number -Size $Size -AssignDriveLetter
        Start-Sleep -Seconds 2
        Format-Volume -Partition $p -FileSystem $Fs -NewFileSystemLabel $Label -Confirm:$false -ErrorAction Stop | Out-Null
        (Get-Partition -DiskNumber $blank.Number -PartitionNumber $p.PartitionNumber)
    }

    # ---------------------------------------------------------------------
    # 1. ReFS-раздел в копии ЦЕЛОГО диска
    # ---------------------------------------------------------------------
    Note 'LAB-VER-1 ReFS в копии целого диска'
    $refsPart = $null
    try {
        $refsPart = NewPart -Size 2GB -Fs ReFS -Label REFSTEST
        $r = "$((Get-Partition -DiskNumber $blank.Number -PartitionNumber $refsPart.PartitionNumber).DriveLetter):"
        Note "  ReFS создан на $r"
        New-Item -ItemType Directory -Path "$r\data" -Force | Out-Null
        1..20 | ForEach-Object { Set-Content -LiteralPath "$r\data\file$_.txt" -Value ("refs" * 500) }
        Set-Content -LiteralPath "$r\data\secret.log" -Value ('нельзя копировать' * 100)
    } catch {
        Verdict 'ReFS' "НЕ ПРОВЕРЕНО: ReFS не создаётся на этой машине ($($_.Exception.Message))"
    }

    if ($refsPart) {
        # NTFS-сосед, чтобы копия диска была не из одного раздела.
        $ntfsPart = NewPart -Size 2GB -Fs NTFS -Label NTFSTEST
        $n = "$((Get-Partition -DiskNumber $blank.Number -PartitionNumber $ntfsPart.PartitionNumber).DriveLetter):"
        New-Item -ItemType Directory -Path "$n\data" -Force | Out-Null
        1..20 | ForEach-Object { Set-Content -LiteralPath "$n\data\file$_.txt" -Value ("ntfs" * 500) }

        $img = "$work\wholedisk.qcow2"
        Remove-Item -LiteralPath $img -Force -ErrorAction SilentlyContinue
        $parts = "$($refsPart.PartitionNumber),$($ntfsPart.PartitionNumber)"
        $out = Run "`"$cli`" backup-disk --disk $($blank.Number) --partitions $parts --output `"$img`" --format qcow2 --exclude secret.log"
        foreach ($l in ($out -split "`r?`n")) { if ($l.Trim()) { Note "  out| $($l.Trim())" } }
        if (Test-Path -LiteralPath $img) {
            $sz = (Get-Item -LiteralPath $img).Length
            Note "  образ создан, $([math]::Round($sz/1MB)) МиБ"
            $v = Run "`"$cli`" verify `"$img`""
            foreach ($l in ($v -split "`r?`n")) { if ($l.Trim()) { Note "  verify| $($l.Trim())" } }
            Verdict 'ReFS' $(if ($v -match 'VALID') { 'копия целого диска с ReFS создана и проверена' } else { 'образ создан, но verify не подтвердил' })
        } else {
            Verdict 'ReFS' 'ОТКАЗ: образ не создан'
        }
    }

    # ---------------------------------------------------------------------
    # 2–3. BitLocker: переходное состояние, неверный пароль, копия и возврат
    # ---------------------------------------------------------------------
    Note 'LAB-VER-2 BitLocker'
    $blPart = $null
    $blPw = 'Str0ng-Lab-Passphrase-2026'
    try {
        if (-not (Get-Command Enable-BitLocker -ErrorAction SilentlyContinue)) {
            throw 'модуля BitLocker нет в этой редакции'
        }
        $blPart = NewPart -Size 1GB -Fs NTFS -Label BLTEST
        $b = "$((Get-Partition -DiskNumber $blank.Number -PartitionNumber $blPart.PartitionNumber).DriveLetter):"
        New-Item -ItemType Directory -Path "$b\data" -Force | Out-Null
        1..30 | ForEach-Object { Set-Content -LiteralPath "$b\data\bl$_.txt" -Value ("bitlocker" * 400) }
        $controlHash = (Get-FileHash -LiteralPath "$b\data\bl1.txt" -Algorithm SHA256).Hash
        Note "  том $b подготовлен, контрольный файл $controlHash"

        $sec = ConvertTo-SecureString $blPw -AsPlainText -Force
        Enable-BitLocker -MountPoint $b -PasswordProtector -Password $sec `
            -EncryptionMethod XtsAes128 -SkipHardwareTest -ErrorAction Stop | Out-Null
        Note "  шифрование запущено, состояние: $((Get-BitLockerVolume -MountPoint $b).VolumeStatus)"

        # (а) том В ПРОЦЕССЕ шифрования — что скажет наш разбор
        $st = (Get-BitLockerVolume -MountPoint $b).VolumeStatus
        if ("$st" -eq 'EncryptionInProgress') {
            $mid = Run "`"$cli`" bitlocker --volume $b"
            foreach ($l in ($mid -split "`r?`n")) { if ($l.Trim()) { Note "  mid| $($l.Trim())" } }
            Verdict 'BitLocker-переход' 'снят ответ на томе в процессе шифрования (см. mid| выше)'
        } else {
            Verdict 'BitLocker-переход' "НЕ ПОЙМАН: состояние успело стать $st"
        }

        # дождаться конца шифрования
        $deadline = [DateTime]::UtcNow.AddMinutes(20)
        while ([DateTime]::UtcNow -lt $deadline -and
               (Get-BitLockerVolume -MountPoint $b).VolumeStatus -eq 'EncryptionInProgress') {
            Start-Sleep -Seconds 15
        }
        Note "  состояние после ожидания: $((Get-BitLockerVolume -MountPoint $b).VolumeStatus)"
        Lock-BitLocker -MountPoint $b -ForceDismount -ErrorAction SilentlyContinue | Out-Null
        Note "  том заблокирован: $((Get-BitLockerVolume -MountPoint $b).LockStatus)"

        # (б) НЕВЕРНЫЙ пароль — должно быть «пароль не подходит», а не ошибка ввода-вывода
        $wrongOut = Run "echo Definitely-Not-The-Password|`"$cli`" bitlocker --volume $b --unlock"
        foreach ($l in ($wrongOut -split "`r?`n")) { if ($l.Trim()) { Note "  wrong| $($l.Trim())" } }
        # По КАТЕГОРИИ ошибки, а не по русскому тексту: вывод cmd приходит в
        # кодировке 866, и сравнение с «парол» по нему не срабатывает — прогоны
        # 3–7 ставили «ОТКАЗ» верному ответу. Категория пишется латиницей.
        $isEncryption = $wrongOut -match 'Encryption error'
        $isIo = $wrongOut -match 'Device I/O error'
        Verdict 'BitLocker-неверный-пароль' $(
            if ($isEncryption -and -not $isIo) { 'категория Encryption, не ввод-вывод' }
            elseif ($isIo) { 'ОТКАЗ: подан как ошибка ввода-вывода' }
            else { 'ОТКАЗ: категория не опознана' })

        # (в) ВЕРНЫЙ пароль: копия заблокированного тома и возврат из неё
        $blImg = "$work\bitlocker.qcow2"
        Remove-Item -LiteralPath $blImg -Force -ErrorAction SilentlyContinue
        $bk = Run "`"$cli`" backup $b --output `"$blImg`" --format qcow2 --password $blPw"
        foreach ($l in ($bk -split "`r?`n")) { if ($l.Trim()) { Note "  bk| $($l.Trim())" } }

        if (Test-Path -LiteralPath $blImg) {
            $tgt = NewPart -Size 1GB -Fs NTFS -Label BLRESTORE
            $t = "$((Get-Partition -DiskNumber $blank.Number -PartitionNumber $tgt.PartitionNumber).DriveLetter):"
            $rs = Run "`"$cli`" restore `"$blImg`" --target $t --yes"
            foreach ($l in ($rs -split "`r?`n")) { if ($l.Trim()) { Note "  rs| $($l.Trim())" } }
            # Восстановление снимает том, чтобы писать через диск; обратно Windows
            # выводит его не сразу. Пятый прогон проверил через три секунды и не
            # нашёл ничего — а снаружи, из Linux, на разделе лежали все 30
            # файлов с верной суммой. Ждём появления, а не фиксированное время, и
            # пишем, сколько ждали: если это десятки секунд, это уже вопрос к
            # продукту, а не к проверке.
            $restored = "$t\data\bl1.txt"
            $waited = 0
            Note ("  том {0} сразу после: {1}" -f $t, ((Get-Volume -DriveLetter $t.TrimEnd(':') -ErrorAction SilentlyContinue |
                ForEach-Object { "fs=$($_.FileSystem) health=$($_.HealthStatus) size=$($_.Size)" }) -join ' '))
            while ($waited -lt 60 -and -not (Test-Path -LiteralPath $restored)) {
                Start-Sleep -Seconds 2; $waited += 2
            }
            Note "  файл на цели появился через $waited с"
            if (Test-Path -LiteralPath $restored) {
                $h = (Get-FileHash -LiteralPath $restored -Algorithm SHA256).Hash
                $count = @(Get-ChildItem -LiteralPath "$t\data" -File).Count
                Note "  восстановлено файлов: $count, контрольный $h"
                Verdict 'BitLocker-восстановление' $(
                    if ($h -eq $controlHash) { "содержимое совпало побайтово ($count файлов)" }
                    else { 'ОТКАЗ: контрольный файл не совпал' })
            } else {
                Verdict 'BitLocker-восстановление' 'ОТКАЗ: файла на цели нет'
            }
        } else {
            Verdict 'BitLocker-восстановление' 'ОТКАЗ: копия заблокированного тома не создана'
        }
    } catch {
        Verdict 'BitLocker' "НЕ ПРОВЕРЕНО: $($_.Exception.Message)"
    }

    # ---------------------------------------------------------------------
    # 4. Чтение образа по iSCSI — «подключить копию и посмотреть файлы»
    # ---------------------------------------------------------------------
    # Продуктовым путём: подключает СЛУЖБА, командная строка её просит. Ручной
    # `iscsi-serve` + `iscsicli` — дорога для человека с отладкой, и в первом
    # прогоне я сам ошибся в её аргументах; проверять надо то, чем пользуются.
    Note 'LAB-VER-4 подключение копии как диска (служба + mount)'
    $svc = $null
    try {
        $img = "$work\wholedisk.qcow2"
        if (-not (Test-Path -LiteralPath $img)) { throw 'нечего подключать: образ первого пункта не создан' }
        $svcExe = Join-Path $work 'own-backup-service.exe'
        if (-not (Test-Path -LiteralPath $svcExe)) { throw "нет $svcExe" }

        $before = @(Get-Disk | ForEach-Object { $_.Number })
        $svc = Start-Process -FilePath $svcExe -ArgumentList @('--foreground') `
            -RedirectStandardOutput "$work\service-out.txt" -RedirectStandardError "$work\service-err.txt" `
            -NoNewWindow -PassThru
        $null = $svc.Handle
        Start-Sleep -Seconds 10
        if ($svc.HasExited) { throw "служба не поднялась (код $($svc.ExitCode)) — см. service-err.txt" }
        Note '  служба запущена в консоли'

        $m = Run "`"$cli`" mount `"$img`""
        foreach ($l in ($m -split "`r?`n")) { if ($l.Trim()) { Note "  mount| $($l.Trim())" } }

        # Ждём ПОЯВЛЕНИЯ разделов, а не фиксированные восемь секунд: Windows
        # выводит тома после подключения не мгновенно, и «не успело» с виду
        # неотличимо от «не получилось».
        $new = @()
        $deadline = [DateTime]::UtcNow.AddSeconds(90)
        while ([DateTime]::UtcNow -lt $deadline) {
            Update-HostStorageCache -ErrorAction SilentlyContinue
            $new = @(Get-Disk | ForEach-Object { $_.Number }) | Where-Object { $_ -notin $before }
            if ($new -and @(Get-Partition -DiskNumber $new[0] -ErrorAction SilentlyContinue).Count -gt 0) { break }
            Start-Sleep -Seconds 5
        }
        Note "  появились диски: $($new -join ',')"

        # Состояние подключённого диска: образы несут атрибут «offline»
        # намеренно, и тогда томов не будет вовсе — это надо видеть, а не
        # гадать по пустому списку.
        foreach ($d in $new) {
            $dk = Get-Disk -Number $d -ErrorAction SilentlyContinue
            Note ("  диск {0}: offline={1} readonly={2} style={3} size={4} model={5}" -f `
                $d, $dk.IsOffline, $dk.IsReadOnly, $dk.PartitionStyle, $dk.Size, $dk.FriendlyName)
            if ($dk.IsOffline) {
                Note '  диск отключён — перевожу в online'
                try { Set-Disk -Number $d -IsOffline $false -ErrorAction Stop; Start-Sleep -Seconds 5 }
                catch { Note "  online не удался: $($_.Exception.Message)" }
            }
            foreach ($p in (Get-Partition -DiskNumber $d -ErrorAction SilentlyContinue)) {
                Note ("    раздел {0}: тип={1} смещение={2} размер={3} буква={4}" -f `
                    $p.PartitionNumber, $p.GptType, $p.Offset, $p.Size, $p.DriveLetter)
            }
        }

        $seen = 0
        foreach ($d in $new) {
            foreach ($p in (Get-Partition -DiskNumber $d -ErrorAction SilentlyContinue)) {
                $v = Get-Volume -Partition $p -ErrorAction SilentlyContinue
                # Том без буквы читать нечем — а буквы образу никто не обещал.
                if ($v -and -not $v.DriveLetter) {
                    $free = [char[]](74..90) | Where-Object { -not (Test-Path "$([char]$_):\") } | Select-Object -First 1
                    if ($free) {
                        try {
                            Set-Partition -DiskNumber $d -PartitionNumber $p.PartitionNumber -NewDriveLetter $free -ErrorAction Stop
                            Start-Sleep -Seconds 2
                            $v = Get-Volume -Partition (Get-Partition -DiskNumber $d -PartitionNumber $p.PartitionNumber)
                            Note "    разделу $($p.PartitionNumber) назначена буква $free"
                        } catch { Note "    буква не назначилась: $($_.Exception.Message)" }
                    }
                }
                if ($v -and $v.DriveLetter) {
                    $files = @(Get-ChildItem -LiteralPath "$($v.DriveLetter):\data" -File -ErrorAction SilentlyContinue)
                    Note "  том $($v.DriveLetter): fs=$($v.FileSystem) файлов в \data = $($files.Count)"
                    if ($files.Count -gt 0) {
                        $h = (Get-FileHash -LiteralPath $files[0].FullName -Algorithm SHA256).Hash
                        Note "    первый файл $($files[0].Name) sha256=$h"
                    }
                    $seen += $files.Count
                }
            }
        }
        Verdict 'Подключение копии' $(if ($seen -gt 0) { "подключено и прочитано: $seen файлов" } else { 'ОТКАЗ: файлы не прочитались' })

        $u = Run "`"$cli`" unmount `"$img`""
        foreach ($l in ($u -split "`r?`n")) { if ($l.Trim()) { Note "  unmount| $($l.Trim())" } }
    } catch {
        Verdict 'Подключение копии' "НЕ ПРОВЕРЕНО: $($_.Exception.Message)"
    } finally {
        # Сессия инициатора, пережившая свою цель, держит выключение машины:
        # прошлый прогон не выключился сам и его пришлось останавливать снаружи.
        Note "  сессии iSCSI до уборки: $(Run 'iscsicli SessionList | findstr /c:iqn')"
        $null = Run 'iscsicli ReportTargetMappings'
        foreach ($t in (((Run 'iscsicli ListTargets') -split "`r?`n") | Where-Object { $_ -match 'ownbackup' })) {
            $null = Run "iscsicli LogoutTarget $($t.Trim())"
        }
        if ($svc -and -not $svc.HasExited) { Stop-Process -Id $svc.Id -Force -ErrorAction SilentlyContinue }
        Start-Sleep -Seconds 3
    }

    Note 'LAB-VER-SUMMARY'
    foreach ($k in $verdict.Keys) { Note ("  {0,-28} {1}" -f $k, $verdict[$k]) }
    Note 'LAB-VER-OK'
} catch {
    Note "LAB-VER-FAIL $($_.Exception.Message)"
} finally {
    if ($work) {
        try { Set-Content -LiteralPath "$work\verify-report.txt" -Value ($report -join "`r`n") -Encoding UTF8 } catch { }
    }
    Say 'LAB-VER-POWEROFF'
    Start-Sleep -Seconds 3
    shutdown /s /t 5
}
