# SYSTEM startup script on a disposable Windows VM. Update the service on the
# first boot; arm the watcher on the second while the lab user runs the client.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

$ErrorActionPreference = 'Stop'
$action = 'stop'
if (Test-Path -LiteralPath 'C:\Lab\audit-h02-verify.action') {
    $action = (Get-Content -LiteralPath 'C:\Lab\audit-h02-verify.action' -Raw).Trim()
}
if ($action -notin @('stop', 'update')) { throw 'invalid H02 action' }
$ready = "C:\Lab\audit-h02-verify-stream-shutdown-$action-stage.ready"
try {
    if (Test-Path -LiteralPath $ready) {
        $watcher = Start-Process powershell.exe -ArgumentList `
            '-NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-h02-verify-watch.ps1' `
            -WindowStyle Hidden -PassThru
        Say "H02-VERIFY-WATCHER pid=$($watcher.Id)"
        return
    }
    $staged = 'C:\Lab\own-backup-service-h02.exe'
    $installed = 'C:\Lab\own-backup-service.exe'
    $want = (Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash
    $have = (Get-FileHash -LiteralPath $installed -Algorithm SHA256).Hash
    if ($have -ne $want) {
        $output = (& $staged /update 2>&1 | Out-String).Trim()
        if ($LASTEXITCODE -ne 0) { throw "service update failed: $output" }
        $have = (Get-FileHash -LiteralPath $installed -Algorithm SHA256).Hash
        if ($have -ne $want) { throw 'installed service hash differs from staged build' }
    }
    Say "H02-VERIFY-SERVICE sha256=$have"
    Remove-Item -LiteralPath 'C:\Lab\audit-h02-verify-watch.ready' -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath 'C:\Lab\audit-h02-verify.done' -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath 'C:\Lab\audit-h02-verify-watch.result.json' -ErrorAction SilentlyContinue
    $image = Get-ChildItem 'C:\Lab\audit-2026-09-27\a07-images' -Filter '*.qcow2' -File |
        Sort-Object Length -Descending | Select-Object -First 1
    if (-not $image -or $image.Length -lt 500MB) { throw 'large A07 image missing' }
    New-ItemProperty -Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\RunOnce' `
        -Name OwnBackupH02Verify -PropertyType String -Force `
        -Value 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-h02-verify-user.ps1' | Out-Null
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
    Say 'H02-VERIFY-STAGE-READY REBOOT'
    shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "H02-VERIFY-STAGE-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
