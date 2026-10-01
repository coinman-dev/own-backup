# SYSTEM startup: prepare an ordinary user's GUI probe on a disposable overlay.
function Say([string]$Text) {
    try {
        $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $p.Open(); $p.WriteLine($Text); $p.Close()
    } catch { }
}
$ErrorActionPreference = 'Stop'
$ready = 'C:\Lab\audit-a02-gui-probe.ready'
try {
    if (Test-Path -LiteralPath $ready) { Say 'A02-GUI-STAGE-SECOND-BOOT'; return }
    $staged = 'C:\Lab\own-backup-service-a02.exe'
    $want = (Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash
    $installed = (Get-FileHash -LiteralPath 'C:\Lab\own-backup-service.exe' -Algorithm SHA256).Hash
    if ($installed -ne $want) {
        $output = & $staged /update 2>&1 | Out-String
        if ($LASTEXITCODE -ne 0) { throw "service update failed: $output" }
    }
    Say "A02-GUI-SERVICE sha256=$((Get-FileHash -LiteralPath 'C:\Lab\own-backup-service.exe' -Algorithm SHA256).Hash)"
    if (-not (Test-Path -LiteralPath 'C:\Lab\own-backup-gui.exe')) { throw 'GUI binary missing' }
    $reg = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
    $version = [string](Get-ItemProperty -Path $reg -Name pv -ErrorAction SilentlyContinue).pv
    if (-not $version -or $version -eq '0.0.0.0') {
        $installer = 'C:\Lab\WebView2Runtime-x64.exe'
        if ((Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash -ne
            'FA07BC67C2A68763E127F6AABF5622B40A0220228899E78DAF02CE2CC3971A50') {
            throw 'offline WebView2 installer hash differs'
        }
        if ((Get-AuthenticodeSignature -LiteralPath $installer).Status -ne 'Valid') {
            throw 'offline WebView2 installer signature invalid'
        }
        $install = Start-Process -FilePath $installer -ArgumentList @('/silent', '/install') -PassThru -Wait
        if ($install.ExitCode -ne 0) { throw "WebView2 install exit=$($install.ExitCode)" }
        $version = [string](Get-ItemProperty -Path $reg -Name pv -ErrorAction SilentlyContinue).pv
    }
    Say "A02-GUI-WEBVIEW version=$version"
    if (-not $version -or $version -eq '0.0.0.0') { throw 'WebView2 runtime missing' }
    $admins = ([Security.Principal.SecurityIdentifier]'S-1-5-32-544').Translate(
        [Security.Principal.NTAccount]).Value.Split('\')[-1]
    $isAdmin = @(Get-LocalGroupMember -Group $admins | Where-Object { $_.Name -like '*\lab' }).Count -gt 0
    if ($isAdmin) { Remove-LocalGroupMember -Group $admins -Member "$env:COMPUTERNAME\lab" -ErrorAction Stop }
    Say "A02-GUI-LAB admin=$(@(Get-LocalGroupMember -Group $admins | Where-Object { $_.Name -like '*\lab' }).Count -gt 0)"
    $startup = [Environment]::GetFolderPath('CommonStartup')
    foreach ($name in @('OwnBackupAudit29.cmd', 'OwnBackupA13Single.cmd')) {
        Remove-Item -LiteralPath (Join-Path $startup $name) -Force -ErrorAction SilentlyContinue
    }
    [IO.File]::WriteAllText((Join-Path $startup 'OwnBackupA02GuiProbe.cmd'),
        "@echo off`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-a02-gui-probe-user.ps1`r`n")
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
    Say 'A02-GUI-STAGE-READY REBOOT'
    shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "A02-GUI-STAGE-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
