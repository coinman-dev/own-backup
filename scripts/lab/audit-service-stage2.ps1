# Run from the existing lab VM's C:\Lab\lab-stage2-boot.ps1 startup task.
# The host must boot a disposable qcow2 overlay, never win-lab.qcow2 itself.
# The answer ISO supplies this file and the matching service/CLI binaries.

function Say {
    param([string]$Text)
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open()
        $port.WriteLine($Text)
        $port.Close()
    } catch { }
}

try {
    $ErrorActionPreference = 'Stop'
    Say "AUDIT-SERVICE-BEGIN $([DateTime]::UtcNow.ToString('o'))"

    $binary = 'C:\Lab\own-backup-service.exe'
    if (-not (Test-Path -LiteralPath $binary)) { throw 'service binary missing from lab media' }
    $hash = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash
    Say "AUDIT-SERVICE-BINARY sha256=$hash"
    $expectedFile = 'C:\Lab\expected-service.sha256'
    if (-not (Test-Path -LiteralPath $expectedFile)) { throw 'expected service hash missing from lab media' }
    $expected = (Get-Content -LiteralPath $expectedFile -Raw).Trim().ToUpperInvariant()
    if ($hash -ne $expected) {
        throw 'unexpected service binary hash'
    }
    if (Get-Service -Name OwnBackup -ErrorAction SilentlyContinue) {
        throw 'OwnBackup is already installed in the base VM; use a clean snapshot'
    }

    $installOutput = & $binary /install 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) { throw "install failed: $installOutput" }
    Say 'AUDIT-SERVICE-INSTALL ok'

    $startOutput = & $binary /start 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) { throw "start failed: $startOutput" }
    $deadline = [DateTime]::UtcNow.AddMinutes(2)
    do {
        $service = Get-CimInstance Win32_Service -Filter "Name='OwnBackup'"
        if ($service -and $service.State -eq 'Running') { break }
        Start-Sleep -Seconds 2
    } while ([DateTime]::UtcNow -lt $deadline)
    if (-not $service -or $service.State -ne 'Running') {
        throw "service did not reach Running: $($service.State)"
    }
    Say "AUDIT-SERVICE-RUNNING pid=$($service.ProcessId) path=$($service.PathName)"
    if ($service.PathName -ne $binary) { throw 'SCM is not running the staged binary' }

    $token = 'C:\ProgramData\OwnBackup\service.token'
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    while (-not (Test-Path -LiteralPath $token) -and [DateTime]::UtcNow -lt $deadline) {
        Start-Sleep -Seconds 1
    }
    if (-not (Test-Path -LiteralPath $token)) { throw 'service token missing' }
    $acl = Get-Acl -LiteralPath $token
    $sids = @($acl.Access | ForEach-Object {
        $_.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]).Value
    })
    Say "AUDIT-SERVICE-TOKEN bytes=$((Get-Item -LiteralPath $token).Length) protected=$($acl.AreAccessRulesProtected) sids=$($sids -join ',')"
    if (-not $acl.AreAccessRulesProtected -or $sids -contains 'S-1-5-32-545') {
        throw 'token DACL is inherited or grants Users access'
    }
    if ($sids -notcontains 'S-1-5-18' -or $sids -notcontains 'S-1-5-4') {
        throw 'token DACL lacks SYSTEM or INTERACTIVE'
    }

    $db = 'C:\ProgramData\OwnBackup\metadata.db'
    if (Test-Path -LiteralPath $db) {
        $dbAcl = Get-Acl -LiteralPath $db
        $dbSids = @($dbAcl.Access | ForEach-Object {
            $_.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]).Value
        })
        Say "AUDIT-SERVICE-METADATA-ACL users_read=$($dbSids -contains 'S-1-5-32-545')"
    }
    Say 'AUDIT-SERVICE-PASS'
} catch {
    Say "AUDIT-SERVICE-FAIL $($_.Exception.Message)"
} finally {
    Say 'AUDIT-SERVICE-DONE'
    Start-Sleep -Seconds 2
    shutdown /s /t 5
}
