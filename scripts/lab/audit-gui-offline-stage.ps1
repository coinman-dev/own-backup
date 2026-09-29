# Runs as SYSTEM from the disposable Windows lab's startup task.
# The QEMU guest has no network adapter. The WebView2 standalone installer is
# supplied on the answer CD, so Windows Update cannot stall this GUI check.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

$ErrorActionPreference = 'Stop'
$ready = 'C:\Lab\audit-gui-offline.ready'
try {
    if (Test-Path -LiteralPath $ready) {
        Say 'AUDIT29-OFFLINE-SECOND-BOOT'
        return
    }
    Say "AUDIT29-OFFLINE-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    $installer = Get-PSDrive -PSProvider FileSystem |
        ForEach-Object { Join-Path $_.Root 'MicrosoftEdgeWebView2RuntimeInstallerX64.exe' } |
        Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
    if (-not $installer) { throw 'offline WebView2 installer missing from answer CD' }
    $installerHash = (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash
    Say "AUDIT29-OFFLINE-INSTALLER sha256=$installerHash"
    if ($installerHash -ne 'FA07BC67C2A68763E127F6AABF5622B40A0220228899E78DAF02CE2CC3971A50') {
        throw 'installer hash differs from Microsoft winget manifest'
    }
    $signature = Get-AuthenticodeSignature -LiteralPath $installer
    Say "AUDIT29-OFFLINE-SIGNATURE status=$($signature.Status) signer=$($signature.SignerCertificate.Subject)"
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'Microsoft') {
        throw 'installer signature is not valid for Microsoft'
    }
    $reg = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
    $version = [string](Get-ItemProperty -Path $reg -Name pv -ErrorAction SilentlyContinue).pv
    if (-not $version -or $version -eq '0.0.0.0') {
        $process = Start-Process -FilePath $installer -ArgumentList @('/silent', '/install') -PassThru -Wait
        Say "AUDIT29-OFFLINE-INSTALL exit=$($process.ExitCode)"
        if ($process.ExitCode -ne 0) { throw 'offline WebView2 installation failed' }
        $version = [string](Get-ItemProperty -Path $reg -Name pv -ErrorAction SilentlyContinue).pv
    }
    Say "AUDIT29-OFFLINE-WEBVIEW version=$version"
    if (-not $version -or $version -eq '0.0.0.0') { throw 'installed WebView2 Runtime not detected' }

    $staged = 'C:\Lab\own-backup-service-audit29.exe'
    $want = (Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash
    $service = Get-Service -Name 'OwnBackup' -ErrorAction SilentlyContinue
    if (-not $service) {
        Copy-Item -LiteralPath $staged -Destination 'C:\Lab\own-backup-service.exe' -Force
        $install = & 'C:\Lab\own-backup-service.exe' /install 2>&1 | Out-String
        if ($LASTEXITCODE -ne 0) { throw "service install failed: $install" }
        $start = & 'C:\Lab\own-backup-service.exe' /start 2>&1 | Out-String
        if ($LASTEXITCODE -ne 0) { throw "service start failed: $start" }
    } else {
        $have = (Get-FileHash -LiteralPath 'C:\Lab\own-backup-service.exe' -Algorithm SHA256).Hash
    }
    if ($service -and $have -ne $want) {
        $update = & $staged /update 2>&1 | Out-String
        if ($LASTEXITCODE -ne 0) { throw "service update failed: $update" }
    }
    $have = (Get-FileHash -LiteralPath 'C:\Lab\own-backup-service.exe' -Algorithm SHA256).Hash
    Say "AUDIT29-OFFLINE-SERVICE sha256=$have"
    if ($have -ne $want) { throw 'installed service hash differs from staged build' }

    $admins = ([Security.Principal.SecurityIdentifier]'S-1-5-32-544').Translate(
        [Security.Principal.NTAccount]).Value.Split('\')[-1]
    Remove-LocalGroupMember -Group $admins -Member "$env:COMPUTERNAME\lab" -ErrorAction Stop
    $stillAdmin = @(Get-LocalGroupMember -Group $admins | Where-Object { $_.Name -like '*\lab' }).Count -gt 0
    Say "AUDIT29-OFFLINE-LAB-STANDARD admin=$stillAdmin"
    if ($stillAdmin) { throw 'lab user still belongs to Administrators' }

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
    [IO.File]::WriteAllText((Join-Path $startup 'OwnBackupAudit29.cmd'),
        "@echo off`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-gui-offline-user.ps1`r`n")
    [IO.File]::WriteAllText($ready, 'ready')
    Say 'AUDIT29-OFFLINE-READY REBOOT'
    shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "AUDIT29-OFFLINE-FAIL id=$($_.FullyQualifiedErrorId) line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
}
