# The whole service killed while it deletes excluded files inside a new copy
# (CHECKS 2), guest side, for the format in C:\Lab\killx-format.txt: vhdx,
# qcow2 or obk (encrypted). Run by phase: at the first logon after Setup
# (install), then at every startup as SYSTEM, and in the lab user's session.
#
#   install - install the product silently, schedule this script, power off
#   prepare - (SYSTEM) source P: and store S: on the data disk, the control
#             data, the lab user's logon, reboot
#   run     - (user) job: P: -> S:\killx-<format>, *.tmp excluded, a new
#             chain after one increment, one chain kept. Run 1 is killed in
#             the exclusions phase by the SYSTEM half; then runs 2..4, each
#             checked, and the last copy mounted and compared
#   kill    - (SYSTEM, at the same boot as run) waits for the user half to
#             ask, kills own-backup-service.exe, records the folder, makes
#             sure the service runs again
#
# Everything is reported over COM1. ASCII only (PowerShell 5.1, no BOM).
param([switch]$User)
$ErrorActionPreference = 'Stop'
$lab = 'C:\Lab'
$base = 'C:\Lab\killx'
$installed = 'C:\Program Files\OwnBackup'
# One VM, one format at a time; each has its own state, store and job.
# Line 1 the format, line 2 an optional tag: a new tag is a fresh attempt.
$spec = if (Test-Path "$lab\killx-format.txt") { @(Get-Content "$lab\killx-format.txt" | Where-Object { $_.Trim() }) } else { @('vhdx') }
$format = $spec[0].Trim()
$tag = if ($spec.Count -gt 1) { '-' + $spec[1].Trim() } else { '' }
$work = "$base\$format$tag"
$store = "S:\killx-$format$tag"
$encrypted = $format -eq 'obk'
$private = if ($encrypted) { '.own-backup-obk-*' } else { '.own-backup-new-*' }
$imageExt = @{ vhdx = 'vhdx'; qcow2 = 'qcow2'; obk = 'obk' }[$format]
$childExt = @{ vhdx = 'avhdx'; qcow2 = 'qcow2'; obk = 'obk' }[$format]
$publicPattern = @{ vhdx = '\.(vhdx|avhdx)$'; qcow2 = '\.qcow2$'; obk = '\.obk$' }[$format]
$jobName = "KillX$format"
$secret = if ($encrypted) { @{ secret = 'password'; password_file = "$base\$format$tag\password.txt" } } else { @{ secret = 'none' } }
$seed = 20261017

