# Active-console setup of the saved password, followed later by a read-only
# content check of the backup created with nobody logged on.
function Say([string]$Text) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

function RunNative([string]$Command) {
    $output = (cmd.exe /d /c "$Command 2>&1" | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) { throw "exit=$LASTEXITCODE command=$Command output=$output" }
    return $output
}

function InvokeAuditCli([string[]]$Arguments) {
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = (& 'C:\Lab\own-backup-cli.exe' @Arguments 2>&1 | Out-String)
        [pscustomobject]@{ Code = [int]$LASTEXITCODE; Output = [string]$output }
    } finally { $ErrorActionPreference = $old }
}

function Need([string]$Label, [string[]]$Arguments) {
    $answer = InvokeAuditCli -Arguments $Arguments
    Say "H01-NIGHT-$Label exit=$($answer.Code)"
    if ($answer.Code -ne 0) { throw "$Label failed: $($answer.Output.Trim())" }
    return $answer.Output
}

$ErrorActionPreference = 'Stop'
$ready = Join-Path $env:LOCALAPPDATA 'audit-h01-unattended.ready'
$imageReady = Join-Path $env:LOCALAPPDATA 'audit-h01-unattended-image-ready'
$contentDone = Join-Path $env:LOCALAPPDATA 'audit-h01-unattended-content-done'
try {
    if (Test-Path -LiteralPath $contentDone) { return }
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    $admin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    Say "H01-NIGHT-USER name=$($identity.Name) session=$([Diagnostics.Process]::GetCurrentProcess().SessionId) admin=$admin"
    if ($admin) { throw 'lab user is not a standard user' }

    if (Test-Path -LiteralPath $imageReady) {
        $image = [IO.File]::ReadAllText((Join-Path $env:LOCALAPPDATA 'audit-h01-unattended-image.txt'))
        $mount = Need 'MOUNT' @('mount', $image)
        try {
            $match = [regex]::Match($mount, 'PhysicalDrive(\d+)')
            if (-not $match.Success) { throw 'mounted disk number absent' }
            $diskNumber = [int]$match.Groups[1].Value
            Start-Sleep -Seconds 3
            $letters = @(Get-Partition -DiskNumber $diskNumber -ErrorAction Stop |
                Where-Object { $_.DriveLetter } | ForEach-Object { $_.DriveLetter })
            $found = @($letters | ForEach-Object { "${_}:\h01-control.bin" } |
                Where-Object { Test-Path -LiteralPath $_ })
            if ($found.Count -ne 1) { throw "control file missing in image; letters=$($letters -join ',')" }
            $actual = (Get-FileHash -LiteralPath $found[0] -Algorithm SHA256).Hash
            $expected = [IO.File]::ReadAllText('C:\Lab\audit-h01-control.sha256')
            Say "H01-NIGHT-CONTENT expected=$expected actual=$actual"
            if ($actual -ne $expected) { throw 'backup content hash mismatch' }
        } finally {
            $null = Need 'UNMOUNT' @('unmount', $image)
        }
        [IO.File]::WriteAllText($contentDone, 'pass')
        Say 'H01-NIGHT-PASS UNATTENDED-BACKUP-CONTENT'
        shutdown.exe /s /t 5 | Out-Null
        return
    }
    if (Test-Path -LiteralPath $ready) { Say 'H01-NIGHT-WAITING-FOR-BACKUP'; return }

    for ($tick = 0; $tick -lt 30; $tick++) {
        $status = InvokeAuditCli -Arguments @('jobs', 'status')
        if ($status.Code -eq 0 -and $status.Output.Contains('uptime_secs')) { break }
        Start-Sleep -Seconds 2
    }
    if ($status.Code -ne 0 -or -not $status.Output.Contains('uptime_secs')) {
        throw "service RPC unavailable: $($status.Output.Trim())"
    }
    $volume = [IO.File]::ReadAllText('C:\Lab\audit-h01-unattended.setup')
    Say "H01-NIGHT-ACTIVE-CONSOLE volume=$volume"
    Say (RunNative "C:\Lab\audit_h01_unattended.exe prepare $volume")
    Say (RunNative 'C:\Lab\audit_h01_unattended.exe arm')
    [IO.File]::WriteAllText($ready, $volume)
    Say 'H01-NIGHT-PASSWORD-SAVED REBOOT-NO-USER'
    shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "H01-NIGHT-USER-FAIL id=$($_.FullyQualifiedErrorId) line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
}
