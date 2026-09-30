# Disposable Windows VM: install the tested service and enter the lab console.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}
$ErrorActionPreference = 'Stop'
$ready = 'C:\Lab\h02-consolidation-stage.ready'
try {
    if (Test-Path -LiteralPath $ready) {
        $watcher = Start-Process powershell.exe -ArgumentList `
            '-NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-h02-consolidation-watch.ps1' `
            -WindowStyle Hidden -PassThru
        Say "H02-CONSOL-WATCHER pid=$($watcher.Id)"
        return
    }
    $staged = 'C:\Lab\own-backup-service-h02.exe'
    $installed = 'C:\Lab\own-backup-service.exe'
    $want = (Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash
    if ((Get-FileHash -LiteralPath $installed -Algorithm SHA256).Hash -ne $want) {
        $output = cmd.exe /d /c 'C:\Lab\own-backup-service-h02.exe /update 2>&1' | Out-String
        if ($LASTEXITCODE -ne 0) { throw "service update failed: $output" }
    }
    if ((Get-FileHash -LiteralPath $installed -Algorithm SHA256).Hash -ne $want) {
        throw 'installed service hash mismatch'
    }
    Say "H02-CONSOL-SERVICE sha256=$want"
    foreach ($name in @('watch.ready', 'stop.arm.json', 'stop.result.json', 'kill.arm.json', 'kill.result.json')) {
        Remove-Item -LiteralPath ("C:\Lab\h02-consolidation-" + $name) -ErrorAction SilentlyContinue
    }
    New-ItemProperty -Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\RunOnce' `
        -Name OwnBackupH02Consolidation -PropertyType String -Force `
        -Value 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-h02-consolidation-user.ps1' | Out-Null
    $answer = New-Object System.Xml.XmlDocument
    $answer.Load('C:\Lab\autounattend.xml')
    $password = $answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    if (-not $password) { throw 'disposable lab password missing' }
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty -Path $winlogon -Name DefaultUserName -Value 'lab'
    Set-ItemProperty -Path $winlogon -Name DefaultDomainName -Value $env:COMPUTERNAME
    Set-ItemProperty -Path $winlogon -Name DefaultPassword -Value $password
    Set-ItemProperty -Path $winlogon -Name AutoAdminLogon -Value '1'
    Set-ItemProperty -Path $winlogon -Name AutoLogonCount -Value 1
    [IO.File]::WriteAllText($ready, 'ready')
    Say 'H02-CONSOL-STAGE-READY REBOOT'
    shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "H02-CONSOL-STAGE-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
