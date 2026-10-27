# The installer, on the machine that built it: silent install, update, removal.
# Run by .github/workflows/release.yml before a release is published.
#
# It registers a LocalSystem service and writes under Program Files, so it
# refuses to run anywhere but a throwaway CI runner. The thorough run -- a
# Cyrillic folder, a reboot, the wizard page by page -- is
# scripts/lab/run-installer.py; this one answers a narrower question: does the
# file GitHub has just built install at all.
#
# ASCII only: Windows PowerShell reads a script without a byte-order mark in
# the ANSI code page.
param(
    [Parameter(Mandatory = $true)][string]$Installer
)
$ErrorActionPreference = 'Stop'
if (-not $env:CI) { throw 'Run installer integration checks on an isolated CI runner.' }

$installerPath = (Resolve-Path -LiteralPath $Installer).ProviderPath
$version = [regex]::Match((Get-Content -LiteralPath (Join-Path $PSScriptRoot '..\Cargo.toml') -Raw),
    '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
$dir = Join-Path $env:ProgramFiles 'OwnBackup'
$dataDir = 'C:\ProgramData\OwnBackup'
$serviceKey = 'HKLM:\SYSTEM\CurrentControlSet\Services\OwnBackup'
$uninstallKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\OwnBackup'
$files = 'own-backup-gui.exe', 'own-backup-service.exe', 'own-backup-cli.exe', 'own-backup-shell.exe',
    'LICENSE.txt', 'NOTICE.txt', 'COMMERCIAL-LICENSE.txt', 'THIRD-PARTY-NOTICES.txt', 'uninstall.exe',
    'TotalCommander\OwnBackup.wfx64', 'TotalCommander\pluginst.inf', 'TotalCommander\OwnBackup-TC-plugin.zip'

if (Test-Path -LiteralPath $dir) { throw "The program folder already exists: $dir" }
if (Get-Service OwnBackup -ErrorAction SilentlyContinue) { throw 'The service is already registered on this machine.' }

function Install {
    $setup = Start-Process -FilePath $installerPath -ArgumentList '/S' -Wait -PassThru
    if ($setup.ExitCode -ne 0) {
        $log = Get-ChildItem -LiteralPath (Join-Path $dir 'Logs') -Filter 'own-backup-service-*.log' -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime | Select-Object -Last 1
        if ($log) { Get-Content -LiteralPath $log.FullName -Tail 40 | Write-Output }
        throw "Installer failed: $($setup.ExitCode)."
    }
}
function Assert-Installed([string]$Label) {
    foreach ($file in $files) {
        if (-not (Test-Path -LiteralPath (Join-Path $dir $file))) { throw "$Label`: missing installed file $file" }
    }
    $exe = Join-Path $dir 'own-backup-service.exe'
    $image = (Get-ItemProperty $serviceKey).ImagePath
    # Quoted: an unquoted path with a space in it lets C:\Program.exe run as SYSTEM.
    if ($image -ne ('"' + $exe + '"')) { throw "$Label`: the service is registered as $image" }
    for ($n = 0; $n -lt 120 -and (Get-Service OwnBackup).Status -ne 'Running'; $n++) { Start-Sleep -Milliseconds 500 }
    if ((Get-Service OwnBackup).Status -ne 'Running') { throw "$Label`: the service is not running" }
    if ((Get-ItemProperty $uninstallKey).DisplayVersion -ne $version) { throw "$Label`: wrong version in Programs and Features" }
}

Install
Assert-Installed 'install'
$status = cmd.exe /d /c "`"$(Join-Path $dir 'own-backup-cli.exe')`" jobs status 2>&1" | Out-String
if ($LASTEXITCODE -ne 0 -or -not $status.Contains('uptime_secs')) { throw "The command line does not reach the service: $status" }

# Something of the person's in the data folder: an update and a silent removal
# both have to leave it alone.
$mark = Join-Path $dataDir 'ci-mark.txt'
[IO.File]::WriteAllText($mark, 'kept')
Install
Assert-Installed 'update'
if (-not (Test-Path -LiteralPath $mark)) { throw 'The update removed a file from the data folder.' }

# _?= keeps the uninstaller in place instead of handing over to a copy of
# itself in TEMP, so -Wait waits for the removal. It then cannot delete itself.
$removed = Start-Process -FilePath (Join-Path $dir 'uninstall.exe') -ArgumentList "/S _?=$dir" -Wait -PassThru
if ($removed.ExitCode -ne 0) { throw "Uninstaller failed: $($removed.ExitCode)." }
if (Get-Service OwnBackup -ErrorAction SilentlyContinue) { throw 'The uninstaller left the service registered.' }
$left = @(Get-ChildItem -LiteralPath $dir -Force | Where-Object { $_.Name -ne 'uninstall.exe' } | ForEach-Object { $_.Name })
if ($left.Count) { throw "The uninstaller left files: $($left -join ', ')" }
if (Test-Path $uninstallKey) { throw 'The uninstaller left the Programs and Features entry.' }
if (-not (Test-Path -LiteralPath $mark)) { throw 'A silent removal deleted the settings.' }
Remove-Item -LiteralPath $dir -Recurse -Force
Write-Output "Install, update and removal of $version passed; the service ran and the data folder was kept."
