# Run inside a disposable Windows VM, from an Administrator PowerShell.
# This registers only OwnBackupSealProbe; it does not run the backup engine.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('Install', 'Verify', 'Remove')]
    [string]$Action,
    [string]$BinaryPath
)

$ErrorActionPreference = 'Stop'
$probeName = 'OwnBackupSealProbe'
$probeRoot = Join-Path $env:ProgramData 'OwnBackup-SealProbe-Lab'
$probeMarker = 'OwnBackup E2 disposable laboratory probe'
$principal = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this test inside the test VM from an Administrator PowerShell.'
}

if ($Action -eq 'Install') {
    if (Get-Service -Name $probeName -ErrorAction SilentlyContinue) {
        throw 'The probe service already exists; keep its evidence or remove it first.'
    }
    if (Test-Path -LiteralPath $probeRoot) {
        throw "Probe directory already exists: $probeRoot. Choose a fresh test VM or preserve/rename the old evidence manually."
    }
    if (-not $BinaryPath -or -not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) {
        throw 'Supply -BinaryPath pointing to the built seal_store_service.exe example.'
    }
    New-Item -ItemType Directory -Path $probeRoot | Out-Null
    $security = [Security.AccessControl.DirectorySecurity]::new()
    $security.SetAccessRuleProtection($true, $false)
    $inherit = [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
    foreach ($sid in @('S-1-5-18', 'S-1-5-32-544')) {
        $rule = [Security.AccessControl.FileSystemAccessRule]::new(
            [Security.Principal.SecurityIdentifier]::new($sid),
            [Security.AccessControl.FileSystemRights]::FullControl,
            $inherit, [Security.AccessControl.PropagationFlags]::None,
            [Security.AccessControl.AccessControlType]::Allow)
        $security.AddAccessRule($rule)
    }
    Set-Acl -LiteralPath $probeRoot -AclObject $security
    [IO.File]::WriteAllText((Join-Path $probeRoot 'lab-probe.marker'), $probeMarker)
    $binary = Join-Path $probeRoot 'seal_store_service.exe'
    Copy-Item -LiteralPath $BinaryPath -Destination $binary
    $os = Get-CimInstance Win32_OperatingSystem
    @{
        boot_time = $os.LastBootUpTime.ToUniversalTime().ToString('o')
        installed_at = [DateTime]::UtcNow.ToString('o')
    } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $probeRoot 'baseline.json') -Encoding UTF8
    New-Service -Name $probeName -DisplayName 'Own Backup disposable encryption probe' -StartupType Automatic -BinaryPathName ('"{0}"' -f $binary) | Out-Null
    Start-Service -Name $probeName
    $deadline = [DateTime]::UtcNow.AddMinutes(1)
    $reports = Join-Path $probeRoot 'reports.jsonl'
    while (-not (Test-Path -LiteralPath $reports)) {
        if ([DateTime]::UtcNow -ge $deadline) { throw 'Probe did not write its first report; inspect the service event log.' }
        Start-Sleep -Milliseconds 200
    }
    $first = Get-Content -LiteralPath $reports | Select-Object -First 1 | ConvertFrom-Json
    if (-not $first.ok -or $first.phase -ne 'created-password-discarded') {
        throw "Initial probe failed: $($first.error)"
    }
    Write-Output "Initial LocalSystem probe passed. Evidence: $probeRoot"
    Write-Output 'Shut down and start this test VM. Wait for automatic service startup before logging in; then run -Action Verify.'
}
elseif ($Action -eq 'Verify') {
    if ([IO.File]::ReadAllText((Join-Path $probeRoot 'lab-probe.marker')) -ne $probeMarker) {
        throw 'Unexpected probe directory.'
    }
    $baseline = Get-Content -LiteralPath (Join-Path $probeRoot 'baseline.json') -Raw | ConvertFrom-Json
    $boot = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime()
    if ($boot -le [DateTime]::Parse($baseline.boot_time).ToUniversalTime()) {
        throw 'The VM has not restarted since installation; a service restart alone is not the cold-start test.'
    }
    $reports = @(Get-Content -LiteralPath (Join-Path $probeRoot 'reports.jsonl') | ForEach-Object { $_ | ConvertFrom-Json })
    $afterBoot = @($reports | Where-Object { [DateTime]::Parse($_.at).ToUniversalTime() -ge $boot })
    if ($afterBoot.Count -eq 0) { throw 'No automatic service report after this boot.' }
    $last = $afterBoot[-1]
    if (-not $last.ok -or $last.phase -ne 'reopened-without-password') {
        throw "Cold-start probe failed: $($last.error)"
    }
    Write-Output 'PASS: LocalSystem reopened the stored key after VM reboot without a saved password.'
    Write-Output "Evidence: $probeRoot"
}
else {
    $service = Get-Service -Name $probeName -ErrorAction SilentlyContinue
    if ($service) {
        if ($service.Status -ne 'Stopped') { throw 'Wait for the short probe to finish before removing it.' }
        & sc.exe delete $probeName
        if ($LASTEXITCODE -ne 0) { throw 'Could not unregister the probe service.' }
    }
    Write-Output "Probe service removed. Evidence and disposable key files remain in $probeRoot."
}
