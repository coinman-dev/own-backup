# Runs as SYSTEM in a fresh disposable overlay of the original lab image.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

$ErrorActionPreference = 'Stop'
$ready = 'C:\Lab\audit-gui-no-webview.ready'
try {
    if (Test-Path -LiteralPath $ready) { Say 'AUDIT29-NOWEBVIEW-SECOND-BOOT'; return }
    $reg = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
    $version = [string](Get-ItemProperty -Path $reg -Name pv -ErrorAction SilentlyContinue).pv
    Say "AUDIT29-NOWEBVIEW-RUNTIME version=$version"
    if ($version -and $version -ne '0.0.0.0') { throw 'base VM already has WebView2' }
    $answer = New-Object System.Xml.XmlDocument
    $answer.Load('C:\Lab\autounattend.xml')
    $password = $answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty -Path $winlogon -Name DefaultUserName -Value 'lab'
    Set-ItemProperty -Path $winlogon -Name DefaultDomainName -Value $env:COMPUTERNAME
    Set-ItemProperty -Path $winlogon -Name DefaultPassword -Value $password
    Set-ItemProperty -Path $winlogon -Name AutoAdminLogon -Value '1'
    Set-ItemProperty -Path $winlogon -Name AutoLogonCount -Value 1
    $startup = [Environment]::GetFolderPath('CommonStartup')
    [IO.File]::WriteAllText((Join-Path $startup 'OwnBackupNoWebviewAudit.cmd'),
        "@echo off`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-gui-no-webview-user.ps1`r`n")
    [IO.File]::WriteAllText($ready, 'ready')
    Say 'AUDIT29-NOWEBVIEW-READY REBOOT'
    shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "AUDIT29-NOWEBVIEW-STAGE-FAIL $($_.Exception.Message)"
}
