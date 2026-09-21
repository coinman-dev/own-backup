# Stage 1 of the disposable E2 laboratory: runs once, at the first logon of the
# freshly installed test VM, elevated. It installs the probe service and then
# powers the machine OFF, so the next boot is a real cold start rather than a
# service restart.
#
# Everything it says goes to COM1, which the host captures into a log file:
# the VM never needs networking, shared folders or a human at its console.

function Say {
    param([string]$Text)
    if ($null -eq $Text) { $Text = '' }
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open()
        $port.WriteLine($Text)
        $port.Close()
    } catch {
        # A lost line must not abort the run; the host also sees the exit state.
    }
    Write-Host $Text
}

try {
    $ErrorActionPreference = 'Stop'
    Say "LAB-STAGE1-BEGIN $([DateTime]::UtcNow.ToString('o'))"

    $source = Get-PSDrive -PSProvider FileSystem |
        ForEach-Object { Join-Path $_.Root 'lab' } |
        Where-Object { Test-Path -LiteralPath (Join-Path $_ 'lab-marker.txt') } |
        Select-Object -First 1
    if (-not $source) { throw 'lab media not found' }

    New-Item -ItemType Directory -Force -Path 'C:\Lab' | Out-Null
    Copy-Item -Path (Join-Path $source '*') -Destination 'C:\Lab' -Recurse -Force
    Say "LAB-STAGE1-MEDIA $source"

    $out = & 'C:\Lab\test-seal-service.ps1' -Action Install -BinaryPath 'C:\Lab\seal_store_service.exe' 2>&1 | Out-String
    Say $out.Trim()
    Say 'LAB-STAGE1-OK'

    # Stage 2 runs at the next boot as SYSTEM. It is started through a bootstrap
    # that prefers the copy on the laboratory media, so fixing the cold-start
    # script means rebuilding the answer CD, not reinstalling Windows.
    $bootstrap = @'
function SayBoot {
    param([string]$Text)
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}
# This runs at startup, and the CD-ROM is not always there yet: without the
# wait the refresh silently loses the race and the old script runs instead.
$media = $null
for ($attempt = 0; $attempt -lt 30 -and -not $media; $attempt++) {
    # Skip the system drive: C:\Lab is this VM's own copy, and finding it first
    # turns the refresh into copying the old script over itself.
    $media = Get-PSDrive -PSProvider FileSystem |
        Where-Object { $_.Root -ne ($env:SystemDrive + [string][char]92) } |
        ForEach-Object { Join-Path $_.Root 'lab' } |
        Where-Object { Test-Path -LiteralPath (Join-Path $_ 'lab-stage2.ps1') } |
        Select-Object -First 1
    if (-not $media) { Start-Sleep -Seconds 2 }
}
SayBoot "LAB-BOOTSTRAP media=$media"
if ($media) {
    # Files copied from a CD keep the read-only attribute, and Copy-Item -Force
    # will not overwrite them — clear it on both sides of the copy.
    Get-ChildItem -LiteralPath 'C:\Lab' -File -ErrorAction SilentlyContinue |
        ForEach-Object { try { $_.IsReadOnly = $false } catch { } }
    Copy-Item -Path (Join-Path $media '*') -Destination 'C:\Lab' -Recurse -Force -ErrorAction SilentlyContinue
    Get-ChildItem -LiteralPath 'C:\Lab' -File -ErrorAction SilentlyContinue |
        ForEach-Object { try { $_.IsReadOnly = $false } catch { } }
}
& 'C:\Lab\lab-stage2.ps1'
'@
    Set-Content -LiteralPath 'C:\Lab\lab-stage2-boot.ps1' -Value $bootstrap -Encoding UTF8
    schtasks /Create /TN 'OwnBackupLabStage2' /SC ONSTART /RU SYSTEM /RL HIGHEST /F `
        /TR 'powershell -NoProfile -ExecutionPolicy Bypass -File C:\Lab\lab-stage2-boot.ps1' | Out-Null

    # The cold start must have nobody logged on: the probe service has to come
    # up by itself, and stage 2 runs from that startup task.
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty -Path $winlogon -Name 'AutoAdminLogon' -Value '0'
    Remove-ItemProperty -Path $winlogon -Name 'DefaultPassword' -ErrorAction SilentlyContinue
} catch {
    Say "LAB-STAGE1-FAIL $($_.Exception.Message)"
} finally {
    Say 'LAB-STAGE1-POWEROFF'
    Start-Sleep -Seconds 2
    shutdown /s /t 5
}
