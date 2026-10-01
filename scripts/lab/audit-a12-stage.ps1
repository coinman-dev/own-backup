# Disposable VM: a real SMB share and two different local NTFS partitions.
function Say([string]$Text) {
    try { $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'; $p.Open(); $p.WriteLine($Text); $p.Close() } catch { }
}
$ErrorActionPreference = 'Stop'
$ready = 'C:\Lab\a12-stage.ready'
try {
    if (Test-Path -LiteralPath $ready) {
        $watcher = Start-Process powershell.exe -ArgumentList '-NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-a12-watch.ps1' -WindowStyle Hidden -PassThru
        Say "A12-WATCHER pid=$($watcher.Id)"
        return
    }
    foreach ($name in @('audit-a12-user.ps1', 'audit-a12-watch.ps1', 'audit-a12-threads.ps1')) {
        $errors = $null
        $null = [System.Management.Automation.Language.Parser]::ParseFile("C:\Lab\$name", [ref]$null, [ref]$errors)
        if ($errors) { throw "script parse errors in $name : $errors" }
    }
    $want = (Get-FileHash 'C:\Lab\own-backup-service-a12.exe' -Algorithm SHA256).Hash
    if ((Get-FileHash 'C:\Lab\own-backup-service.exe' -Algorithm SHA256).Hash -ne $want) {
        $output = cmd.exe /d /c 'C:\Lab\own-backup-service-a12.exe /update 2>&1' | Out-String
        if ($LASTEXITCODE -ne 0) { throw "service update failed: $output" }
    }
    Say "A12-SERVICE sha256=$want"
    New-Item -ItemType Directory -Force -Path 'C:\Lab\a12-share', 'C:\Lab\a12' | Out-Null
    $config = 'C:\ProgramData\OwnBackup\config.toml'
    $settings = $(if (Test-Path -LiteralPath $config) { [IO.File]::ReadAllText($config) } else { '' })
    if ($settings -match '(?m)^log_level\s*=') {
        $settings = [regex]::Replace($settings, '(?m)^log_level\s*=\s*"[^"]*"', 'log_level = "debug"')
    } else { $settings = "log_level = `"debug`"`r`n" + $settings }
    [IO.File]::WriteAllText($config, $settings)
    if ((Get-Service LanmanServer).Status -ne 'Running') { Start-Service LanmanServer }
    $everyone = ([System.Security.Principal.SecurityIdentifier]::new('S-1-1-0')).Translate([System.Security.Principal.NTAccount]).Value
    $acl = (& icacls.exe 'C:\Lab\a12-share' /grant '*S-1-1-0:(OI)(CI)F' 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) { throw "share directory ACL failed: $acl" }
    if (-not (Get-SmbShare -Name AuditA12 -ErrorAction SilentlyContinue)) {
        New-SmbShare -Name AuditA12 -Path 'C:\Lab\a12-share' -FullAccess $everyone | Out-Null
    }
    $unc = '\\localhost\AuditA12'
    $readable = $false
    for ($i=0; $i -lt 15; $i++) {
        if (Test-Path -LiteralPath $unc) { $readable = $true; break }
        Start-Sleep -Seconds 1
    }
    if (-not $readable) { throw 'SYSTEM cannot read its test SMB share' }
    [IO.File]::WriteAllText((Join-Path $unc 'auth-probe.txt'), 'a12')
    Say 'A12-SMB-PREFLIGHT writable_by_SYSTEM=True'
    $data = @(Get-Partition -DiskNumber 1 | Where-Object {
        $_.Size -gt 500MB -and $_.Size -lt 900MB -and -not $_.DriveLetter
    } | Sort-Object PartitionNumber)
    if ($data.Count -ne 2 -or (Get-Partition -DriveLetter R -ErrorAction SilentlyContinue)) {
        throw 'two letterless data partitions and a free R: are required'
    }
    [IO.File]::WriteAllText('C:\Lab\a12\parts.json', (@{
        source = "part:1:$($data[0].Offset):$($data[0].Size)"
        destination_part = $data[1].PartitionNumber
        unc = $unc
    } | ConvertTo-Json))
    New-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\RunOnce' `
        -Name OwnBackupA12 -PropertyType String -Force `
        -Value 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-a12-user.ps1' | Out-Null
    $answer = New-Object System.Xml.XmlDocument
    $answer.Load('C:\Lab\autounattend.xml')
    $password = $answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    if (-not $password) { throw 'disposable lab password missing' }
    $w = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $w DefaultUserName 'lab'; Set-ItemProperty $w DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $w DefaultPassword $password; Set-ItemProperty $w AutoAdminLogon '1'; Set-ItemProperty $w AutoLogonCount 1
    [IO.File]::WriteAllText($ready, 'ready')
    Say 'A12-STAGE-READY REBOOT'
    shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "A12-STAGE-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
