function Say([string]$Text) {
    try {
        $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $p.Open(); $p.WriteLine($Text); $p.Close()
    } catch { }
}
$ErrorActionPreference = 'Stop'
$ready = 'C:\Lab\a08-stage.ready'
try {
    if (Test-Path -LiteralPath $ready) {
        Start-Process powershell.exe -ArgumentList '-NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-a08-restart.ps1' -WindowStyle Hidden | Out-Null
        return
    }
    foreach ($file in @('audit-a08-user.ps1')) {
        $errors = $null
        $null = [System.Management.Automation.Language.Parser]::ParseFile("C:\Lab\$file", [ref]$null, [ref]$errors)
        if ($errors) { throw "script parse errors: $errors" }
    }
    $staged = 'C:\Lab\own-backup-service-h02.exe'
    $want = (Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash
    if ((Get-FileHash 'C:\Lab\own-backup-service.exe' -Algorithm SHA256).Hash -ne $want) {
        $output = cmd.exe /d /c 'C:\Lab\own-backup-service-h02.exe /update 2>&1' | Out-String
        if ($LASTEXITCODE -ne 0) { throw "service update failed: $output" }
    }
    Say "A08-SERVICE sha256=$((Get-FileHash 'C:\Lab\own-backup-service.exe' -Algorithm SHA256).Hash)"
    if ((Get-Content 'C:\Lab\a08-mode.txt' -Raw).Trim() -eq 'fixed') {
        $testOutput = cmd.exe /d /c 'C:\Lab\file_restore_windows.exe --nocapture 2>&1' | Out-String
        $testCode = $LASTEXITCODE
        foreach ($line in ($testOutput -split "`r?`n")) { if ($line) { Say "A08-NATIVE-TEST $line" } }
        if ($testCode -ne 0) { throw 'Windows file-restore regressions failed' }
        Say 'A08-NATIVE-TESTS-PASS'
    }
    $target = @(Get-Disk | Where-Object { $_.Size -eq 2GB -and -not $_.IsBoot -and -not $_.IsSystem })
    if ($target.Count -ne 1 -or $target[0].Number -lt 2) { throw 'unique disposable 2-GiB target missing' }
    if ($target[0].PartitionStyle -eq 'RAW') {
        Initialize-Disk -Number $target[0].Number -PartitionStyle GPT | Out-Null
    }
    $devices = Get-Content 'C:\Lab\audit-2026-09-27\devices.json' -Raw | ConvertFrom-Json
    $source = Get-Partition -DriveLetter $devices.source_letter
    if ($source.DiskNumber -ne 1) { throw 'source is not the disposable data disk' }
    New-Item -ItemType Directory -Force -Path 'C:\Lab\a08' | Out-Null
    $image = 'C:\Lab\a08\source.qcow2'
    Say "A08-BACKUP disk=$($source.DiskNumber) part=$($source.PartitionNumber) size=$($source.Size) target=$($target[0].Number)"
    $output = cmd.exe /d /c "C:\Lab\own-backup-cli.exe backup-disk --disk 1 --partitions $($source.PartitionNumber) --output $image --format qcow2 2>&1" | Out-String
    if ($LASTEXITCODE -ne 0) { throw "backup-disk failed: $output" }
    Say "A08-IMAGE bytes=$((Get-Item $image).Length) sha256=$((Get-FileHash $image -Algorithm SHA256).Hash)"
    $cases = @('sectors_early', 'sectors_late', 'sectors_complete', 'files_early', 'files_late', 'files_complete')
    if ((Get-Content 'C:\Lab\a08-mode.txt' -Raw).Trim() -eq 'baseline') { $cases = @('sectors_early') }
    [IO.File]::WriteAllText('C:\Lab\a08\config.json', (@{
        image = $image; source_index = $source.PartitionNumber; source_size = $source.Size
        target_disk = $target[0].Number; target_size = $target[0].Size
        cases = $cases; results = 'C:\Lab\a08\results.json'
    } | ConvertTo-Json -Depth 5))
    New-ItemProperty -Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\RunOnce' `
        -Name OwnBackupA08 -PropertyType String -Force `
        -Value 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-a08-user.ps1' | Out-Null
    $answer = New-Object System.Xml.XmlDocument
    $answer.Load('C:\Lab\autounattend.xml')
    $password = $answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $winlogon DefaultUserName 'lab'
    Set-ItemProperty $winlogon DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $winlogon DefaultPassword $password
    Set-ItemProperty $winlogon AutoAdminLogon '1'
    Set-ItemProperty $winlogon AutoLogonCount 1
    [IO.File]::WriteAllText($ready, 'ready')
    Say 'A08-STAGE-READY REBOOT'
    shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "A08-STAGE-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