function Say([string]$text) {
    $port = New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One'
    try {
        for ($attempt = 0; $attempt -lt 40; $attempt++) {
            try { $port.Open(); break } catch { if ($attempt -eq 39) { throw }; Start-Sleep -Milliseconds 100 }
        }
        $port.WriteLine($text)
    } finally { $port.Dispose() }
}
function Check([bool]$ok, [string]$text) { if (-not $ok) { throw "check failed: $text" }; Say "KILLX-OK $text" }
function JsonFile([string]$path, $value) {
    [IO.File]::WriteAllText($path, (ConvertTo-Json -InputObject $value -Depth 20), [Text.UTF8Encoding]::new($false))
}
function Report([string]$name, $value) {
    $path = Join-Path $work "$name.json"
    JsonFile $path $value
    Say "SYS-REPORT $name $([Convert]::ToBase64String([IO.File]::ReadAllBytes($path)))"
}
function Step([string]$text) {
    [IO.File]::AppendAllText("$work\system-steps.txt", "$([DateTime]::UtcNow.ToString('o')) $text`r`n")
    try { Say "KILLX-STEP $text" } catch { }
}
function Phase([string]$value) { [IO.File]::WriteAllText("$base\phase.txt", $value) }
function Rpc([string]$name, $request) {
    $path = Join-Path $work "$name.request.json"
    JsonFile $path $request
    $out = Join-Path $work "$name.out"; $err = Join-Path $work "$name.err"
    $p = Start-Process "$lab\audit_autonomous.exe" -ArgumentList ('"' + $path + '"') -RedirectStandardOutput $out -RedirectStandardError $err -NoNewWindow -PassThru
    $null = $p.Handle
    if (-not $p.WaitForExit(3600000)) { $p.Kill(); throw "RPC timeout: $name" }
    $p.Refresh()
    $resultPath = [IO.Path]::ChangeExtension($path, 'result.json')
    $result = if (Test-Path $resultPath) { Get-Content $resultPath -Raw -Encoding UTF8 | ConvertFrom-Json } else { $null }
    if ($result) { Report $name $result }
    if ($p.ExitCode -ne 0) { throw "RPC failed: $name exit=$($p.ExitCode) $([IO.File]::ReadAllText($err))" }
    return $result.result
}
function WaitService {
    for ($i = 0; $i -lt 180; $i++) {
        $s = Get-Service OwnBackup -ErrorAction SilentlyContinue
        if ($s -and $s.Status -eq 'Running' -and (Test-Path 'C:\ProgramData\OwnBackup\service.token')) { Start-Sleep -Seconds 3; return }
        Start-Sleep -Seconds 1
    }
    throw 'the OwnBackup service did not come up'
}
function Sha([byte[]]$bytes) {
    $h = [Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($h.ComputeHash($bytes))).Replace('-', '') } finally { $h.Dispose() }
}
function Listing {
    return @(Get-ChildItem -LiteralPath $store -Force -File | Sort-Object Name |
        ForEach-Object { [ordered]@{ name = $_.Name; length = $_.Length } })
}
function Names { return @(Get-ChildItem -LiteralPath $store -Force -File | ForEach-Object { $_.Name }) }
function Pending { return @(Names | Where-Object { $_ -like $private }) }
function Public { return @(Names | Where-Object { $_ -notlike $private -and $_ -match $publicPattern }) }

# --- Control data -----------------------------------------------------------
# Kept files from a generator (so the expectation does not live on a disk
# being checked) and many excluded ones, so the deletion pass takes a while.
function Kept {
    $rng = New-Object Random $seed
    $list = New-Object Collections.Generic.List[object]
    for ($i = 1; $i -le 60; $i++) {
        $bytes = New-Object byte[] ($rng.Next(64KB, 4MB)); $rng.NextBytes($bytes)
        $list.Add([pscustomobject]@{ Name = ("keep\k{0:D3}.bin" -f $i); Bytes = $bytes })
    }
    $big = New-Object byte[] (160MB); $rng.NextBytes($big)
    $list.Add([pscustomobject]@{ Name = 'keep\big.bin'; Bytes = $big })
    return $list
}
function MakeData {
    New-Item -ItemType Directory -Force 'P:\keep', 'P:\cache\deep\er' | Out-Null
    foreach ($f in Kept) { [IO.File]::WriteAllBytes("P:\$($f.Name)", $f.Bytes) }
    $rng = New-Object Random ($seed + 1)
    for ($i = 1; $i -le 3000; $i++) {
        $bytes = New-Object byte[] ($rng.Next(1KB, 48KB)); $rng.NextBytes($bytes)
        $dir = if ($i % 3 -eq 0) { 'P:\cache\deep\er' } else { 'P:\cache' }
        [IO.File]::WriteAllBytes(("$dir\c{0:D5}.tmp" -f $i), $bytes)
    }
    for ($i = 1; $i -le 24; $i++) {
        $bytes = New-Object byte[] (8MB); $rng.NextBytes($bytes)
        [IO.File]::WriteAllBytes(("P:\cache\big{0:D2}.tmp" -f $i), $bytes)
    }
    [IO.File]::WriteAllText('P:\killx-marker.txt', 'Own Backup kill-during-exclusions source')
}
function VerifyMounted([string]$root) {
    foreach ($f in Kept) {
        $path = Join-Path $root $f.Name
        Check ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -eq (Sha $f.Bytes)) "restored content of $($f.Name)"
    }
    $left = @(Get-ChildItem -LiteralPath (Join-Path $root 'cache') -Recurse -Force -File -ErrorAction SilentlyContinue)
    Check ($left.Count -eq 0) "no excluded file has a directory entry in the copy ($($left.Count) left)"
}

