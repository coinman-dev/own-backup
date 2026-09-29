# SYSTEM startup probe on a disposable overlay that already has OwnBackup
# installed. Seeds one legacy DPAPI blob, updates the service, and checks the
# migration and second-user access without putting a password in the log.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

function RunNative([string]$Command) {
    $output = (cmd.exe /d /c "$Command 2>&1" | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) { throw "native exit=$LASTEXITCODE command=$Command output=$output" }
    return $output
}

function SayServiceLog {
    $file = Get-ChildItem 'C:\Lab\Logs' -Filter 'own-backup-service-*.log' `
        -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending |
        Select-Object -First 1
    if (-not $file) { Say 'H01-SERVICE-LOG missing'; return }
    Say "H01-SERVICE-LOG file=$($file.FullName) bytes=$($file.Length)"
    foreach ($line in Get-Content -LiteralPath $file.FullName | Select-Object -Last 20) {
        $plain = $line -replace '[^\x20-\x7E]', '?'
        Say "H01-SERVICE-LINE $($plain.Substring(0, [Math]::Min(450, $plain.Length)))"
    }
}

$ErrorActionPreference = 'Stop'
$ready = 'C:\Lab\audit-h01.ready'
$probe = 'C:\Lab\audit_h01.exe'
try {
    if (Test-Path -LiteralPath $ready) {
        Say 'H01-SECOND-BOOT'
        for ($tick = 0; $tick -lt 30; $tick++) {
            if ((Get-Service -Name OwnBackup).Status -eq 'Running') { break }
            Start-Sleep -Seconds 2
        }
        if ((Get-Service -Name OwnBackup).Status -ne 'Running') { throw 'service failed after cold boot' }
        Say (RunNative "$probe check")
        Say (RunNative "$probe write-check")
        Say (RunNative "$probe acl-check")
        Say 'H01-COLD-BOOT-PASS'
        SayServiceLog
        return
    }
    Say "H01-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    $installed = 'C:\Lab\own-backup-service.exe'
    $staged = 'C:\Lab\own-backup-service-h01.exe'
    RunNative "$installed /stop" | Out-Null
    Say (RunNative "$probe seed")
    RunNative "$staged /update" | Out-Null
    $hash = (Get-FileHash -LiteralPath $installed -Algorithm SHA256).Hash
    Say "H01-SERVICE sha256=$hash"
    if ($hash -ne 'BB06982EBE7B68130E7326F204F1A1F54B92228310DB682AF2B4AE17DD116903') {
        throw 'installed service differs from migration build'
    }
    for ($tick = 0; $tick -lt 30; $tick++) {
        if ((Get-Service -Name OwnBackup).Status -eq 'Running') { break }
        Start-Sleep -Seconds 2
    }
    if ((Get-Service -Name OwnBackup).Status -ne 'Running') { throw 'service did not start' }
    Say (RunNative "$probe check")
    Say (RunNative "$probe write-check")

    $answer = New-Object System.Xml.XmlDocument
    $answer.Load('C:\Lab\autounattend.xml')
    $password = $answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    $securePassword = ConvertTo-SecureString $password -AsPlainText -Force
    if (-not (Get-LocalUser -Name lab2 -ErrorAction SilentlyContinue)) {
        New-LocalUser -Name lab2 -Password $securePassword | Out-Null
    }
    Say (RunNative "$probe acl-check")

    RunNative "$installed /restart" | Out-Null
    for ($tick = 0; $tick -lt 30; $tick++) {
        if ((Get-Service -Name OwnBackup).Status -eq 'Running') { break }
        Start-Sleep -Seconds 2
    }
    if ((Get-Service -Name OwnBackup).Status -ne 'Running') { throw 'service failed after restart' }
    Say (RunNative "$probe check")
    [IO.File]::WriteAllText($ready, 'pass')
    Say 'H01-PASS MIGRATION-ACL-RESTART'
    SayServiceLog
} catch {
    Say "H01-FAIL id=$($_.FullyQualifiedErrorId) line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
}
