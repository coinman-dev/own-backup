# Stage 2: the cold start itself. Runs as SYSTEM from a startup task after the
# VM was powered off and on again, with nobody logged on. It proves the point of
# E2: the service reopened its DPAPI-protected private key on its own.
#
# Whatever happens, this script powers the machine off at the end. A laboratory
# phase that hangs tells the operator nothing and blocks the next one.

function Say {
    param([string]$Text)
    if ($null -eq $Text) { $Text = '' }
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open()
        $port.WriteLine($Text)
        $port.Close()
    } catch {
    }
}

# Native tools write to stderr for ordinary answers ("no users are logged on"),
# and in PowerShell 5.1 that becomes a terminating NativeCommandError as soon as
# the stream is redirected. Let cmd.exe do the redirection instead.
function Run {
    param([string]$CommandLine)
    try {
        (cmd.exe /c "$CommandLine 2>&1" | Out-String).Trim()
    } catch {
        "failed: $($_.Exception.Message)"
    }
}

$probeRoot = 'C:\ProgramData\OwnBackup-SealProbe-Lab'
try {
    $ErrorActionPreference = 'Continue'
    Say "LAB-STAGE2-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    # Which copy of this script ran: the one installed with the VM, or a newer
    # one picked up from the laboratory media at boot.
    Say 'LAB-STAGE2-VERSION 6'
    Say "LAB-WHOAMI $(Run 'whoami')"
    $boot = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime()
    Say "LAB-BOOT $($boot.ToString('o'))"

    # The probe service starts by itself and exits once it has written its
    # report; wait for that report rather than guessing how long it takes.
    $reports = Join-Path $probeRoot 'reports.jsonl'
    $deadline = [DateTime]::UtcNow.AddMinutes(3)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (Test-Path -LiteralPath $reports) {
            $fresh = @(Get-Content -LiteralPath $reports |
                ForEach-Object { try { $_ | ConvertFrom-Json } catch { } } |
                Where-Object { $_ -and [DateTime]::Parse($_.at).ToUniversalTime() -ge $boot })
            if ($fresh.Count -gt 0) { break }
        }
        Start-Sleep -Seconds 5
    }
    Say "LAB-SERVICE $((Get-Service -Name 'OwnBackupSealProbe' -ErrorAction SilentlyContinue).Status)"

    # For a cold start the answer must be that nobody is logged on, and it has
    # to be readable whatever the Windows display language is: `query user`
    # answers in Russian here and arrives over the serial port unreadable.
    #
    # Counting interactive sessions is not the answer either — a locked,
    # nobody-home Windows still has interactive logons for the desktop window
    # manager and the font driver host. Name the accounts instead and leave out
    # those built-in ones: what remains is people.
    $interactive = @{}
    foreach ($session in Get-CimInstance Win32_LogonSession -ErrorAction SilentlyContinue |
        Where-Object { $_.LogonType -in 2, 10, 11 }) {
        $interactive[$session.LogonId] = $true
    }
    # A backslash is a regex escape and a PowerShell path separator at once, so
    # this builds and splits the account names by character code instead.
    $slash = [string][char]92
    $accounts = @(Get-CimInstance Win32_LoggedOnUser -ErrorAction SilentlyContinue |
        Where-Object { $interactive.ContainsKey($_.Dependent.LogonId) } |
        ForEach-Object { $_.Antecedent.Domain + $slash + $_.Antecedent.Name } |
        Sort-Object -Unique)
    # A locked, nobody-home Windows still logs on the desktop window manager and
    # the font driver host — and it does so under the COMPUTER's name, not under
    # "Window Manager", so the account name is what has to be looked at.
    $builtinDomains = @('NT AUTHORITY', 'Window Manager', 'Font Driver Host', 'NT SERVICE')
    $human = @($accounts | Where-Object {
        $parts = $_.Split([char]92)
        ($builtinDomains -notcontains $parts[0]) -and
        (-not $parts[-1].StartsWith('DWM-')) -and
        (-not $parts[-1].StartsWith('UMFD-'))
    })
    Say "LAB-INTERACTIVE-ACCOUNTS $($accounts -join ', ')"
    Say "LAB-HUMAN-LOGONS $($human.Count)"
    Say "LAB-EXPLORER-PROCESSES $(@(Get-Process explorer -ErrorAction SilentlyContinue).Count)"
    Say '--- logon sessions (localized text) ---'
    Say (Run 'query user')

    $ErrorActionPreference = 'Stop'
    try {
        $out = & 'C:\Lab\test-seal-service.ps1' -Action Verify 2>&1 | Out-String
        Say $out.Trim()
        Say 'LAB-STAGE2-OK'
    } catch {
        Say "LAB-STAGE2-FAIL $($_.Exception.Message)"
    }
    $ErrorActionPreference = 'Continue'

    Say '--- reports.jsonl ---'
    if (Test-Path -LiteralPath $reports) {
        Say ((Get-Content -LiteralPath $reports -Raw).Trim())
    } else {
        Say 'MISSING'
    }

    # The probe keeps its job state, and therefore its key store, under `state`.
    Say '--- key store ---'
    $store = Join-Path $probeRoot 'state\.own-backup-service-keys'
    if (Test-Path -LiteralPath $store) {
        Say ((Get-ChildItem -LiteralPath $store | Select-Object Name, Length | Out-String).Trim())
        Say (Run "icacls `"$store`"")
    } else {
        Say 'MISSING'
    }

    # The encrypted backup path, when the media carries its probe. It is a
    # separate script so a change to it means rebuilding the answer CD, not
    # reinstalling Windows.
    if (Test-Path -LiteralPath 'C:\Lab\lab-obk.ps1') {
        try {
            & 'C:\Lab\lab-obk.ps1'
        } catch {
            Say "LAB-OBK-CRASH $($_.Exception.Message)"
        }
    }
} catch {
    Say "LAB-STAGE2-CRASH $($_.Exception.Message)"
} finally {
    Say 'LAB-DONE'
    Start-Sleep -Seconds 2
    shutdown /s /t 5
}