function AutoLogon {
    $answer = New-Object Xml.XmlDocument; $answer.Load("$lab\autounattend.xml")
    $password = $answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    $key = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $key DefaultUserName 'lab'; Set-ItemProperty $key DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $key DefaultPassword $password; Set-ItemProperty $key AutoAdminLogon '1'; Set-ItemProperty $key AutoLogonCount 1
    $startup = Join-Path ([Environment]::GetFolderPath('CommonStartup')) 'OwnBackupKillxLogon.cmd'
    [IO.File]::WriteAllText($startup, "@echo off`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File $lab\killx-guest.ps1 -User`r`n", [Text.Encoding]::ASCII)
}
function NoLogon {
    $key = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $key AutoAdminLogon '0'
    Remove-ItemProperty $key DefaultPassword -ErrorAction SilentlyContinue
    Remove-Item (Join-Path ([Environment]::GetFolderPath('CommonStartup')) 'OwnBackupKillxLogon.cmd') -ErrorAction SilentlyContinue
}

$phase = 'install'
try {
    if (Test-Path "$base\phase.txt") { $phase = (Get-Content "$base\phase.txt" -Raw).Trim() }
    # A format that passed leaves the VM in 'run' for the next one.
    if ($phase -eq 'done') { $phase = 'run' }
    New-Item -ItemType Directory -Force $work | Out-Null
    Say "KILLX-BEGIN format=$format phase=$phase utc=$([DateTime]::UtcNow.ToString('o')) user=$([Security.Principal.WindowsIdentity]::GetCurrent().Name)"

    if ($phase -eq 'install') {
        $media = Get-PSDrive -PSProvider FileSystem | ForEach-Object { Join-Path $_.Root 'lab' } |
            Where-Object { Test-Path -LiteralPath (Join-Path $_ 'sys-marker.txt') } | Select-Object -First 1
        Check ([bool]$media) "lab media found at $media"
        New-Item -ItemType Directory -Force $lab, $work | Out-Null
        Copy-Item -Path (Join-Path $media '*') -Destination $lab -Recurse -Force
        Get-ChildItem $lab -Recurse -File | ForEach-Object { $_.IsReadOnly = $false }
        $setup = Start-Process "$lab\own-backup-install.exe" -ArgumentList '/S' -Wait -PassThru
        Check ($setup.ExitCode -eq 0) "installer exit $($setup.ExitCode)"
        WaitService
        Check (Test-Path "$installed\own-backup-service.exe") 'product installed'
        $null = & powercfg.exe /h off
        $boot = @'
$media = $null
for ($i = 0; $i -lt 30 -and -not $media; $i++) {
    $media = Get-PSDrive -PSProvider FileSystem | Where-Object { $_.Root -ne 'C:\' } |
        ForEach-Object { Join-Path $_.Root 'lab' } |
        Where-Object { Test-Path -LiteralPath (Join-Path $_ 'sys-marker.txt') } | Select-Object -First 1
    if (-not $media) { Start-Sleep -Seconds 2 }
}
if ($media) {
    Get-ChildItem 'C:\Lab' -File | ForEach-Object { try { $_.IsReadOnly = $false } catch { } }
    Copy-Item -Path (Join-Path $media '*') -Destination 'C:\Lab' -Recurse -Force -ErrorAction SilentlyContinue
    Get-ChildItem 'C:\Lab' -File | ForEach-Object { try { $_.IsReadOnly = $false } catch { } }
}
& 'C:\Lab\killx-guest.ps1'
'@
        [IO.File]::WriteAllText("$lab\killx-boot.ps1", $boot, [Text.Encoding]::ASCII)
        schtasks.exe /Create /TN 'OwnBackupKillx' /SC ONSTART /RU SYSTEM /RL HIGHEST /F `
            /TR "powershell -NoProfile -ExecutionPolicy Bypass -File $lab\killx-boot.ps1" | Out-Null
        NoLogon
        Phase 'prepare'
        Say 'KILLX-INSTALL-DONE'
        shutdown.exe /s /t 5 | Out-Null
        return
    }

    if ($User -and $phase -ne 'run') { Say "KILLX-WAIT phase=$phase user"; return }

    if ($phase -eq 'prepare') {
        WaitService
        if (-not (Get-Volume -FileSystemLabel OBKSTORE -ErrorAction SilentlyContinue)) {
            $raw = @(Get-Disk | Where-Object { $_.PartitionStyle -eq 'RAW' })
            Check ($raw.Count -eq 1) 'one blank data disk attached'
            Initialize-Disk -Number $raw[0].Number -PartitionStyle GPT
            $src = New-Partition -DiskNumber $raw[0].Number -Size 2GB -DriveLetter P
            Format-Volume -Partition $src -FileSystem NTFS -NewFileSystemLabel OBKSRC -Confirm:$false | Out-Null
            $dst = New-Partition -DiskNumber $raw[0].Number -UseMaximumSize -DriveLetter S
            Format-Volume -Partition $dst -FileSystem NTFS -NewFileSystemLabel OBKSTORE -Confirm:$false | Out-Null
        }
        Check (Test-Path 'P:\') 'source volume at P:'
        Check (Test-Path 'S:\') 'store volume at S:'
        if (-not (Test-Path 'P:\killx-marker.txt')) { MakeData }
        Report 'source' @{ files = @(Get-ChildItem P:\ -Recurse -File).Count; excluded = @(Get-ChildItem P:\cache -Recurse -File).Count }
        AutoLogon
        Phase 'run'
        Say 'KILLX-PREPARED rebooting into the lab user'
        shutdown.exe /r /t 5 | Out-Null
        return
    }

    if ($phase -eq 'run' -and (Test-Path "$work\passed.txt")) { Say "KILLX-ALREADY-DONE format=$format"; shutdown.exe /s /t 5 | Out-Null; return }

    # The SYSTEM half of 'run': the kill, when the user half asks for it.
    if (-not $User -and $phase -eq 'run') {
        # A newer build on the CD goes over the installed one first; the user
        # half waits for it (see 'upgrading').
        $want = (Get-FileHash "$lab\own-backup-install.exe").Hash
        $have = if (Test-Path "$base\installed.sha256") { (Get-Content "$base\installed.sha256" -Raw).Trim() } else { '' }
        if ($want -ne $have) {
            [IO.File]::WriteAllText("$base\upgrading", 'upgrading')
            $setup = Start-Process "$lab\own-backup-install.exe" -ArgumentList '/S' -Wait -PassThru
            Check ($setup.ExitCode -eq 0) "upgrade installer exit $($setup.ExitCode)"
            WaitService
            [IO.File]::WriteAllText("$base\installed.sha256", $want)
            Remove-Item "$base\upgrading" -Force
            Say "KILLX-UPGRADED $want"
        }
        if (Test-Path "$work\killed.json") {
            # A reboot after the kill: the user half finishes by itself.
            $armed = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon').AutoAdminLogon -eq '1'
            if (-not $armed) { AutoLogon; Say 'KILLX-REARM'; shutdown.exe /r /t 5 | Out-Null }
            return
        }
        # A request left by an attempt that did not finish is not this one's.
        Remove-Item "$work\kill.request" -ErrorAction SilentlyContinue
        # AutoLogonCount is one logon: a retried attempt arms it again, which
        # only counts from the next boot.
        $armed = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon').AutoAdminLogon -eq '1'
        if (-not $armed) {
            # Winlogon also clears it once the logon has happened: rearm only
            # when nobody logs on at all.
            for ($i = 0; $i -lt 120 -and -not (Get-Process explorer -ErrorAction SilentlyContinue); $i++) { Start-Sleep -Seconds 1 }
            if (-not (Get-Process explorer -ErrorAction SilentlyContinue)) { AutoLogon; Say 'KILLX-REARM'; shutdown.exe /r /t 5 | Out-Null; return }
        }
        Step 'waiting for the kill request'
        $deadline = [DateTime]::UtcNow.AddMinutes(40)
        while (-not (Test-Path "$work\kill.request")) {
            if ([DateTime]::UtcNow -gt $deadline) { throw 'the user half never asked for the kill' }
            Start-Sleep -Milliseconds 200
        }
        $service = Get-Process own-backup-service -ErrorAction Stop
        $at = [DateTime]::UtcNow.ToString('o')
        Stop-Process -Id $service.Id -Force
        Say "KILLX-KILLED pid=$($service.Id) utc=$at"
        Step 'killed'
        Start-Sleep -Seconds 2
        $right = [ordered]@{ utc = $at; pid = $service.Id; listing = (Listing) }
        Step "listed $(@($right.listing).Count) files"
        # The service log names the step the pass was in.
        $log = Get-ChildItem "$installed\Logs" -Filter 'own-backup-service-*.log' -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime | Select-Object -Last 1
        if ($log) {
            $copy = Join-Path $work 'service-at-kill.log'
            Copy-Item -LiteralPath $log.FullName $copy -Force
            # Plain strings: Get-Content's carry PSDrive/PSProvider, which
            # ConvertTo-Json -Depth 20 walks for minutes (PowerShell 5.1).
            $right.log_tail = [string[]]@(Get-Content -LiteralPath $copy -Tail 25 | ForEach-Object { "$_" })
        }
        Step 'log copied'
        for ($i = 0; $i -lt 60 -and (Get-Service OwnBackup).Status -ne 'Running'; $i++) {
            try { Start-Service OwnBackup -ErrorAction Stop } catch { Start-Sleep -Seconds 1 }
        }
        Step "service $((Get-Service OwnBackup).Status)"
        WaitService
        Step 'service answers'
        $right.restarted = @(Get-Process own-backup-service | ForEach-Object { $_.Id })
        JsonFile "$work\killed.json" $right
        Step 'killed.json written'
        Report 'killed' $right
        return
    }

    if ($User -and $phase -eq 'run') {
        for ($i = 0; $i -lt 600 -and (Test-Path "$base\upgrading"); $i++) { Start-Sleep -Seconds 1 }
        # The installer's own start of the service is not the end of it.
        Start-Sleep -Seconds 5
        WaitService
        New-Item -ItemType Directory -Force $store | Out-Null
        $state = if (Test-Path "$work\job.json") { Get-Content "$work\job.json" -Raw | ConvertFrom-Json } else { $null }
        if (-not $state) {
            # Synthetic secret of a throwaway VM.
            [IO.File]::WriteAllText("$work\password.txt", 'KillX-Lab-2026-q9')
            $job = Rpc 'create' @{ op = 'create'; encrypted = $encrypted
                password_file = "$work\password.txt"; recovery_file = "$work\recovery.txt"; job = @{
                name = $jobName; source = 'P:'; sources = @('P:'); dest_folder = $store
                image_format = $(if ($format -eq 'vhdx') { 'vhdx' } else { 'qcow2' });
                schedule = @{ kind = 'none' };
                scheme = @{ method = 'incremental'; only_incremental = $false; full_every_n = 1; keep_chains = 1 };
                exclusions = @{ enabled = $true; patterns = @('*.tmp'); patterns_linux = @() };
                advanced = @{ validate_after = $false; retry_attempts = 0; after_backup = 'none' } } }
            JsonFile "$work\job.json" @{ id = $job.id }
            $state = Get-Content "$work\job.json" -Raw | ConvertFrom-Json
        }
        $id = $state.id

        # Run 1, killed inside the deletion of the excluded files.
        if (-not (Test-Path "$work\killed.json")) {
            # What an earlier unfinished attempt left; this one starts clean.
            $stale = @(Names)
            if ($stale.Count) { Report 'stale-before-run1' @{ files = $stale }; Get-ChildItem -LiteralPath $store -Force | Remove-Item -Force }
            $at = Rpc 'run1-at-exclusions' @{ op = 'start-run-at-phase'; id = $id; phase = 'exclusions'; after_ms = 1500; timeout_secs = 1200 }
            [IO.File]::WriteAllText("$work\kill.request", 'kill')
            for ($i = 0; $i -lt 600 -and -not (Test-Path "$work\killed.json"); $i++) { Start-Sleep -Milliseconds 500 }
            if (-not (Test-Path "$work\killed.json")) {
                Say "SYS-DIAG work: $((Get-ChildItem $work -Force | ForEach-Object { $_.Name }) -join ', ')"
                if (Test-Path "$work\system-steps.txt") { Get-Content "$work\system-steps.txt" | ForEach-Object { Say "SYS-DIAG system: $_" } }
            }
            Check (Test-Path "$work\killed.json") "the service was killed $($at.in_phase_ms) ms into the exclusions phase"
        }
        $killed = Get-Content "$work\killed.json" -Raw | ConvertFrom-Json
        $atKill = @($killed.listing | ForEach-Object { $_.name })
        Check (@($atKill | Where-Object { $_ -notlike $private }).Count -eq 0) "nothing under a public name at the kill ($($atKill -join ', '))"
        Check (@($atKill | Where-Object { $_ -like "$private.tmp.$imageExt" }).Count -eq 1) 'the unfinished image is private'
        WaitService
        $after = Rpc 'job-after-kill' @{ op = 'get'; id = $id }
        Check ($null -eq $after.last_run) 'the killed run left no success in the history'
        $leftover = @(Pending)

        # Run 2: a full; the leftover is not a parent and not a version.
        $run2 = Rpc 'run2' @{ op = 'run'; id = $id; timeout_secs = 1800 }
        Check ($run2.status -eq 'success') "run 2 succeeded: $($run2.error)"
        Check ($run2.backup_kind -eq 'full') "run 2 is a full ($($run2.backup_kind))"
        Check ($run2.image_path -like "$store\$($jobName)_*_full_*.$imageExt") "run 2 published under its public name ($($run2.image_path))"
        # Run 3: an increment of run 2.
        $run3 = Rpc 'run3' @{ op = 'run'; id = $id; timeout_secs = 1800 }
        Check ($run3.status -eq 'success') "run 3 succeeded: $($run3.error)"
        Check ($run3.backup_kind -eq 'incremental') "run 3 is an increment ($($run3.backup_kind))"
        Check ($run3.image_path -like "$store\$($jobName)_*_inc_*.$childExt") "run 3 published as a child ($($run3.image_path))"
        # Run 4: the chain rolls over; keeping one chain deletes runs 2 and 3,
        # which needs a strict scan of a folder that holds the leftover.
        $run4 = Rpc 'run4' @{ op = 'run'; id = $id; timeout_secs = 1800 }
        Check ($run4.status -eq 'success') "run 4 succeeded: $($run4.error)"
        Check ($run4.backup_kind -eq 'full') "run 4 starts a new chain ($($run4.backup_kind))"
        Check (-not (Test-Path -LiteralPath $run2.image_path) -and -not (Test-Path -LiteralPath $run3.image_path)) 'retention deleted the previous chain'
        $public = @(Public)
        Check ($public.Count -eq 1 -and "$store\$($public[0])" -eq $run4.image_path) "only run 4 is a public image ($($public -join ', '))"
        $versions = Rpc 'versions' @{ op = 'versions'; id = $id }
        $leftNow = @(Pending)
        if ($encrypted) {
            # The stage, its owner record and the exclusion scratch named from
            # it are proved by the job's key, so the next run reclaims them all.
            Check (@($leftover | Where-Object { $leftNow -contains $_ }).Count -eq 0) "the killed run's proved leftovers were reclaimed ($($leftover -join ', '))"
        } else {
            Check (@($leftover | Where-Object { $leftNow -contains $_ }).Count -eq $leftover.Count) 'the killed run leftover is kept, not deleted by guesswork'
        }
        Report 'listing-after' @{ files = (Listing); versions = $versions; left_of_kill = @($leftover | Where-Object { $leftNow -contains $_ }) }

        $verify = Rpc 'verify4' (@{ op = 'verify'; image = $run4.image_path } + $secret)
        $null = Rpc 'mount4' (@{ op = 'mount'; image = $run4.image_path } + $secret)
        try {
            $root = $null
            for ($i = 0; $i -lt 60 -and -not $root; $i++) {
                $root = Get-Volume | Where-Object { $_.DriveLetter -and $_.DriveLetter -notin @('C', 'P', 'S') } |
                    ForEach-Object { "$($_.DriveLetter):\" } | Where-Object { Test-Path (Join-Path $_ 'killx-marker.txt') } | Select-Object -First 1
                if (-not $root) { Start-Sleep -Seconds 1 }
            }
            Check ([bool]$root) "the copy of run 4 is mounted at $root"
            VerifyMounted $root
        } finally {
            try { $null = Rpc 'unmount4' @{ op = 'unmount'; image = $run4.image_path } } catch { Say "KILLX-NOTE unmount: $($_.Exception.Message)" }
        }
        # Not NoLogon: HKLM is out of an unelevated user's reach, and
        # Winlogon has already used up the one logon it was armed for.
        [IO.File]::WriteAllText("$work\passed.txt", 'pass')
        Report 'result' @{ pass = $true; leftover = $leftover; runs = @($run2.image_path, $run3.image_path, $run4.image_path); verify = $verify }
        Say "KILLX-PASS format=$format"
        shutdown.exe /s /t 5 | Out-Null
        return
    }
    throw "unknown phase $phase"
} catch {
    # A failed user half starts over next time: a new job, a clean store.
    if ($User) { Remove-Item "$work\killed.json", "$work\job.json" -ErrorAction SilentlyContinue }
    Say "KILLX-FAIL phase=$phase line=$($_.InvocationInfo.ScriptLineNumber) reason=$($_.Exception.Message)"
    try { Say "SYS-DIAG service=$((Get-Service OwnBackup -ErrorAction SilentlyContinue).Status)" } catch { }
    try {
        Get-ChildItem $work -Filter '*.err' -ErrorAction SilentlyContinue |
            ForEach-Object { Say "SYS-DIAG helper $($_.Name): $(([IO.File]::ReadAllText($_.FullName)) -replace '\s+', ' ')" }
    } catch { }
    try { Say "SYS-DIAG store: $((Names) -join ', ')" } catch { }
    try {
        $log = Get-ChildItem "$installed\Logs" -Filter 'own-backup-service-*.log' -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime | Select-Object -Last 1
        if ($log) {
            $copy = Join-Path $env:TEMP 'service-tail.log'
            Copy-Item -LiteralPath $log.FullName $copy -Force
            Get-Content -LiteralPath $copy -Tail 60 | ForEach-Object { Say "SYS-LOG $_" }
        }
    } catch { }
    shutdown.exe /s /t 10 | Out-Null
}
