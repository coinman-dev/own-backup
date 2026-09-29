# Runs as SYSTEM at startup in a disposable Windows QEMU overlay.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

$ErrorActionPreference = 'Stop'
$ready = 'C:\Lab\audit-a02-stage-v4.ready'
if (Test-Path -LiteralPath $ready) { Say 'A02-STAGE-SECOND-BOOT'; return }
try {
    Say "A02-STAGE-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    $staged = 'C:\Lab\own-backup-service-a02.exe'
    $want = (Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash
    $output = (& $staged /update 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) { throw "service update failed: $output" }
    $have = (Get-FileHash 'C:\Lab\own-backup-service.exe' -Algorithm SHA256).Hash
    Say "A02-STAGE-SERVICE sha256=$have"
    if ($have -ne $want) { throw 'installed service differs from staged binary' }
    Say "A02-STAGE-CLI sha256=$((Get-FileHash 'C:\Lab\own-backup-cli.exe' -Algorithm SHA256).Hash)"

    $answer = New-Object System.Xml.XmlDocument
    $answer.Load('C:\Lab\autounattend.xml')
    $password = $answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty -Path $winlogon -Name DefaultUserName -Value 'lab'
    Set-ItemProperty -Path $winlogon -Name DefaultDomainName -Value $env:COMPUTERNAME
    Set-ItemProperty -Path $winlogon -Name DefaultPassword -Value $password
    Set-ItemProperty -Path $winlogon -Name AutoAdminLogon -Value '1'
    Set-ItemProperty -Path $winlogon -Name AutoLogonCount -Value 1
    New-ItemProperty -Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\RunOnce' `
        -Name OwnBackupAuditA02 -PropertyType String -Force `
        -Value 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-a02-user.ps1' | Out-Null
    [IO.File]::WriteAllText($ready, 'ready')
    Say 'A02-STAGE-READY REBOOT'
    shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "A02-STAGE-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
