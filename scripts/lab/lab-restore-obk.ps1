# Полное восстановление зашифрованной копии диска на стенде.
#
# Запускается СИСТЕМОЙ из задачи при старте (бутстрап stage 1 копирует сюда
# всё из `<носитель>\lab` и вызывает `lab-stage2.ps1` — этот файл кладётся на
# диск с архивом под этим именем).
#
# Чего этот прогон стоит: пароль вводится в продуктовую командную строку на
# машине, которая никогда не видела ни задания, ни базы, ни локального ключа
# службы. Открыть цепочку можно только переносимыми слотами внутри образов —
# ровно то, что обещает §7.5. Восстанавливается ВЕСЬ диск: таблица разделов и
# каждый раздел, а не отдельный том.
#
# Что бы ни случилось, машина в конце выключается: зависшая фаза не сообщает
# оператору ничего и блокирует следующую.

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
function Note {
    param([string]$Text)
    [void]$report.Add($Text)
    Say $Text
}

$archRoot = $null
try {
    Note "LAB-RST-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    Note "LAB-RST-VERSION 1"

    # --- где что лежит -----------------------------------------------------
    $vol = Get-Volume -ErrorAction SilentlyContinue |
        Where-Object { $_.FileSystemLabel -eq 'OBKARCH' -and $_.DriveLetter } |
        Select-Object -First 1
    if (-not $vol) { throw 'диск с архивом (метка OBKARCH) не найден' }
    $archRoot = "$($vol.DriveLetter):"
    Note "LAB-RST-ARCHIVE $archRoot"

    $cli = Join-Path $archRoot 'own-backup-cli.exe'
    $img = Join-Path $archRoot 'test_disk4_inc_20260922-130745.obk'
    $pwf = Join-Path $archRoot 'secret.txt'
    foreach ($p in @($cli, $img, $pwf)) {
        if (-not (Test-Path -LiteralPath $p)) { throw "нет файла: $p" }
    }

    # --- какой диск перезаписываем ----------------------------------------
    # Образ несёт диск на 137 415 884 800 байт; цель должна быть не меньше и
    # не должна быть ни системной, ни той, с которой читается архив.
    $needed = 137415884800
    $archDiskNumber = (Get-Partition -DriveLetter $vol.DriveLetter -ErrorAction SilentlyContinue).DiskNumber
    Note "LAB-RST-DISKS"
    foreach ($d in (Get-Disk | Sort-Object Number)) {
        Note ("  disk={0} size={1} style={2} system={3} boot={4} model={5}" -f `
            $d.Number, $d.Size, $d.PartitionStyle, $d.IsSystem, $d.IsBoot, $d.FriendlyName)
    }
    $target = Get-Disk |
        Where-Object { -not $_.IsSystem -and -not $_.IsBoot -and
                       $_.Number -ne $archDiskNumber -and $_.Size -ge $needed } |
        Sort-Object Number | Select-Object -First 1
    if (-not $target) { throw "нет пустого диска размером от $needed байт" }
    Note "LAB-RST-TARGET disk=$($target.Number) size=$($target.Size)"

    # --- само восстановление ----------------------------------------------
    $out = Join-Path $archRoot 'restore-stdout.txt'
    $err = Join-Path $archRoot 'restore-stderr.txt'
    $started = [DateTime]::UtcNow
    Note "LAB-RST-RUN restore-disk --target-disk $($target.Number) --yes"
    $proc = Start-Process -FilePath $cli `
        -ArgumentList @('restore-disk', $img, '--target-disk', "$($target.Number)", '--yes') `
        -RedirectStandardInput $pwf -RedirectStandardOutput $out -RedirectStandardError $err `
        -NoNewWindow -PassThru
    # Touching .Handle makes PowerShell keep the process handle open. Without
    # it the object is there, HasExited is true and ExitCode is EMPTY — so a
    # restore that finished perfectly reads as a failure.
    $null = $proc.Handle

    # Пока идёт запись — говорить о ней вслух: молчащая фаза неотличима от
    # зависшей, а восстановление сорока гигабайт идёт минутами.
    while (-not $proc.HasExited) {
        Start-Sleep -Seconds 30
        $last = ''
        try {
            $tail = Get-Content -LiteralPath $out -Tail 1 -ErrorAction SilentlyContinue
            if ($tail) { $last = ($tail -split "`r")[-1] }
        } catch { }
        Say ("LAB-RST-PROGRESS {0}s {1}" -f [int]([DateTime]::UtcNow - $started).TotalSeconds, $last)
    }
    $proc.WaitForExit()
    $secs = [int]([DateTime]::UtcNow - $started).TotalSeconds
    Note "LAB-RST-EXIT code=$($proc.ExitCode) secs=$secs"
    foreach ($line in (Get-Content -LiteralPath $out -ErrorAction SilentlyContinue)) {
        foreach ($piece in ($line -split "`r")) {
            if ($piece.Trim()) { Note "  out| $($piece.Trim())" }
        }
    }
    foreach ($line in (Get-Content -LiteralPath $err -ErrorAction SilentlyContinue)) {
        if ($line.Trim()) { Note "  err| $($line.Trim())" }
    }
    if ($null -eq $proc.ExitCode) { throw 'код возврата restore-disk неизвестен' }
    if ($proc.ExitCode -ne 0) { throw "restore-disk вернул $($proc.ExitCode)" }

    # --- что получилось на цели -------------------------------------------
    Update-HostStorageCache -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 3
    $t = Get-Disk -Number $target.Number
    Note "LAB-RST-LAYOUT style=$($t.PartitionStyle) guid=$($t.Guid)"
    foreach ($p in (Get-Partition -DiskNumber $target.Number -ErrorAction SilentlyContinue | Sort-Object PartitionNumber)) {
        Note ("  part={0} type={1} offset={2} size={3} guid={4}" -f `
            $p.PartitionNumber, $p.GptType, $p.Offset, $p.Size, $p.Guid)
    }

    # Файловые свидетельства: тот же обход, которым пользуется человек.
    foreach ($p in (Get-Partition -DiskNumber $target.Number -ErrorAction SilentlyContinue)) {
        $v = $null
        try { $v = Get-Volume -Partition $p -ErrorAction SilentlyContinue } catch { }
        if (-not $v) { continue }
        Note ("  vol part={0} fs={1} label={2} size={3} remaining={4}" -f `
            $p.PartitionNumber, $v.FileSystem, $v.FileSystemLabel, $v.Size, $v.SizeRemaining)
        if (-not $v.DriveLetter) {
            $free = [char[]](70..90) | Where-Object { -not (Test-Path "$([char]$_):\") } | Select-Object -First 1
            if ($free) {
                try {
                    Set-Partition -DiskNumber $target.Number -PartitionNumber $p.PartitionNumber -NewDriveLetter $free -ErrorAction Stop
                    $v = Get-Volume -Partition (Get-Partition -DiskNumber $target.Number -PartitionNumber $p.PartitionNumber)
                } catch { Note "  (буква не назначилась: $($_.Exception.Message))" }
            }
        }
        if ($v.DriveLetter) {
            $root = "$($v.DriveLetter):\"
            $names = @(Get-ChildItem -LiteralPath $root -Force -ErrorAction SilentlyContinue |
                       Select-Object -First 30 -ExpandProperty Name)
            Note ("  root {0} -> {1}" -f $root, ($names -join ', '))
        }
    }

    # Известные файлы системы: существуют и с какой суммой.
    $probe = @(
        'Windows\System32\ntoskrnl.exe',
        'Windows\System32\kernel32.dll',
        'Windows\System32\ntdll.dll',
        'Windows\System32\drivers\ntfs.sys',
        'Windows\explorer.exe',
        'Windows\System32\notepad.exe'
    )
    foreach ($v in (Get-Volume -ErrorAction SilentlyContinue | Where-Object { $_.DriveLetter })) {
        $root = "$($v.DriveLetter):\"
        if ("$($v.DriveLetter):" -eq $env:SystemDrive) { continue }
        if (-not (Test-Path -LiteralPath (Join-Path $root 'Windows\System32\ntoskrnl.exe'))) { continue }
        Note "LAB-RST-WINDOWS $root"
        foreach ($rel in $probe) {
            $f = Join-Path $root $rel
            if (Test-Path -LiteralPath $f) {
                $h = (Get-FileHash -LiteralPath $f -Algorithm SHA256).Hash
                $len = (Get-Item -LiteralPath $f).Length
                Note ("  file {0} len={1} sha256={2}" -f $rel, $len, $h)
            } else {
                Note "  file $rel MISSING"
            }
        }
        foreach ($rel in @('pagefile.sys', 'swapfile.sys', 'hiberfil.sys', 'DumpStack.log.tmp')) {
            $f = Join-Path $root $rel
            Note ("  excluded {0} present={1}" -f $rel, (Test-Path -LiteralPath $f))
        }
        $count = @(Get-ChildItem -LiteralPath (Join-Path $root 'Windows\System32') -File -ErrorAction SilentlyContinue).Count
        Note "  System32 files=$count"
    }

    Note 'LAB-RST-OK'
} catch {
    Note "LAB-RST-FAIL $($_.Exception.Message)"
} finally {
    if ($archRoot) {
        try {
            Set-Content -LiteralPath (Join-Path $archRoot 'restore-report.txt') `
                -Value ($report -join "`r`n") -Encoding UTF8
        } catch { }
    }
    Say 'LAB-RST-POWEROFF'
    Start-Sleep -Seconds 3
    shutdown /s /t 5
}
