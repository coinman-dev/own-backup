# SYSTEM bootstrap in a fresh overlay of the saved Windows audit VM.
function Say([string]$Text) {
    try {
        $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $p.Open(); $p.WriteLine($Text); $p.Close()
    } catch { }
}
$ErrorActionPreference = 'Stop'
try {
    $ready = 'C:\Lab\a05-stage.ready'
    if (Test-Path $ready) {
        Start-Process powershell.exe -ArgumentList '-NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-a05-watch.ps1' -WindowStyle Hidden | Out-Null
        Say 'A05-STAGE-SECOND-BOOT'
        return
    }
    Say "A05-STAGE-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    foreach ($script in @('audit-a05-user.ps1', 'audit-a05-watch.ps1')) {
        $errors = $null
        $null = [Management.Automation.Language.Parser]::ParseFile("C:\Lab\$script", [ref]$null, [ref]$errors)
        if ($errors) { throw "script syntax invalid: $script $errors" }
    }
    $output = & 'C:\Lab\a05_service_windows_tests.exe' --exact tests::service_token_dacl_is_protected_before_first_write --nocapture 2>&1 | Out-String
    $code = $LASTEXITCODE
    [IO.File]::WriteAllText('C:\Lab\a05-token-test.log', $output)
    foreach ($line in ($output -split "`r?`n")) { if ($line) { Say "A05-TOKEN-TEST $line" } }
    if ($code -ne 0 -or $output -notmatch '1 passed' -or $output -notmatch 'bytes=0 protected=true inherited=false') {
        throw 'native token creation test failed or did not run'
    }
    $staged = 'C:\Lab\own-backup-service-a05.exe'
    $output = & $staged /update 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) { throw "service update failed: $output" }
    $wanted = (Get-FileHash $staged -Algorithm SHA256).Hash
    if ((Get-FileHash 'C:\Lab\own-backup-service.exe' -Algorithm SHA256).Hash -ne $wanted) { throw 'SCM binary hash differs' }
    Say "A05-SERVICE sha256=$wanted state=$((Get-Service OwnBackup).Status)"
    $admins = ([Security.Principal.SecurityIdentifier]'S-1-5-32-544').Translate([Security.Principal.NTAccount]).Value.Split('\')[-1]
    if (@(Get-LocalGroupMember $admins | Where-Object { $_.Name -like '*\lab' }).Count) {
        Remove-LocalGroupMember -Group $admins -Member "$env:COMPUTERNAME\lab"
    }
    # This credential exists only in this disposable VM.
    $password = ConvertTo-SecureString 'A05-Disposable-20261003!' -AsPlainText -Force
    if (-not (Get-LocalUser AuditA05Rdp -ErrorAction SilentlyContinue)) {
        New-LocalUser -Name AuditA05Rdp -Password $password -PasswordNeverExpires -AccountNeverExpires | Out-Null
    }
    $users = ([Security.Principal.SecurityIdentifier]'S-1-5-32-545').Translate([Security.Principal.NTAccount]).Value.Split('\')[-1]
    $remote = ([Security.Principal.SecurityIdentifier]'S-1-5-32-555').Translate([Security.Principal.NTAccount]).Value.Split('\')[-1]
    Add-LocalGroupMember -Group $users -Member "$env:COMPUTERNAME\AuditA05Rdp" -ErrorAction SilentlyContinue
    Add-LocalGroupMember -Group $remote -Member "$env:COMPUTERNAME\AuditA05Rdp" -ErrorAction SilentlyContinue
    Set-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Terminal Server' fDenyTSConnections 0
    Set-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations\RDP-Tcp' UserAuthentication 1
    Set-Service TermService -StartupType Automatic
    Start-Service TermService
    New-NetFirewallRule -DisplayName 'Disposable A05 RDP' -Direction Inbound -Action Allow -Protocol TCP -LocalPort 3389 | Out-Null
    New-Item -ItemType Directory -Force 'C:\Lab\a05-results' | Out-Null
    $null = & icacls.exe 'C:\Lab\a05-results' /grant '*S-1-1-0:(OI)(CI)M'
    if ($LASTEXITCODE -ne 0) { throw 'could not grant fixture result directory access' }
    $startup = [Environment]::GetFolderPath('CommonStartup')
    foreach ($name in @('OwnBackupAudit29.cmd', 'OwnBackupA13Single.cmd', 'OwnBackupA02GuiProbe.cmd')) {
        Remove-Item (Join-Path $startup $name) -Force -ErrorAction SilentlyContinue
    }
    [IO.File]::WriteAllText((Join-Path $startup 'OwnBackupA05.cmd'),
        "@echo off`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\Lab\audit-a05-user.ps1`r`n")
    $answer = New-Object System.Xml.XmlDocument
    $answer.Load('C:\Lab\autounattend.xml')
    $labPassword = $answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $winlogon DefaultUserName 'lab'
    Set-ItemProperty $winlogon DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $winlogon DefaultPassword $labPassword
    Set-ItemProperty $winlogon AutoAdminLogon '1'
    Set-ItemProperty $winlogon AutoLogonCount 1
    [IO.File]::WriteAllText($ready, 'ready')
    Say 'A05-STAGE-READY REBOOT'
    shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "A05-STAGE-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
