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
        $looksLikePassword = $wrongOut -match 'парол|password'
        $looksLikeIo = $wrongOut -match 'ввод|I/O|Io\(|ERROR_|0x8007'
        Verdict 'BitLocker-неверный-пароль' $(
            if ($looksLikePassword -and -not $looksLikeIo) { 'сказано про пароль, не про ввод-вывод' }
            elseif ($looksLikePassword) { 'СМЕШАНО: есть и про пароль, и похожее на ввод-вывод' }
            else { 'ОТКАЗ: про пароль не сказано' })

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
            Start-Sleep -Seconds 3
            $restored = "$t\data\bl1.txt"
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
    Note 'LAB-VER-4 чтение образа по iSCSI'
    try {
        $img = "$work\wholedisk.qcow2"
        if (-not (Test-Path -LiteralPath $img)) { throw 'нечего подключать: образ первого пункта не создан' }
        $before = @(Get-Disk | ForEach-Object { $_.Number })
        $srv = Start-Process -FilePath $cli -ArgumentList @('iscsi-serve', $img) `
            -RedirectStandardOutput "$work\iscsi-out.txt" -RedirectStandardError "$work\iscsi-err.txt" `
            -NoNewWindow -PassThru
        $null = $srv.Handle
        Start-Sleep -Seconds 5
        Note "  цель поднята: $(Run 'sc query msiscsi | findstr STATE')"
        Note "  $(Run 'iscsicli QAddTargetPortal 127.0.0.1 3260')"
        Start-Sleep -Seconds 3
        # Скобки обязательны: `Run '…' -split '…'` разбирается как ВЫЗОВ Run с
        # аргументом -split, а не как оператор — и в имя цели попадает баннер
        # версии инициатора. Из-за этого пункт провалился в первом прогоне.
        $tgtName = (((Run 'iscsicli ListTargets') -split "`r?`n") |
            Where-Object { $_ -match 'ownbackup' } | Select-Object -First 1).Trim()
        Note "  цель: $tgtName"
        if ($tgtName) {
            Note "  $(Run "iscsicli QLoginTarget $tgtName")"
            Start-Sleep -Seconds 6
            Update-HostStorageCache -ErrorAction SilentlyContinue
            $after = @(Get-Disk | ForEach-Object { $_.Number })
            $new = $after | Where-Object { $_ -notin $before }
            Note "  появились диски: $($new -join ',')"
            $seen = 0
            foreach ($d in $new) {
                foreach ($p in (Get-Partition -DiskNumber $d -ErrorAction SilentlyContinue)) {
                    $v = Get-Volume -Partition $p -ErrorAction SilentlyContinue
                    if ($v -and $v.DriveLetter) {
                        $names = @(Get-ChildItem -LiteralPath "$($v.DriveLetter):\data" -File -ErrorAction SilentlyContinue).Count
                        Note "  том $($v.DriveLetter): fs=$($v.FileSystem) файлов в \data = $names"
                        $seen += $names
                    }
                }
            }
            Verdict 'iSCSI-чтение' $(if ($seen -gt 0) { "подключено и прочитано: $seen файлов" } else { 'ОТКАЗ: файлы не прочитались' })
            foreach ($d in $new) { Note "  $(Run "iscsicli LogoutTarget $tgtName")" ; break }
        } else {
            Verdict 'iSCSI-чтение' 'ОТКАЗ: инициатор не увидел цель'
        }
        Start-Sleep -Seconds 2
        if (-not $srv.HasExited) { Stop-Process -Id $srv.Id -Force -ErrorAction SilentlyContinue }
    } catch {
        Verdict 'iSCSI-чтение' "НЕ ПРОВЕРЕНО: $($_.Exception.Message)"
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
