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
# Anything that fails before the main try (the setup below) would otherwise
# end the script without a word: say it on COM1 directly.
trap {
    try {
        $port = New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One'
        $port.Open(); $port.WriteLine("KILLX-FAIL early line=$($_.InvocationInfo.ScriptLineNumber) reason=$($_.Exception.Message)"); $port.Dispose()
    } catch { }
    break
}
$lab = 'C:\Lab'
$base = 'C:\Lab\killx'
$installed = 'C:\Program Files\OwnBackup'
# One VM, one format at a time; each has its own state, store and job.
# Line 1 the format, line 2 an optional tag: a new tag is a fresh attempt.
# @() outside the if: an if hands back one line as a bare string, and
# $spec[0] of a string is its first character.
$spec = @(if (Test-Path "$lab\killx-format.txt") { Get-Content "$lab\killx-format.txt" | Where-Object { $_.Trim() } } else { 'vhdx' })
$format = $spec[0].Trim()
$tag = if ($spec.Count -gt 1) { '-' + $spec[1].Trim() } else { '' }
# Line 3: the phase run 1 is killed in (exclusions unless said otherwise).
$killPhase = if ($spec.Count -gt 2) { $spec[2].Trim() } else { 'exclusions' }
$work = "$base\$format$tag"
$store = if ($format -like 'flush-*' -or $format -like 'stall-*' -or $format -eq 'probe-io') { "T:\killx-$format$tag" } else { "S:\killx-$format$tag" }
# 'consol', 'consol-vhdx', 'consol-qcow2': the consolidation scenario over
# OBK, VHDX or qcow2; anything else is the exclusions scenario.
# 'flush-vhdx', 'flush-obk': every flush of the copies' own disk fails.
$mode = if ($format -eq 'history') { 'history' } elseif ($format -like 'consol*') { 'consol' } elseif ($format -like 'flush-*' -or $format -like 'stall-*' -or $format -eq 'probe-io') { 'flush' } else { 'exclusions' }
$container = switch ($format) { 'consol' { 'obk' } 'consol-vhdx' { 'vhdx' } 'consol-qcow2' { 'qcow2' } 'flush-vhdx' { 'vhdx' } 'flush-obk' { 'obk' } 'stall-vhdx' { 'vhdx' } 'stall-obk' { 'obk' } 'probe-io' { 'vhdx' } 'history' { 'vhdx' } default { $format } }
$encrypted = $container -eq 'obk'
$private = if ($encrypted) { '.own-backup-obk-*' } else { '.own-backup-new-*' }
$imageExt = @{ vhdx = 'vhdx'; qcow2 = 'qcow2'; obk = 'obk' }[$container]
$childExt = @{ vhdx = 'avhdx'; qcow2 = 'qcow2'; obk = 'obk' }[$container]
$publicPattern = @{ vhdx = '\.(vhdx|avhdx)$'; qcow2 = '\.qcow2$'; obk = '\.obk$' }[$container]
$jobName = "KillX$format"
# What stands where the history's temporary files go (mode 'history'): both
# files, so that nothing of a run reaches the disk.
$obstacles = @('C:\ProgramData\OwnBackup\activity.json.tmp', 'C:\ProgramData\OwnBackup\jobs.json.tmp')
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
function Rpc([string]$name, $request, [switch]$Quiet) {
    $path = Join-Path $work "$name.request.json"
    JsonFile $path $request
    $out = Join-Path $work "$name.out"; $err = Join-Path $work "$name.err"
    $p = Start-Process "$lab\audit_autonomous.exe" -ArgumentList ('"' + $path + '"') -RedirectStandardOutput $out -RedirectStandardError $err -NoNewWindow -PassThru
    $null = $p.Handle
    if (-not $p.WaitForExit(3600000)) { $p.Kill(); throw "RPC timeout: $name" }
    $p.Refresh()
    $resultPath = [IO.Path]::ChangeExtension($path, 'result.json')
    $result = if (Test-Path $resultPath) { Get-Content $resultPath -Raw -Encoding UTF8 | ConvertFrom-Json } else { $null }
    # -Quiet: a result of megabytes would take minutes on the serial line.
    if ($result -and (-not $Quiet -or -not $result.ok)) { Report $name $result }
    if ($p.ExitCode -ne 0) { throw "RPC failed: $name exit=$($p.ExitCode) $([IO.File]::ReadAllText($err))" }
    return $result.result
}
function WaitService {
    for ($i = 0; $i -lt 180; $i++) {
        $s = Get-Service OwnBackup -ErrorAction SilentlyContinue
        # The pipe, not the token file: a token outlives the service that wrote
        # it, and a restarted service answers only once its pipe is there.
        $pipe = [IO.Directory]::GetFiles('\\.\pipe\') -contains '\\.\pipe\OwnBackup'
        if ($s -and $s.Status -eq 'Running' -and $pipe -and (Test-Path 'C:\ProgramData\OwnBackup\service.token')) { Start-Sleep -Seconds 3; return }
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

# --- Consolidation killed (format 'consol') ----------------------------------
# A window of one increment: from the third run on, every run folds the oldest
# increment into the base after its copy. The fold says nothing over the event
# stream, so the kill waits for its marker file and then a delay. Each run adds
# its own file to the source, so a fold that lost or doubled anything shows in
# the last copy's content.
function RunBytes([int]$k) {
    $bytes = New-Object byte[] (24MB); (New-Object Random ($seed + 100 + $k)).NextBytes($bytes)
    return ,$bytes
}
function AddRunFile([int]$k) { [IO.File]::WriteAllBytes("P:\keep\run-$k.bin", (RunBytes $k)) }
function RunJob([string]$name, [string]$id) {
    $run = Rpc $name @{ op = 'run'; id = $id; timeout_secs = 1800 }
    Check ($run.status -eq 'success') "$name succeeded: $($run.error)"
    return $run
}
function Consolidation([string]$id) {
    Get-ChildItem 'P:\keep' -Filter 'run-*.bin' -ErrorAction SilentlyContinue | Remove-Item -Force
    $stale = @(Names)
    if ($stale.Count) { Report 'stale-before-run1' @{ files = $stale }; Get-ChildItem -LiteralPath $store -Force | Remove-Item -Force }
    $k = 1; AddRunFile $k; $null = RunJob "run$k" $id
    $k++; AddRunFile $k; $null = RunJob "run$k" $id
    $kills = @()
    foreach ($delay in @(0, 100, 250, 500)) {
        $k++; AddRunFile $k
        $name = "consol-$k"
        [IO.File]::WriteAllText("$work\kill.request", (ConvertTo-Json -Compress @{ name = $name; pattern = '*.consolidating.json'; delay_ms = $delay; timeout_s = 600 }))
        # The run's RPC dies with the service.
        try { $null = Rpc "run$k" @{ op = 'run'; id = $id; timeout_secs = 1800 } } catch { Say "KILLX-NOTE run $k ended with the service: $($_.Exception.Message -replace '\s+', ' ')" }
        for ($i = 0; $i -lt 1200 -and -not (Test-Path "$work\$name.json"); $i++) { Start-Sleep -Milliseconds 500 }
        Check (Test-Path "$work\$name.json") "run $k was killed $delay ms after its consolidation marker"
        $kill = Get-Content "$work\$name.json" -Raw | ConvertFrom-Json
        Check ([bool]$kill.trigger) "the marker of run $k's consolidation was seen ($($kill.trigger))"
        $kills += [ordered]@{ run = $k; delay_ms = $delay; at_kill = @($kill.listing | ForEach-Object { $_.name }) }
        WaitService
        # A clean run finishes what the kill interrupted, copies, and folds its
        # own; the next kill then meets a fold of its own run, not a resume.
        $k++; AddRunFile $k; $null = RunJob "run$k" $id
        Check (@(Get-ChildItem -LiteralPath $store -Force -Filter '*.consolidating.json').Count -eq 0) "run $k left no consolidation marker"
        foreach ($image in @(Public)) {
            $v = Rpc "verify-after-$k-$image" (@{ op = 'verify'; image = "$store\$image" } + $secret)
            Check $v.valid "after run $k, $image verifies"
        }
    }
    [IO.File]::WriteAllText("$work\kills.done", 'done')
    # The next run finishes what the kill interrupted, then copies and folds.
    $k++; AddRunFile $k; $last = RunJob "run$k" $id
    Check (@(Get-ChildItem -LiteralPath $store -Force -Filter '*.consolidating.json').Count -eq 0) 'no consolidation marker is left'
    $public = @(Public)
    Check ($public.Count -eq 2) "the window holds a base and one increment ($($public -join ', '))"
    Check (@(Pending).Count -eq 0) "no private file is left ($((@(Pending)) -join ', '))"
    foreach ($image in $public) {
        $v = Rpc "verify-final-$image" (@{ op = 'verify'; image = "$store\$image" } + $secret)
        Check $v.valid "$image verifies"
    }
    $versions = Rpc 'versions' @{ op = 'versions'; id = $id }
    Report 'listing-after' @{ files = (Listing); versions = $versions; kills = $kills }
    $null = Rpc 'mount-last' (@{ op = 'mount'; image = $last.image_path } + $secret)
    try {
        $root = $null
        for ($i = 0; $i -lt 60 -and -not $root; $i++) {
            $root = Get-Volume | Where-Object { $_.DriveLetter -and $_.DriveLetter -notin @('C', 'P', 'S') } |
                ForEach-Object { "$($_.DriveLetter):\" } | Where-Object { Test-Path (Join-Path $_ 'killx-marker.txt') } | Select-Object -First 1
            if (-not $root) { Start-Sleep -Seconds 1 }
        }
        Check ([bool]$root) "the last copy is mounted at $root"
        foreach ($f in Kept) {
            Check ((Get-FileHash -LiteralPath (Join-Path $root $f.Name) -Algorithm SHA256).Hash -eq (Sha $f.Bytes)) "content of $($f.Name)"
        }
        for ($j = 1; $j -le $k; $j++) {
            Check ((Get-FileHash -LiteralPath (Join-Path $root "keep\run-$j.bin") -Algorithm SHA256).Hash -eq (Sha (RunBytes $j))) "run $j's own file is in the last copy"
        }
    } finally {
        try { $null = Rpc 'unmount-last' @{ op = 'unmount'; image = $last.image_path } } catch { Say "KILLX-NOTE unmount: $($_.Exception.Message)" }
    }
    [IO.File]::WriteAllText("$work\passed.txt", 'pass')
    Report 'result' @{ pass = $true; kills = $kills; last = $last.image_path }
    Say "KILLX-PASS format=$format"
    shutdown.exe /s /t 5 | Out-Null
}

# --- A device that fails every flush (formats 'flush-*') ---------------------
# The host slides a blkdebug node under the copies' own disk on
# KILLX-ARM-FLUSH and removes it on KILLX-DISARM-FLUSH.
function FlushProbe {
    try {
        $fs = [IO.File]::Open('T:\flush-probe.bin', 'OpenOrCreate', 'ReadWrite', 'None')
        try {
            $b = New-Object byte[] 4096; (New-Object Random).NextBytes($b)
            $fs.Write($b, 0, 4096); $fs.Flush($true)
            return $true
        } finally { $fs.Dispose() }
    } catch { return $false }
}
function WaitProbe([bool]$want, [string]$text) {
    for ($i = 0; $i -lt 90 -and (FlushProbe) -ne $want; $i++) { Start-Sleep -Seconds 1 }
    Check ((FlushProbe) -eq $want) $text
}
function FlushFault([string]$id) {
    [IO.File]::WriteAllText("$work\kills.done", 'done')
    # T: holds 3 GB: what earlier attempts left there would fill it, and a
    # full disk is a different failure from the one under test.
    Get-ChildItem 'T:\' -Directory -Filter 'killx-*' | Where-Object { $_.FullName -ne $store } |
        ForEach-Object { Report "removed-$($_.Name)" @{ files = @(Get-ChildItem $_.FullName -Force | ForEach-Object { $_.Name }) }; Remove-Item $_.FullName -Recurse -Force }
    $stale = @(Names)
    if ($stale.Count) { Report 'stale-before-run1' @{ files = $stale }; Get-ChildItem -LiteralPath $store -Force | Remove-Item -Force }
    WaitProbe $true 'a flush of T: succeeds before the fault'
    $run1 = RunJob 'run1' $id
    Say 'KILLX-ARM-FLUSH'
    WaitProbe $false 'a flush of T: now fails in Windows'
    $before = @(Names)
    $run2 = Rpc 'run2' @{ op = 'run'; id = $id; timeout_secs = 1800 }
    $new = @(Names | Where-Object { $before -notcontains $_ })
    Report 'flush-failed-run' @{ status = $run2.status; error = $run2.error; new_files = $new }
    Check ($run2.status -eq 'error') "the run on a failing disk failed ($($run2.status)): $($run2.error)"
    Check (@($new | Where-Object { $_ -notlike $private }).Count -eq 0) "it published nothing ($($new -join ', '))"
    Say 'KILLX-DISARM-FLUSH'
    WaitProbe $true 'flushes of T: succeed again'
    $left = @(Pending)
    $run3 = RunJob 'run3' $id
    foreach ($image in @($run1.image_path, $run3.image_path)) {
        $v = Rpc "verify-$([IO.Path]::GetFileName($image))" (@{ op = 'verify'; image = $image } + $secret)
        Check $v.valid "$image verifies"
    }
    # The same fault switched on in the middle of a copy, not at its start.
    $before = @(Names)
    # Line 3 of the format file, when a number: how far into the copy (ms).
    $armDelay = if ($killPhase -match '^\d+$') { [int]$killPhase } else { 2000 }
    $at = Rpc 'run4-at-copy' @{ op = 'start-run-at-phase'; id = $id; phase = 'backup'; after_ms = $armDelay; timeout_secs = 600 }
    Say 'KILLX-ARM-FLUSH'
    $armedAt = [DateTime]::UtcNow
    function NextRun([int]$seconds) {
        for ($i = 0; $i -lt $seconds; $i += 2) {
            $job = Rpc "run4-wait" @{ op = 'get'; id = $id }
            if ($job.last_run -and $job.last_run.id -ne $run3.id) { return $job.last_run }
            Start-Sleep -Seconds 2
        }
        return $null
    }
    # A run that does not end on a failing disk is a finding, not the end of
    # the scenario: what happens once the disk recovers is recorded too.
    $run4 = NextRun 300
    $hung = -not $run4
    if ($hung) {
        Say "KILLX-NOTE the run hit $($at.in_phase_ms) ms into its copy has not ended in 5 minutes on the failing disk"
        Say 'KILLX-DISARM-FLUSH'
        $run4 = NextRun 600
    }
    Check ([bool]$run4) "the run hit in the middle of its copy ended (hung while failing: $hung)"
    $new = @(Names | Where-Object { $before -notcontains $_ })
    Report 'midcopy-failed-run' @{ armed_after = $at; hung_while_failing = $hung; ended_s_after_arm = ([DateTime]::UtcNow - $armedAt).TotalSeconds
        status = $run4.status; error = $run4.error; image = $run4.image_path; new_files = $new }
    if ($run4.status -eq 'success') {
        $v = Rpc 'verify-run4' (@{ op = 'verify'; image = $run4.image_path } + $secret)
        Check $v.valid "the run that outlived the fault published a copy that verifies ($($run4.image_path))"
    } else {
        Check (@($new | Where-Object { $_ -notlike $private }).Count -eq 0) "the failed run published nothing ($($new -join ', ')): $($run4.error)"
    }
    if (-not $hung) { Say 'KILLX-DISARM-FLUSH' }
    WaitProbe $true 'flushes of T: succeed again after the second fault'
    $left = @($left) + @(Pending)
    $run5 = RunJob 'run5' $id
    $v = Rpc 'verify-run5' (@{ op = 'verify'; image = $run5.image_path } + $secret)
    Check $v.valid "$($run5.image_path) verifies"
    Report 'listing-after' @{ files = (Listing); private_after_failures = $left; private_now = @(Pending) }
    Check (@(Pending).Count -eq 0) "no private file is left after the next run ($((@(Pending)) -join ', '))"
    [IO.File]::WriteAllText("$work\passed.txt", 'pass')
    Report 'result' @{ pass = $true; failed_run_error = $run2.error; runs = @($run1.image_path, $run3.image_path) }
    Say "KILLX-PASS format=$format"
    shutdown.exe /s /t 5 | Out-Null
}

# --- How Windows itself treats a disk failing writes (format 'probe-io') ----
# No product code: 300 MB written by a separate PowerShell, buffered and then
# write-through, while every write of T: fails. Each writer reports how far it
# got and when; one that is still running after its limit is recorded as hung.
function WriterScript([bool]$through) {
    $opt = if ($through) { '[IO.FileOptions]::WriteThrough' } else { '[IO.FileOptions]::None' }
    return @"
`$out = '$work\probe-$(if ($through) { 'through' } else { 'buffered' }).txt'
`$sw = [Diagnostics.Stopwatch]::StartNew(); `$n = 0
try {
    `$fs = New-Object IO.FileStream '$store\probe-$through.bin', 'Create', 'Write', 'None', 1048576, $opt
    `$buf = New-Object byte[] 1048576; (New-Object Random 7).NextBytes(`$buf)
    for (`$i = 0; `$i -lt 300; `$i++) { `$fs.Write(`$buf, 0, `$buf.Length); `$n++; [IO.File]::WriteAllText(`$out, "writing `$n MB `$(`$sw.ElapsedMilliseconds) ms") }
    `$fs.Flush(`$true); `$fs.Dispose()
    [IO.File]::WriteAllText(`$out, "done `$n MB `$(`$sw.ElapsedMilliseconds) ms")
} catch { [IO.File]::WriteAllText(`$out, "error after `$n MB `$(`$sw.ElapsedMilliseconds) ms: `$(`$_.Exception.Message)") }
"@
}
function ProbeWriter([bool]$through, [int]$limit) {
    $name = if ($through) { 'through' } else { 'buffered' }
    $script = "$work\probe-$name.ps1"
    [IO.File]::WriteAllText($script, (WriterScript $through), [Text.Encoding]::ASCII)
    Remove-Item "$work\probe-$name.txt" -ErrorAction SilentlyContinue
    $p = Start-Process powershell.exe -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$script`"" -WindowStyle Hidden -PassThru
    $null = $p.Handle
    $ended = $p.WaitForExit($limit * 1000)
    $state = if (Test-Path "$work\probe-$name.txt") { Get-Content "$work\probe-$name.txt" -Raw } else { 'no output' }
    if (-not $ended) { try { $p.Kill() } catch { }; $killed = $p.WaitForExit(30000) } else { $killed = $null }
    $row = @{ writer = $name; ended_within_limit = $ended; limit_s = $limit; state = "$state".Trim(); kill_took = $killed }
    Report "probe-$name" $row
    Say "KILLX-PROBE $name ended=$ended state=$("$state".Trim())"
    return $row
}
function ProbeIo {
    [IO.File]::WriteAllText("$work\kills.done", 'done')
    Get-ChildItem 'T:\' -Directory -Filter 'killx-*' | Where-Object { $_.FullName -ne $store } | Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
    Get-ChildItem -LiteralPath $store -Force -ErrorAction SilentlyContinue | Remove-Item -Force
    WaitProbe $true 'a flush of T: succeeds before the fault'
    $rows = @()
    $rows += ProbeWriter $false 120
    $rows += ProbeWriter $true 120
    Say 'KILLX-ARM-FLUSH'
    WaitProbe $false 'a flush of T: now fails in Windows'
    $rows += ProbeWriter $true 240
    $rows += ProbeWriter $false 240
    Say 'KILLX-DISARM-FLUSH'
    WaitProbe $true 'flushes of T: succeed again'
    Start-Sleep -Seconds 20
    foreach ($n in 'buffered', 'through') { $rows += @{ writer = "$n-after-disarm"; state = "$(Get-Content "$work\probe-$n.txt" -Raw -ErrorAction SilentlyContinue)".Trim() } }
    Report 'result' @{ pass = $true; probes = $rows }
    [IO.File]::WriteAllText("$work\passed.txt", 'pass')
    Say "KILLX-PASS format=$format"
    shutdown.exe /s /t 5 | Out-Null
}

# --- A run stuck on a failing disk, said and cancelled (formats 'stall-*') ---
# The service must say the copy stands still, a cancel must end the run while
# the disk still fails, and the thread left behind must publish nothing.
function StallCancel([string]$id) {
    [IO.File]::WriteAllText("$work\kills.done", 'done')
    Get-ChildItem 'T:\' -Directory -Filter 'killx-*' | Where-Object { $_.FullName -ne $store } | Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
    Get-ChildItem -LiteralPath $store -Force -ErrorAction SilentlyContinue | Remove-Item -Force
    WaitProbe $true 'a flush of T: succeeds before the fault'
    $run1 = RunJob 'run1' $id
    $before = @(Names)
    $at = Rpc 'run2-at-copy' @{ op = 'start-run-at-phase'; id = $id; phase = 'backup'; after_ms = 300; timeout_secs = 600 }
    Say 'KILLX-ARM-FLUSH'
    $stall = Rpc 'wait-stalled' @{ op = 'wait-stalled'; id = $id; timeout_secs = 420 }
    Check ($stall.stalled_secs -ge 120) "the service said the copy stands still ($($stall.stalled_secs) s at $($stall.blocks_done)/$($stall.blocks_total))"
    $cancelAt = [DateTime]::UtcNow
    $null = Rpc 'cancel-run2' @{ op = 'cancel-running' }
    $run2 = $null
    for ($i = 0; $i -lt 90 -and -not $run2; $i++) {
        $job = Rpc 'run2-wait' @{ op = 'get'; id = $id }
        if ($job.last_run -and $job.last_run.id -ne $run1.id) { $run2 = $job.last_run } else { Start-Sleep -Seconds 1 }
    }
    $took = ([DateTime]::UtcNow - $cancelAt).TotalSeconds
    Check ([bool]$run2) "the cancel ended the run while the disk still fails ($([int]$took) s)"
    Report 'stall-cancelled-run' @{ stall = $stall; status = $run2.status; error = $run2.error; took_s = $took }
    Check ($run2.status -eq 'cancelled') "the run is recorded as cancelled ($($run2.status)): $($run2.error)"
    Say 'KILLX-DISARM-FLUSH'
    WaitProbe $true 'flushes of T: succeed again'
    # The thread left behind wakes up now: it must discard, not publish.
    for ($i = 0; $i -lt 180 -and @(Pending).Count; $i++) { Start-Sleep -Seconds 1 }
    $new = @(Names | Where-Object { $before -notcontains $_ })
    Report 'after-left-behind' @{ new_files = $new; private = @(Pending) }
    Check (@($new | Where-Object { $_ -notlike $private }).Count -eq 0) "the thread left behind published nothing ($($new -join ', '))"
    Check (@(Pending).Count -eq 0) "and removed its unfinished file ($((@(Pending)) -join ', '))"
    $run3 = RunJob 'run3' $id
    $v = Rpc 'verify-run3' (@{ op = 'verify'; image = $run3.image_path } + $secret)
    Check $v.valid "the next run's copy verifies ($($run3.image_path))"
    [IO.File]::WriteAllText("$work\passed.txt", 'pass')
    Report 'result' @{ pass = $true; stall = $stall; cancel_took_s = $took; cancelled_error = $run2.error }
    Say "KILLX-PASS format=$format"
    shutdown.exe /s /t 5 | Out-Null
}

# --- The history cannot be written (format 'history') -----------------------
function Ask([string]$name, [string]$action, [switch]$Stopped) {
    [IO.File]::WriteAllText("$work\kill.request", (ConvertTo-Json -Compress @{ name = $name; action = $action }))
    for ($i = 0; $i -lt 1200 -and -not (Test-Path "$work\$name.json"); $i++) { Start-Sleep -Milliseconds 500 }
    Check (Test-Path "$work\$name.json") "the SYSTEM half did $action"
    # -Stopped: the request was to stop the service.
    if (-not $Stopped) { WaitService }
}
# Mode 'media': a hand-chosen Winre.wim judged by the service (InspectBaseImage),
# a build refused for a file that is no recovery image, then the medium built
# on the found base and handed raw to the host, which boots it.
function MediaBuild {
    # The SYSTEM half installs WebView2 at boot; the build needs it there.
    $runtime = 'C:\Program Files (x86)\Microsoft\EdgeWebView\Application\*\msedgewebview2.exe'
    for ($i = 0; $i -lt 600 -and -not (Get-ChildItem $runtime -ErrorAction SilentlyContinue); $i++) { Start-Sleep -Seconds 1 }
    Check ([bool](Get-ChildItem $runtime -ErrorAction SilentlyContinue)) 'the WebView2 runtime is installed'
    $bases = @(Rpc 'find-bases' @{ op = 'find-bases' })
    Check ($bases.Count -ge 1) "a base image is found ($($bases.Count))"
    $winre = $bases[0].path
    $good = Rpc 'inspect-winre' @{ op = 'inspect-base'; path = $winre }
    Check ($good.base.path -eq $winre -and -not $good.refused) "the found Winre.wim chosen by hand is accepted ($($good.base.origin))"
    $junk = "$work\not-a-wim.wim"
    [IO.File]::WriteAllBytes($junk, (New-Object byte[] 8192))
    $bad = Rpc 'inspect-junk' @{ op = 'inspect-base'; path = $junk }
    Check ([bool]$bad.refused -and $bad.refused -notlike '*Image format error*') "a file that is no WIM is refused plainly: $($bad.refused)"
    $refusal = $null
    try { $null = Rpc 'build-junk' @{ op = 'build-media'; base_wim = $junk; output_iso = "$work\junk.iso" } } catch { $refusal = $_.Exception.Message }
    Check ([bool]$refusal -and -not (Test-Path "$work\junk.iso")) "a build on it is refused before anything is made: $refusal"

    $iso = "$store\media.iso"
    Remove-Item $iso -ErrorAction SilentlyContinue
    # 'media-sxs': the runtime beside boot.wim on the medium, the layout whose
    # window does not open (0x800736B1); the SYSTEM half starts it from the
    # mounted ISO under sxstrace first.
    $inImage = $format -eq 'media'
    $report = Rpc 'build-media' @{ op = 'build-media'; base_wim = $winre; output_iso = $iso; runtime_in_image = $inImage }
    Check (Test-Path $iso) "the medium is built ($((Get-Item $iso).Length) bytes)"
    if (-not $inImage) {
        [IO.File]::WriteAllText("$work\kill.request", (ConvertTo-Json -Compress @{ name = 'sxs'; action = 'sxs'; source = $iso }))
        # Copying the runtime off the CD and three starts take minutes; the
        # SYSTEM half reports what it saw itself.
        for ($i = 0; $i -lt 1200 -and -not (Test-Path "$work\sxs.json"); $i++) { Start-Sleep -Seconds 1 }
        Check (Test-Path "$work\sxs.json") 'the SYSTEM half tried the runtime from the mounted medium'
    }
    $request = @{ name = 'rawcopy'; action = 'rawcopy'; source = $iso }
    [IO.File]::WriteAllText("$work\kill.request", (ConvertTo-Json -Compress $request))
    for ($i = 0; $i -lt 600 -and -not (Test-Path "$work\rawcopy.json"); $i++) { Start-Sleep -Milliseconds 500 }
    Check (Test-Path "$work\rawcopy.json") 'the SYSTEM half wrote the medium to the media disk'
    [IO.File]::WriteAllText("$work\kills.done", 'done')
    Say "KILLX-MEDIA length=$((Get-Item $iso).Length) sha256=$((Get-FileHash $iso).Hash)"
    [IO.File]::WriteAllText("$work\passed.txt", 'pass')
    Report 'result' @{ pass = $true; base = $good.base; refused = $bad.refused; build_refused = $refusal; report = $report }
    Say "KILLX-PASS format=$format"
    shutdown.exe /s /t 5 | Out-Null
}
# Mode 'checks9' (CHECKS 9): a hundred mounts and unmounts of one copy must
# not grow the service's handles or threads nor leave an iSCSI session; a job
# saved while C: is full is refused plainly and jobs.json stays whole.
function ServiceState([string]$name) {
    [IO.File]::WriteAllText("$work\kill.request", (ConvertTo-Json -Compress @{ name = $name; action = 'measure' }))
    for ($i = 0; $i -lt 240 -and -not (Test-Path "$work\$name.json"); $i++) { Start-Sleep -Milliseconds 500 }
    Check (Test-Path "$work\$name.json") "the SYSTEM half measured ($name)"
    $m = Get-Content "$work\$name.json" -Raw | ConvertFrom-Json
    Report $name $m
    return $m
}
function Checks9 {
    # 'nospace': only the full-disk half, without the hundred mounts.
    $mounts = $format -eq 'checks9'
    Ask 'purge' 'purge'
    $job = Rpc 'create' @{ op = 'create'; encrypted = $false; job = @{
        name = $jobName; source = 'P:'; sources = @('P:'); dest_folder = $store; image_format = 'vhdx'
        schedule = @{ kind = 'none' }; scheme = @{ method = 'full'; keep_chains = 5 }
        exclusions = @{ enabled = $false; patterns = @(); patterns_linux = @() }
        advanced = @{ validate_after = $false; retry_attempts = 0; after_backup = 'none' } } }
    if ($mounts) {
    $run = Rpc 'run1' @{ op = 'run'; id = $job.id; timeout_secs = 1800 }
    Check ($run.status -eq 'success') "a copy to mount ($($run.error))"
    $image = $run.image_path
    $cycle = {
        param($i)
        $null = Rpc "mount$i" @{ op = 'mount'; image = $image; secret = 'none' }
        $null = Rpc "unmount$i" @{ op = 'unmount'; image = $image }
        Remove-Item "$work\mount$i.*", "$work\unmount$i.*" -ErrorAction SilentlyContinue
    }
    # Two to warm up: the first mount loads what stays loaded.
    & $cycle 0; & $cycle 1
    $before = ServiceState 'before'
    for ($i = 2; $i -lt 102; $i++) { & $cycle $i }
    $after = ServiceState 'after'
    Check ($after.iscsi_sessions -eq 0) "no iSCSI session is left ($($after.iscsi_sessions))"
    Check ($after.disks -eq $before.disks) "no disk is left attached ($($before.disks) -> $($after.disks))"
    Check ($after.handles -le $before.handles + 100) "100 mounts did not grow the service's handles ($($before.handles) -> $($after.handles))"
    Check ($after.threads -le $before.threads + 5) "100 mounts did not grow the service's threads ($($before.threads) -> $($after.threads))"
    }

    # jobs.json made 1.5 MB while there is room; on a C: with 256 KB left a small
    # change still has to rewrite all of it. The request itself stays small.
    $patterns = @(for ($i = 0; $i -lt 1500; $i++) { "killx-padding-$i-" + ('x' * 1000) })
    $null = Rpc -Quiet 'save-big' @{ op = 'save'; id = $job.id; patch = @{ exclusions = @{ enabled = $false; patterns = $patterns } } }
    Remove-Item "$work\save-big.*" -ErrorAction SilentlyContinue
    $whole = ServiceState 'jobs-before'
    Ask 'fill' 'fill'
    Report 'fill' (Get-Content "$work\fill.json" -Raw | ConvertFrom-Json)
    $null = Rpc 'save-full' @{ op = 'save'; id = $job.id; expect_error = $true; patch = @{ name = "$jobName-renamed" } }
    $why = (Get-Content "$work\save-full.request.result.json" -Raw -Encoding UTF8 | ConvertFrom-Json).error
    $full = ServiceState 'jobs-full'
    Ask 'unfill' 'unfill'
    Check ($why -like '*os error 112*') "the save on a full disk is refused with the reason: $why"
    Check ($full.jobs_sha -eq $whole.jobs_sha -and -not $full.jobs_tmp) 'jobs.json is unchanged and no temporary file is left'
    $got = Rpc -Quiet 'get-after-full' @{ op = 'get'; id = $job.id }
    Check ($got.name -eq $jobName -and @($got.exclusions.patterns).Count -eq 1500) 'the job reads back as it was'
    $saved = Rpc 'save-after' @{ op = 'save'; id = $job.id; patch = @{ name = "$jobName-renamed"; exclusions = @{ enabled = $false; patterns = @() } } }
    Check ($saved.name -eq "$jobName-renamed") 'with room again the same save succeeds'

    [IO.File]::WriteAllText("$work\kills.done", 'done')
    [IO.File]::WriteAllText("$work\passed.txt", 'pass')
    Report 'result' @{ pass = $true; before = $before; after = $after; refused = $why }
    Say "KILLX-PASS format=$format"
    shutdown.exe /s /t 5 | Out-Null
}
# Mode 'refsbl' (PLAN 1, ReFS inside an encrypted source): a ReFS volume under
# BitLocker, copied locked and unlocked with '*.tmp' excluded. The copy must
# hold the kept files whole, no '*.tmp' in its catalog, and the excluded
# files' content nowhere in the image's raw bytes.
$refsSecret = 'KILLX-REFS-SECRET-'
function ImageHolds([string]$path, [string]$needle) {
    $enc = [Text.Encoding]::GetEncoding(28591)
    $want = $enc.GetBytes($needle)
    $stream = [IO.File]::OpenRead($path)
    try {
        $buf = New-Object byte[] (16MB + $want.Length)
        $carry = 0
        while (($n = $stream.Read($buf, $carry, 16MB)) -gt 0) {
            $total = $carry + $n
            $text = $enc.GetString($buf, 0, $total)
            if ($text.IndexOf($needle, [StringComparison]::Ordinal) -ge 0) { return $true }
            # The tail goes to the front: a needle split between two reads.
            $keep = [Math]::Min($want.Length - 1, $total)
            [Array]::Copy($buf, $total - $keep, $buf, 0, $keep)
            $carry = $keep
        }
    } finally { $stream.Dispose() }
    return $false
}
function RefsCopyChecks([string]$tag, $run, $kept) {
    Check ($run.status -eq 'success') "$($tag): the copy succeeded ($($run.error))"
    Check ([string]::IsNullOrEmpty($run.file_scan_skipped)) "$($tag): exclusions were applied, not skipped ($($run.file_scan_skipped))"
    Check ($run.excluded_files -eq 12) "$($tag): twelve files excluded ($($run.excluded_files))"
    $null = Rpc "$tag-mount" @{ op = 'mount'; image = $run.image_path; secret = 'none' }
    try {
        $root = $null
        for ($i = 0; $i -lt 60 -and -not $root; $i++) {
            $root = Get-Volume | Where-Object { $_.DriveLetter -and $_.DriveLetter -notin @('C', 'P', 'S', 'R') } |
                ForEach-Object { "$($_.DriveLetter):\" } | Where-Object { Test-Path (Join-Path $_ 'refs-marker.txt') } | Select-Object -First 1
            if (-not $root) { Start-Sleep -Seconds 1 }
        }
        Check ([bool]$root) "$($tag): the copy mounts ($root, $((Get-Volume -FilePath $root -ErrorAction SilentlyContinue).FileSystem))"
        $bad = @($kept.PSObject.Properties | Where-Object { (Get-FileHash -LiteralPath (Join-Path $root $_.Name)).Hash -ne $_.Value } | ForEach-Object { $_.Name })
        Check ($bad.Count -eq 0) "$($tag): every kept file is whole ($($bad -join ', '))"
        $left = @(Get-ChildItem -LiteralPath $root -Recurse -Force -File -Filter '*.tmp' -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName })
        Check ($left.Count -eq 0) "$($tag): no excluded file in the copy's catalog ($($left -join ', '))"
    } finally {
        try { $null = Rpc "$tag-unmount" @{ op = 'unmount'; image = $run.image_path } } catch { Say "KILLX-NOTE unmount: $($_.Exception.Message)" }
    }
    Check (-not (ImageHolds $run.image_path $refsSecret)) "$($tag): the excluded content is nowhere in the image's bytes"
}
function RefsBitLocker {
    Ask 'purge' 'purge'
    Ask 'refs-prepare' 'refs-prepare'
    $prep = Get-Content "$work\refs-prepare.json" -Raw | ConvertFrom-Json
    Report 'refs-prepare' $prep
    $wantFs = if ($format -eq 'ntfsbl') { 'NTFS' } else { 'ReFS' }
    Check ($prep.filesystem -eq $wantFs -and $prep.lock -eq 'Locked') "a locked BitLocker volume with $wantFs inside ($($prep.filesystem), $($prep.method), $($prep.lock))"
    [IO.File]::WriteAllText("$work\refs-password.txt", 'KillX-Refs-2026!')
    $base = @{ schedule = @{ kind = 'none' }; image_format = 'vhdx'; requires_password = $true
        source = 'R:'; sources = @('R:'); dest_folder = $store
        scheme = @{ method = 'full'; keep_chains = 5 }
        exclusions = @{ enabled = $true; patterns = @('*.tmp'); patterns_linux = @() }
        advanced = @{ validate_after = $false; retry_attempts = 0; after_backup = 'none' } }
    $null = Rpc 'refs-password' @{ op = 'provide-source-password'; volume = 'R:'; source_password_file = "$work\refs-password.txt"; remember = $true }
    $locked = Rpc 'create-locked' @{ op = 'create'; encrypted = $false; job = ($base + @{ name = "$jobName-locked" }) }
    $run1 = Rpc 'run-locked' @{ op = 'run'; id = $locked.id; timeout_secs = 1800 }
    RefsCopyChecks 'locked' $run1 $prep.kept
    Ask 'refs-unlock' 'refs-unlock'
    $open = Rpc 'create-open' @{ op = 'create'; encrypted = $false; job = ($base + @{ name = "$jobName-open" }) }
    $run2 = Rpc 'run-open' @{ op = 'run'; id = $open.id; timeout_secs = 1800 }
    RefsCopyChecks 'unlocked' $run2 $prep.kept
    Ask 'refs-detach' 'refs-detach'

    [IO.File]::WriteAllText("$work\kills.done", 'done')
    [IO.File]::WriteAllText("$work\passed.txt", 'pass')
    Report 'result' @{ pass = $true; locked = $run1; unlocked = $run2 }
    Say "KILLX-PASS format=$format"
    shutdown.exe /s /t 5 | Out-Null
}

# Mode 'deletejob' (CHECKS 9): a two-source job that changed folders and ran
# past the activity journal's 200 runs is deleted with its files: every copy
# of it goes, from both folders; another job's copies and a stray file stay;
# nothing is left unreported.
function DeleteJob {
    Ask 'purge' 'purge'
    Ask 'qvol' 'qvol'
    $a = "$store\A"; $b = "$store\B"
    New-Item -ItemType Directory -Force $a, $b | Out-Null
    foreach ($f in $a, $b) { [IO.File]::WriteAllText("$f\keep-me.txt", 'not a copy') }
    $base = @{ schedule = @{ kind = 'none' }; image_format = 'vhdx'
        exclusions = @{ enabled = $false; patterns = @(); patterns_linux = @() }
        advanced = @{ validate_after = $false; retry_attempts = 0; after_backup = 'none' } }
    $other = Rpc 'create-other' @{ op = 'create'; encrypted = $false; job = ($base + @{ name = 'KillXotherjob'; source = 'Q:'; sources = @('Q:'); dest_folder = $a; scheme = @{ method = 'full'; keep_chains = 5 } }) }
    $r = Rpc 'other-a' @{ op = 'run'; id = $other.id; timeout_secs = 900 }
    Check ($r.status -eq 'success') "another job's copy in A ($($r.error))"
    $null = Rpc 'other-to-b' @{ op = 'save'; id = $other.id; patch = @{ dest_folder = $b } }
    $r = Rpc 'other-b' @{ op = 'run'; id = $other.id; timeout_secs = 900 }
    Check ($r.status -eq 'success') "another job's copy in B ($($r.error))"

    $own = Rpc 'create-own' @{ op = 'create'; encrypted = $false; job = ($base + @{ name = 'KillXownjob'; source = 'P:'; sources = @('P:', 'Q:'); dest_folder = $a
        scheme = @{ method = 'incremental'; only_incremental = $false; full_every_n = 20; keep_chains = 2 } }) }
    for ($i = 1; $i -le 3; $i++) {
        $r = Rpc -Quiet "own-a$i" @{ op = 'run'; id = $own.id; timeout_secs = 900 }
        Check ($r.status -eq 'success') "own run $i in A ($($r.error))"
    }
    $inA = @(Get-ChildItem -LiteralPath $a -File | Where-Object { $_.Name -like 'KillXownjob_*' } | ForEach-Object { $_.Name })
    Check ($inA.Count -eq 6) "two sources, three runs: six copies in A ($($inA.Count))"
    $moved = Rpc 'own-to-b' @{ op = 'save'; id = $own.id; patch = @{ dest_folder = $b } }
    Check (@($moved.previous_dest_folders) -contains $a) "the job remembers it wrote into A ($(@($moved.previous_dest_folders) -join ', '))"
    for ($i = 1; $i -le 202; $i++) {
        $r = Rpc -Quiet "own-b$i" @{ op = 'run'; id = $own.id; timeout_secs = 900 }
        if ($r.status -ne 'success') { Check $false "own run $i in B ($($r.error))" }
        Remove-Item "$work\own-b$i.*" -ErrorAction SilentlyContinue
        if ($i % 25 -eq 0) { Say "KILLX-NOTE $i runs in B" }
    }
    $activity = @(Rpc -Quiet 'activity' @{ op = 'activity' })
    # Every path in every run, as JSON spells it (backslashes doubled).
    $journal = ConvertTo-Json -InputObject $activity -Depth 8 -Compress
    $needle = ($a + '\').Replace('\', '\\')
    Check (-not $journal.Contains($needle)) "the activity journal no longer names folder A ($($activity.Count) runs)"

    $report = Rpc 'delete-own' @{ op = 'delete'; id = $own.id; files = $true }
    Report 'delete-own' $report
    $ownLeft = @(Get-ChildItem -LiteralPath $a, $b -File | Where-Object { $_.Name -like 'KillXownjob_*' } | ForEach-Object { $_.FullName })
    Check ($ownLeft.Count -eq 0) "every copy of the job is gone from both folders ($($ownLeft -join ', '))"
    $otherLeft = @(Get-ChildItem -LiteralPath $a, $b -File | Where-Object { $_.Name -like 'KillXotherjob_*' -and $_.Name -match '\.vhdx$' })
    Check ($otherLeft.Count -eq 2) "the other job's copies stay ($($otherLeft.Count))"
    Check ((Test-Path "$a\keep-me.txt") -and (Test-Path "$b\keep-me.txt")) 'the stray files stay'
    Check (@($report.kept).Count -eq 0) "nothing was left behind unreported ($(@($report.kept) | ConvertTo-Json -Compress))"
    Check (@($report.folders) -contains $a) "folder A was searched ($(@($report.folders) -join ', '))"
    Say "KILLX-NOTE deleted=$(@($report.deleted).Count) history_truncated=$($report.history_truncated)"
    Ask 'qvol-detach' 'qvol-detach'

    [IO.File]::WriteAllText("$work\kills.done", 'done')
    [IO.File]::WriteAllText("$work\passed.txt", 'pass')
    Report 'result' @{ pass = $true; deleted = @($report.deleted).Count; truncated = $report.history_truncated; folders = $report.folders }
    Say "KILLX-PASS format=$format"
    shutdown.exe /s /t 5 | Out-Null
}

# Mode 'fatx': a FAT32 volume copied with *.tmp excluded. The files are gone
# from the copy, not zeroed; the freed clusters are counted free in FSInfo
# (Windows takes the free space of a FAT32 volume from there) and chkdsk finds
# nothing to fix.
function FatX {
    Ask 'fvol' 'fvol'
    $src = Get-Content "$work\fvol.json" -Raw | ConvertFrom-Json
    Check ($src.filesystem -eq 'FAT32') "the source is FAT32 ($($src.filesystem), cluster $($src.cluster))"
    $freed = 0; foreach ($len in @($src.tmp)) { $freed += [math]::Ceiling($len / $src.cluster) * $src.cluster }
    $job = Rpc 'create' @{ op = 'create'; encrypted = $false; job = @{ name = $jobName; source = 'F:'; sources = @('F:'); dest_folder = $store
        image_format = 'vhdx'; schedule = @{ kind = 'none' }
        scheme = @{ method = 'incremental'; only_incremental = $false; full_every_n = 1; keep_chains = 1 }
        exclusions = @{ enabled = $true; patterns = @('*.tmp'); patterns_linux = @() }
        advanced = @{ validate_after = $false; retry_attempts = 0; after_backup = 'none' } } }
    $run = Rpc 'run1' @{ op = 'run'; id = $job.id; timeout_secs = 1800 }
    Report 'run1' $run
    Check ($run.status -eq 'success') "the copy of F: succeeded ($($run.error))"
    $null = Rpc 'mount1' @{ op = 'mount'; image = $run.image_path }
    try {
        $root = $null
        for ($i = 0; $i -lt 60 -and -not $root; $i++) {
            $root = Get-Volume | Where-Object { $_.DriveLetter -and $_.DriveLetter -notin @('C', 'F', 'P', 'S') } |
                ForEach-Object { "$($_.DriveLetter):\" } | Where-Object { Test-Path (Join-Path $_ 'fat-marker.txt') } | Select-Object -First 1
            if (-not $root) { Start-Sleep -Seconds 1 }
        }
        Check ([bool]$root) "the copy is mounted at $root"
        $left = @(Get-ChildItem $root -Recurse -File -Filter '*.tmp' | ForEach-Object { $_.FullName })
        Check ($left.Count -eq 0) "no excluded file is left in the copy ($($left -join ', '))"
        foreach ($p in $src.kept.PSObject.Properties) {
            Check ((Get-FileHash -LiteralPath (Join-Path $root $p.Name)).Hash -eq $p.Value) "kept $($p.Name) is intact"
        }
        $vol = Get-Volume -DriveLetter $root.Substring(0, 1)
        $want = $src.free + $freed
        Say "KILLX-NOTE free source=$($src.free) freed=$freed copy=$($vol.SizeRemaining) want=$want"
        Check ($vol.SizeRemaining -eq $want) "the copy counts the freed clusters free (FSInfo): $($vol.SizeRemaining) = $want"
        $chk = [string[]]@(& chkdsk.exe $root.TrimEnd('\') 2>&1 | ForEach-Object { "$_" })
        $code = $LASTEXITCODE
        Report 'chkdsk' @{ exit = $code; output = $chk }
        Check ($code -eq 0) "chkdsk of the copy finds nothing to fix (exit $code)"
    } finally {
        try { $null = Rpc 'unmount1' @{ op = 'unmount'; image = $run.image_path } } catch { Say "KILLX-NOTE unmount: $($_.Exception.Message)" }
    }
    Ask 'fvol-detach' 'fvol-detach'
    [IO.File]::WriteAllText("$work\kills.done", 'done')
    [IO.File]::WriteAllText("$work\passed.txt", 'pass')
    Report 'result' @{ pass = $true; image = $run.image_path; freed = $freed }
    Say "KILLX-PASS format=$format"
    shutdown.exe /s /t 5 | Out-Null
}

# Mode 'settings6a' (PLAN 6a): config.toml is the service's. A file a user
# owned is taken over at start and its mirror folder dropped unless it is
# theirs; the user can no longer write the file; through the service a folder
# of SYSTEM's or a path through a junction is refused, one's own is used; and
# a folder swapped for a junction afterwards gets nothing.
function SettingsRefused([string]$name, $patch) {
    $null = Rpc $name @{ op = 'save-settings'; expect_error = $true; patch = $patch }
    return (Get-Content "$work\$name.request.result.json" -Raw -Encoding UTF8 | ConvertFrom-Json).error
}
function Settings6a {
    $planted = 'C:\Windows\Temp\obk-planted'
    $cfg = 'C:\ProgramData\OwnBackup\config.toml'
    $me = [Security.Principal.WindowsIdentity]::GetCurrent()
    $elevated = ([Security.Principal.WindowsPrincipal]$me).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    Say "KILLX-NOTE user=$($me.User.Value) elevated=$elevated"
    Ask 'plant' 'plant'
    $p = Get-Content "$work\plant.json" -Raw | ConvertFrom-Json
    Check ($p.owner_before -eq $me.User.Value) "the planted config.toml was the user's ($($p.owner_before))"
    # SYSTEM's files are owned by the Administrators group by default.
    Check ($p.owner_after -in @('S-1-5-18', 'S-1-5-32-544')) "the service took config.toml over at start ($($p.owner_after))"
    $s = Rpc 'get1' @{ op = 'get-settings' }
    Check ($s.log_mirror_dir -eq '') "the planted mirror folder was switched off ('$($s.log_mirror_dir)')"
    Check ($s.log_level -eq 'debug') "the rest of the planted file was kept ($($s.log_level))"
    Check (-not (Test-Path "$planted\own-backup-service.log")) 'nothing was written into the planted folder'
    $denied = $false
    try { [IO.File]::AppendAllText($cfg, "`r`n# x") } catch { $denied = $true }
    Check $denied 'the user can no longer write config.toml'

    $why = SettingsRefused 'sys-dir' @{ log_mirror_dir = $planted }
    if ($elevated) { Check (-not $why) "an elevated administrator may name any folder ($why)" }
    else { Check ([bool]$why) "a folder of SYSTEM's is refused: $why" }
    $mine = "$env:USERPROFILE\obk-mirror"
    # What an earlier attempt left: the junction goes as a link only
    # (Remove-Item -Recurse on one empties its target in PowerShell 5.1).
    foreach ($d in @($mine, "$mine-real")) {
        $item = Get-Item -LiteralPath $d -Force -ErrorAction SilentlyContinue
        if (-not $item) { continue }
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { [IO.Directory]::Delete($d, $false) } else { Remove-Item -LiteralPath $d -Recurse -Force }
    }
    New-Item -ItemType Directory -Force $mine | Out-Null
    # Probe: does a stop leave the process behind after a log level change
    # alone (no mirror)?
    $null = Rpc 'level' @{ op = 'save-settings'; patch = @{ log_level = 'info' } }
    Start-Sleep -Seconds 5
    Ask 'svc-stop-level' 'svc-stop' -Stopped
    $probe = Get-Content "$work\svc-stop-level.json" -Raw | ConvertFrom-Json
    Say "KILLX-NOTE after a level change: status=$($probe.status_after_stop) alive=$(@($probe.alive_after_stop) -join ',') exit=$($probe.exit_code)"
    if (@($probe.alive_after_stop).Count) { Get-Process own-backup-service -ErrorAction SilentlyContinue | Wait-Process -Timeout 120 -ErrorAction SilentlyContinue }
    Ask 'svc-start-level' 'svc-start'
    $s = Rpc 'save-mine' @{ op = 'save-settings'; patch = @{ log_mirror_dir = $mine } }
    Check ($s.log_mirror_dir -eq $mine) "a folder of one's own is accepted ($($s.log_mirror_dir))"
    $log = "$mine\own-backup-service.log"
    for ($i = 0; $i -lt 30 -and -not (Test-Path $log); $i++) { $null = Rpc 'poke' @{ op = 'status' } -Quiet; Start-Sleep -Seconds 1 }
    Check (Test-Path $log) 'the service writes its log mirror into it'

    # The folder swapped for a junction while the service is down: at its
    # next start the path leads to SYSTEM's folder, and nothing is written.
    Ask 'svc-stop' 'svc-stop' -Stopped
    $stop = Get-Content "$work\svc-stop.json" -Raw | ConvertFrom-Json
    Report 'svc-stop-state' $stop
    Say "KILLX-NOTE with the mirror on: status=$($stop.status_after_stop) alive=$(@($stop.alive_after_stop) -join ',') exit=$($stop.exit_code)"
    if (@($stop.alive_after_stop).Count) { Get-Process own-backup-service -ErrorAction SilentlyContinue | Wait-Process -Timeout 120 -ErrorAction SilentlyContinue }
    $late = @(Get-Process own-backup-service -ErrorAction SilentlyContinue).Count
    Check (@($probe.alive_after_stop).Count -eq 0 -and @($stop.alive_after_stop).Count -eq 0) "the service process ends with its stop (level probe alive=$(@($probe.alive_after_stop).Count), mirror alive=$(@($stop.alive_after_stop).Count), still alive 2 min later=$late)"
    Rename-Item $mine "$mine-real"
    # mklink: New-Item -ItemType Junction (PowerShell 5.1) wants to read the
    # target, which is SYSTEM's; a junction itself needs no such right.
    & cmd.exe /c mklink /J "$mine" "$planted" | Out-Null
    Check ((Get-Item -LiteralPath $mine -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) "the folder is now a junction to $planted"
    Ask 'svc-start' 'svc-start'
    for ($i = 0; $i -lt 8; $i++) { $null = Rpc 'poke2' @{ op = 'status' } -Quiet; Start-Sleep -Seconds 1 }
    $leaked = @(Get-ChildItem $planted -Force -ErrorAction SilentlyContinue | ForEach-Object { $_.Name })
    Check ($leaked.Count -eq 0) "through a junction the service writes nothing ($($leaked -join ', '))"
    $why = SettingsRefused 'junction' @{ log_mirror_dir = $mine }
    Check ([bool]$why) "a path through a junction is refused: $why"
    $s = Rpc 'off' @{ op = 'save-settings'; patch = @{ log_mirror_dir = '' } }
    Check ($s.log_mirror_dir -eq '') 'the mirror is switched off again'
    [IO.Directory]::Delete($mine, $false)
    Rename-Item "$mine-real" $mine

    [IO.File]::WriteAllText("$work\kills.done", 'done')
    [IO.File]::WriteAllText("$work\passed.txt", 'pass')
    Report 'result' @{ pass = $true; elevated = $elevated }
    Say "KILLX-PASS format=$format"
    shutdown.exe /s /t 5 | Out-Null
}

# Mode 'aliases' (CHECKS 9): one source named by its letter, its volume GUID
# path and a mount point continues one chain; a destination on that volume is
# refused whichever name either side uses, one on another volume is not.
function SaveRefused([string]$name, $job) {
    $null = Rpc $name @{ op = 'create'; encrypted = $false; expect_error = $true; job = $job }
    return (Get-Content "$work\$name.request.result.json" -Raw -Encoding UTF8 | ConvertFrom-Json).error
}
function Aliases {
    Ask 'purge' 'purge'
    $guid = (Get-Volume -DriveLetter P).Path
    Check ($guid -like '\\?\Volume{*}\') "the source volume's GUID path ($guid)"
    $mnt = 'C:\killx-mnt\'
    $base = @{ schedule = @{ kind = 'none' }; image_format = 'vhdx'
        scheme = @{ method = 'incremental'; only_incremental = $true; keep_chains = 5 }
        exclusions = @{ enabled = $false; patterns = @(); patterns_linux = @() }
        advanced = @{ validate_after = $false; retry_attempts = 0; after_backup = 'none' } }
    $job = Rpc 'create' @{ op = 'create'; encrypted = $false; job = ($base + @{ name = $jobName; source = 'P:'; sources = @('P:'); dest_folder = $store }) }
    $run1 = Rpc 'run1' @{ op = 'run'; id = $job.id; timeout_secs = 1800 }
    Check ($run1.status -eq 'success' -and $run1.backup_kind -eq 'full') "by letter: a full ($($run1.backup_kind) $($run1.error))"
    $null = Rpc 'to-guid' @{ op = 'save'; id = $job.id; patch = @{ source = $guid; sources = @($guid) } }
    $run2 = Rpc 'run2' @{ op = 'run'; id = $job.id; timeout_secs = 1800 }
    Check ($run2.status -eq 'success' -and $run2.backup_kind -eq 'incremental') "by GUID path: an increment of the same chain ($($run2.backup_kind) $($run2.error))"
    Ask 'mountpoint' 'mountpoint'
    $null = Rpc 'to-mount' @{ op = 'save'; id = $job.id; patch = @{ source = $mnt; sources = @($mnt) } }
    $run3 = Rpc 'run3' @{ op = 'run'; id = $job.id; timeout_secs = 1800 }
    Check ($run3.status -eq 'success' -and $run3.backup_kind -eq 'incremental') "by mount point: an increment of the same chain ($($run3.backup_kind) $($run3.error))"
    $token = 'V' + $guid.Substring(11, 8).ToUpper()
    Check ((Split-Path $run3.image_path -Leaf) -like "*_$($token)_inc_*") "a mount point is named by its volume, not by C ($($run3.image_path))"
    $public = @(Get-ChildItem -LiteralPath $store -File | Where-Object { $_.Name -match '\.(vhdx|avhdx)$' } | ForEach-Object { $_.Name })
    Check ($public.Count -eq 3 -and @($public | Where-Object { $_ -like '*_full_*' }).Count -eq 1) "one chain: one full and two increments ($($public -join ', '))"
    $versions = Rpc 'versions' @{ op = 'versions'; id = $job.id }

    $cases = @(
        @{ n = 'guid-on-self'; source = $guid; dest = 'P:\killx-dest' },
        @{ n = 'mount-on-self'; source = $mnt; dest = 'P:\killx-dest' },
        @{ n = 'letter-into-mount'; source = 'P:'; dest = ($mnt + 'killx-dest') },
        @{ n = 'guid-into-mount'; source = $guid; dest = ($mnt + 'killx-dest') }
    )
    $refusals = [ordered]@{}
    foreach ($c in $cases) {
        $why = SaveRefused "refuse-$($c.n)" ($base + @{ name = "$jobName-$($c.n)"; source = $c.source; sources = @($c.source); dest_folder = $c.dest })
        Check ([bool]$why) "a destination on the source is refused ($($c.n)): $why"
        $refusals[$c.n] = $why
    }
    $other = Rpc 'allow-other' @{ op = 'create'; encrypted = $false; job = ($base + @{ name = "$jobName-other"; source = $mnt; sources = @($mnt); dest_folder = 'C:\killx-other' }) }
    Check ([bool]$other.id) 'a mount-point source with its copies on another volume is accepted'
    Ask 'unmountpoint' 'unmountpoint'

    [IO.File]::WriteAllText("$work\kills.done", 'done')
    [IO.File]::WriteAllText("$work\passed.txt", 'pass')
    Report 'result' @{ pass = $true; guid = $guid; runs = @($run1.image_path, $run2.image_path, $run3.image_path); refusals = $refusals; versions = $versions }
    Say "KILLX-PASS format=$format"
    shutdown.exe /s /t 5 | Out-Null
}
function HistoryUnwritable([string]$id) {
    Get-ChildItem -LiteralPath $store -Force -ErrorAction SilentlyContinue | Remove-Item -Force
    Ask 'block' 'block'
    $run1 = RunJob 'run1' $id
    Check (-not [string]::IsNullOrEmpty($run1.record_error)) "the run says it is not in the history: $($run1.record_error)"
    $job = Rpc 'job-after-run1' @{ op = 'get'; id = $id }
    Check ($job.last_run.id -eq $run1.id -and $job.last_run.record_error) 'the job shows the run with the mark'
    Ask 'restart-1' 'restart'
    $job = Rpc 'job-after-restart' @{ op = 'get'; id = $id }
    $versions = Rpc 'versions-after-restart' @{ op = 'versions'; id = $id }
    Report 'after-restart' @{ last_run = $job.last_run; versions = $versions }
    Check ($null -eq $job.last_run) 'a restart before the history could be written loses the record, as the message says'
    Check (Test-Path -LiteralPath $run1.image_path) "the copy itself stays on disk ($($run1.image_path))"
    Ask 'unblock' 'unblock'
    $run2 = RunJob 'run2' $id
    Check ($run2.backup_kind -eq 'incremental') "the next run continues the chain on disk ($($run2.backup_kind))"
    Check ([string]::IsNullOrEmpty($run2.record_error)) 'and is in the history'
    Ask 'restart-2' 'restart'
    $job = Rpc 'job-after-restart-2' @{ op = 'get'; id = $id }
    Check ($job.last_run.id -eq $run2.id -and -not $job.last_run.record_error) 'the recorded run survives a restart'
    $v = Rpc 'verify-run2' @{ op = 'verify'; image = $run2.image_path; secret = 'none' }
    Check $v.valid "the increment verifies with its parent ($($run2.image_path))"
    [IO.File]::WriteAllText("$work\kills.done", 'done')
    [IO.File]::WriteAllText("$work\passed.txt", 'pass')
    Report 'result' @{ pass = $true; run1 = $run1.image_path; run2 = $run2.image_path; message = $run1.record_error }
    Say "KILLX-PASS format=$format"
    shutdown.exe /s /t 5 | Out-Null
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

    # 'view': the VM shown in a window for a person to look at; nothing runs
    # and nothing shuts it down.
    if ($format -eq 'view') { Say 'KILLX-VIEW the stand is idle'; return }

    # 'refsprobe': can this Windows make a ReFS volume? A small VHDX through
    # diskpart, formatted ReFS, a file written and read back.
    if ($format -eq 'refsprobe' -and -not $User) {
        $vhd = 'C:\Lab\refs-probe.vhdx'
        $script = "$work\refs-probe.txt"
        if (Test-Path $vhd) { "select vdisk file=$vhd`r`ndetach vdisk" | Set-Content $script -Encoding ASCII; & diskpart.exe /s $script | Out-Null; Remove-Item $vhd -Force }
        "create vdisk file=$vhd maximum=2048 type=expandable`r`nselect vdisk file=$vhd`r`nattach vdisk`r`ncreate partition primary`r`nformat fs=refs quick label=REFSPROBE`r`nassign letter=R" | Set-Content $script -Encoding ASCII
        $out = [string[]]@(& diskpart.exe /s $script | ForEach-Object { "$_" })
        $vol = Get-Volume -DriveLetter R -ErrorAction SilentlyContinue
        $rw = $false
        if ($vol) { [IO.File]::WriteAllText('R:\probe.txt', 'refs ok'); $rw = ([IO.File]::ReadAllText('R:\probe.txt') -eq 'refs ok') }
        $os = Get-CimInstance Win32_OperatingSystem
        Report 'refsprobe' @{ edition = $os.Caption; build = $os.BuildNumber; filesystem = "$($vol.FileSystem)"; size = $vol.Size; read_write = $rw; diskpart = $out }
        Say "KILLX-REFS filesystem=$($vol.FileSystem) read_write=$rw edition=$($os.Caption) build=$($os.BuildNumber)"
        "select vdisk file=$vhd`r`ndetach vdisk" | Set-Content $script -Encoding ASCII; & diskpart.exe /s $script | Out-Null
        Remove-Item $vhd -Force -ErrorAction SilentlyContinue
        Say "KILLX-PASS format=$format"
        shutdown.exe /s /t 5 | Out-Null
        return
    }
    if ($format -eq 'refsprobe') { return }

    # 'vhdxlab' (CHECKS 12): VHDX chains made on Linux, as Windows sees them,
    # plus a chain Windows wrote (raw and through NTFS with TRIM) folded by
    # our code. vhdx_lab.exe prints 'VL PASS/FAIL <step>'.
    if ($format -eq 'vhdxlab' -and -not $User) {
        $run = 'C:\vl-run'
        if (Test-Path $run) { Remove-Item $run -Recurse -Force }
        Copy-Item "$lab\vl\sets" $run -Recurse
        Get-ChildItem $run -Recurse -File | ForEach-Object { $_.IsReadOnly = $false }
        $tool = "$lab\vl\vhdx_lab.exe"
        $fails = 0
        function VlRun([string[]]$argv) {
            $lines = [string[]]@(& $tool @argv 2>&1 | ForEach-Object { "$_" })
            foreach ($l in $lines) { Say "VL> $l"; if ($l -like 'VL FAIL*') { $script:fails++ } }
            if ($LASTEXITCODE -ne 0 -and -not ($lines -like 'VL FAIL*')) { $script:fails++; Say "VL> exit $LASTEXITCODE" }
            return $lines
        }
        $null = VlRun @('check', $run)

        # A chain Windows writes through NTFS, with files deleted and trimmed:
        # Windows read vs our read, before and after our fold and repoint.
        $y = "$run\y"; New-Item -ItemType Directory -Force $y | Out-Null
        function Letter([string]$file) {
            $part = Get-DiskImage -ImagePath $file | Get-Disk | Get-Partition | Where-Object { $_.Type -eq 'Basic' } | Select-Object -First 1
            if (-not $part.DriveLetter) { $part | Add-PartitionAccessPath -AssignDriveLetter; $part = Get-DiskImage -ImagePath $file | Get-Disk | Get-Partition | Where-Object { $_.Type -eq 'Basic' } | Select-Object -First 1 }
            return "$($part.DriveLetter):"
        }
        function Fill([string]$path, [int]$mb, [int]$seedValue) {
            $rng = New-Object Random $seedValue; $buf = New-Object byte[] (1MB)
            $fs = [IO.File]::Create($path)
            try { for ($i = 0; $i -lt $mb; $i++) { $rng.NextBytes($buf); $fs.Write($buf, 0, $buf.Length) }; $fs.Flush($true) } finally { $fs.Dispose() }
        }
        try {
            "create vdisk file=$y\base.vhdx maximum=64 type=expandable" | Set-Content "$work\vl-dp1.txt" -Encoding ASCII
            & diskpart.exe /s "$work\vl-dp1.txt" | Out-Null
            Mount-DiskImage -ImagePath "$y\base.vhdx" | Out-Null
            $disk = Get-DiskImage -ImagePath "$y\base.vhdx" | Get-Disk
            Initialize-Disk -Number $disk.Number -PartitionStyle GPT
            New-Partition -DiskNumber $disk.Number -UseMaximumSize -AssignDriveLetter | Format-Volume -FileSystem NTFS -Confirm:$false | Out-Null
            $d = Letter "$y\base.vhdx"
            Fill "$d\a.bin" 16 1; Fill "$d\b.bin" 16 2
            Dismount-DiskImage -ImagePath "$y\base.vhdx" | Out-Null
            "create vdisk file=$y\c1.vhdx parent=$y\base.vhdx" | Set-Content "$work\vl-dp2.txt" -Encoding ASCII
            & diskpart.exe /s "$work\vl-dp2.txt" | Out-Null
            Mount-DiskImage -ImagePath "$y\c1.vhdx" | Out-Null
            $d = Letter "$y\c1.vhdx"
            Remove-Item "$d\a.bin"; Optimize-Volume -DriveLetter $d.TrimEnd(':') -ReTrim
            Fill "$d\c.bin" 4 3
            Dismount-DiskImage -ImagePath "$y\c1.vhdx" | Out-Null
            "create vdisk file=$y\c2.vhdx parent=$y\c1.vhdx" | Set-Content "$work\vl-dp3.txt" -Encoding ASCII
            & diskpart.exe /s "$work\vl-dp3.txt" | Out-Null
            Mount-DiskImage -ImagePath "$y\c2.vhdx" | Out-Null
            $d = Letter "$y\c2.vhdx"
            Remove-Item "$d\b.bin"; Optimize-Volume -DriveLetter $d.TrimEnd(':') -ReTrim
            Dismount-DiskImage -ImagePath "$y\c2.vhdx" | Out-Null
            Say 'VL> VL PASS y-make'
        } catch { $fails++; Say "VL> VL FAIL y-make: $($_.Exception.Message) line=$($_.InvocationInfo.ScriptLineNumber)" }
        $before = VlRun @('cmp', "$y\c2.vhdx")
        $null = VlRun @('cmp', "$y\c1.vhdx")
        $null = VlRun @('fold', "$y\base.vhdx", "$y\c1.vhdx")
        $null = VlRun @('repoint', "$y\c2.vhdx", "$y\base.vhdx")
        $after = VlRun @('cmp', "$y\c2.vhdx")
        $h1 = (@($before | Where-Object { $_ -like 'VL PASS cmp*' }) -join '').Split(' ')[-1]
        $h2 = (@($after | Where-Object { $_ -like 'VL PASS cmp*' }) -join '').Split(' ')[-1]
        if ($h1 -and $h1 -eq $h2) { Say "VL> VL PASS y-fold-same $h1" } else { $fails++; Say "VL> VL FAIL y-fold-same before=$h1 after=$h2" }

        if ($fails -eq 0) { Say "KILLX-PASS format=$format" } else { Say "KILLX-FAIL vhdxlab fails=$fails" }
        shutdown.exe /s /t 5 | Out-Null
        return
    }
    if ($format -eq 'vhdxlab') { return }

    # 'failrestart': the service is set to restart after a crash; a killed
    # service comes back by itself, without anyone starting it.
    if ($format -eq 'failrestart') {
        if (-not $User) {
            # This build first: the setting is written by its installer.
            $want = (Get-FileHash "$lab\own-backup-install.exe").Hash
            $have = if (Test-Path "$base\installed.sha256") { (Get-Content "$base\installed.sha256" -Raw).Trim() } else { '' }
            if ($want -ne $have) {
                $setup = Start-Process "$lab\own-backup-install.exe" -ArgumentList '/S' -Wait -PassThru
                Check ($setup.ExitCode -eq 0) "upgrade installer exit $($setup.ExitCode)"
                [IO.File]::WriteAllText("$base\installed.sha256", $want)
            }
            WaitService
            $q = [string[]]@(& sc.exe qfailure OwnBackup | ForEach-Object { "$_" })
            $old = (Get-Process own-backup-service).Id
            Stop-Process -Id $old -Force
            Wait-Process -Id $old -Timeout 180 -ErrorAction SilentlyContinue
            $back = $null
            for ($i = 0; $i -lt 90 -and -not $back; $i++) {
                Start-Sleep -Seconds 1
                $p = Get-Process own-backup-service -ErrorAction SilentlyContinue
                if ($p -and $p.Id -ne $old -and (Get-Service OwnBackup).Status -eq 'Running') { $back = $p.Id }
            }
            Report 'failrestart' @{ qfailure = $q; old_pid = $old; new_pid = $back; seconds = $i }
            if ($back) { Say "KILLX-OK the killed service came back by itself in $i s (pid $old -> $back)"; Say "KILLX-PASS format=$format" }
            else { Say "KILLX-FAIL phase=run line=0 reason=the killed service did not come back by itself" }
            shutdown.exe /s /t 5 | Out-Null
        }
        return
    }

    # 'bootlog': why did Windows restart last time? The System log's crash and
    # unexpected-shutdown events, bugcheck details and minidumps.
    if ($format -eq 'bootlog') {
        if (-not $User) {
            $events = [string[]]@(Get-WinEvent -FilterHashtable @{ LogName = 'System'; Id = 41, 1001, 6008, 1074, 109, 6006 } -MaxEvents 30 -ErrorAction SilentlyContinue |
                ForEach-Object { "$($_.TimeCreated.ToString('o')) id=$($_.Id) $($_.ProviderName): $(($_.Message -replace '\s+', ' ').Substring(0, [Math]::Min(400, ($_.Message -replace '\s+', ' ').Length)))" })
            $dumps = [string[]]@(Get-ChildItem 'C:\Windows\Minidump', 'C:\Windows\MEMORY.DMP' -ErrorAction SilentlyContinue | ForEach-Object { "$($_.FullName) $($_.Length) $($_.LastWriteTime.ToString('o'))" })
            $wu = [string[]]@(Get-WinEvent -FilterHashtable @{ LogName = 'System'; ProviderName = 'Microsoft-Windows-WindowsUpdateClient' } -MaxEvents 5 -ErrorAction SilentlyContinue | ForEach-Object { "$($_.TimeCreated.ToString('o')) id=$($_.Id)" })
            # Event 41 carries the bugcheck code and its parameters (0 = none).
            $k41 = [string[]]@(Get-WinEvent -FilterHashtable @{ LogName = 'System'; Id = 41 } -MaxEvents 5 -ErrorAction SilentlyContinue |
                ForEach-Object { "$($_.TimeCreated.ToString('o')) " + (($_.Properties | ForEach-Object { '{0}' -f $_.Value }) -join ' ') })
            $crash = Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\CrashControl' -ErrorAction SilentlyContinue
            # Enabled for the next time: a small dump that survives the reboot.
            Set-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\CrashControl' -Name CrashDumpEnabled -Value 3 -ErrorAction SilentlyContinue
            # The service log around the stall: the newest file, last lines; and
            # the one before it (the boot just now starts a new one).
            $logs = @(Get-ChildItem "$installed\Logs" -Filter 'own-backup-service-*.log' -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending)
            $tail = @()
            foreach ($l in $logs | Select-Object -First 2) {
                $copy = Join-Path $env:TEMP "bootlog-$($l.Name)"
                Copy-Item -LiteralPath $l.FullName $copy -Force
                $tail += "== $($l.Name) $($l.Length)"
                $tail += [string[]]@(Get-Content -LiteralPath $copy -Tail 150 | ForEach-Object { ("$_" -replace '\x1b\[[0-9;]*m', '') })
            }
            # Around the first unreadable image, and what the folders hold.
            $first = @()
            foreach ($l in $logs | Select-Object -First 1) {
                $copy = Join-Path $env:TEMP "bootlog-full-$($l.Name)"
                Copy-Item -LiteralPath $l.FullName $copy -Force
                $all = [string[]]@(Get-Content -LiteralPath $copy)
                $hit = -1
                for ($i = 0; $i -lt $all.Count; $i++) { if ($all[$i] -like '*Skipping unreadable image*') { $hit = $i; break } }
                if ($hit -ge 0) { $first = [string[]]@($all[[Math]::Max(0, $hit - 60)..[Math]::Min($all.Count - 1, $hit + 10)] | ForEach-Object { ("$_" -replace '\x1b\[[0-9;]*m', '') }) }
            }
            $listing = [string[]]@(Get-ChildItem 'S:\killx-deletejob-d3' -Recurse -File -ErrorAction SilentlyContinue | Sort-Object FullName | ForEach-Object { "$($_.FullName) $($_.Length) $($_.LastWriteTime.ToString('HH:mm:ss'))" } | Select-Object -First 80)
            # Why the service did not come back: the service manager and crashes.
            $scm = [string[]]@(Get-WinEvent -FilterHashtable @{ LogName = 'System'; ProviderName = 'Service Control Manager'; Id = 7000, 7009, 7011, 7023, 7024, 7031, 7034 } -MaxEvents 15 -ErrorAction SilentlyContinue |
                ForEach-Object { "$($_.TimeCreated.ToString('o')) id=$($_.Id) $(($_.Message -replace '\s+', ' '))" })
            $crash = [string[]]@(Get-WinEvent -FilterHashtable @{ LogName = 'Application'; Id = 1000, 1001, 1026 } -MaxEvents 10 -ErrorAction SilentlyContinue |
                ForEach-Object { "$($_.TimeCreated.ToString('o')) id=$($_.Id) $(($_.Message -replace '\s+', ' ').Substring(0, [Math]::Min(700, ($_.Message -replace '\s+', ' ').Length)))" })
            Report 'bootlog' @{ scm = $scm; app_crash = $crash; first_unreadable = $first; listing = $listing; service_log = [string[]]$tail; events = $events; k41 = $k41; crash_dump_enabled = $crash.CrashDumpEnabled; dumps = $dumps; windows_update = $wu; boot = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToString('o') }
            Say "KILLX-PASS format=$format"
            shutdown.exe /s /t 5 | Out-Null
        }
        return
    }

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
            Say "KILLX-INSTALLED-TEXTS $((Get-ChildItem "$installed\*.txt" | ForEach-Object { "$($_.Name)=$($_.Length)" }) -join ' ')"
        }
        # A data disk the host replaced with a blank one (it outgrew the run's
        # space bound) is prepared again: the source and the store.
        if (-not (Get-Volume -FileSystemLabel OBKSRC -ErrorAction SilentlyContinue)) {
            $raw = @(Get-Disk | Where-Object { $_.PartitionStyle -eq 'RAW' -and $_.Size -gt 8GB })
            Check ($raw.Count -eq 1) 'a blank data disk to prepare again'
            Initialize-Disk -Number $raw[0].Number -PartitionStyle GPT
            $src = New-Partition -DiskNumber $raw[0].Number -Size 2GB -DriveLetter P
            Format-Volume -Partition $src -FileSystem NTFS -NewFileSystemLabel OBKSRC -Confirm:$false | Out-Null
            $dst = New-Partition -DiskNumber $raw[0].Number -UseMaximumSize -DriveLetter S
            Format-Volume -Partition $dst -FileSystem NTFS -NewFileSystemLabel OBKSTORE -Confirm:$false | Out-Null
            MakeData
            Say 'KILLX-REPREPARED the data disk'
        }
        if ($mode -eq 'flush' -and -not (Get-Volume -FileSystemLabel OBKFAULT -ErrorAction SilentlyContinue)) {
            $raw = @(Get-Disk | Where-Object { $_.PartitionStyle -eq 'RAW' -and $_.Size -lt 8GB })
            Check ($raw.Count -eq 1) 'one blank fault disk attached'
            Initialize-Disk -Number $raw[0].Number -PartitionStyle GPT
            $part = New-Partition -DiskNumber $raw[0].Number -UseMaximumSize -DriveLetter T
            Format-Volume -Partition $part -FileSystem NTFS -NewFileSystemLabel OBKFAULT -Confirm:$false | Out-Null
            Say 'KILLX-FAULT-DISK T:'
        }
        # Mode 'media': the medium carries this machine's WebView2, and the VM
        # has no network; the runtime installer comes on the answer CD.
        if ($format -like 'media*' -and -not (Get-ChildItem 'C:\Program Files (x86)\Microsoft\EdgeWebView\Application\*\msedgewebview2.exe' -ErrorAction SilentlyContinue)) {
            $setup = Start-Process "$lab\WebView2Runtime-x64.exe" -ArgumentList '/silent', '/install' -Wait -PassThru
            Check ($setup.ExitCode -eq 0) "WebView2 runtime installed (exit $($setup.ExitCode))"
        }
        if (Test-Path "$work\kills.done") {
            # A reboot after the kills: the user half finishes by itself.
            $armed = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon').AutoAdminLogon -eq '1'
            if (-not $armed) { AutoLogon; Say 'KILLX-REARM'; shutdown.exe /r /t 5 | Out-Null }
            return
        }
        # Nor is a full C: (mode 'nospace' killed between fill and unfill).
        Remove-Item 'C:\killx-fill.bin' -Force -ErrorAction SilentlyContinue
        # A request left by an attempt that did not finish is not this one's;
        # one written since this boot is (the user half can be first).
        $bootedAt = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime()
        if ((Test-Path "$work\kill.request") -and (Get-Item "$work\kill.request").LastWriteTimeUtc -lt $bootedAt) {
            Remove-Item "$work\kill.request" -ErrorAction SilentlyContinue
        }
        # AutoLogonCount is one logon: a retried attempt arms it again, which
        # only counts from the next boot.
        $armed = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon').AutoAdminLogon -eq '1'
        if (-not $armed) {
            # Winlogon also clears it once the logon has happened: rearm only
            # when nobody logs on at all.
            for ($i = 0; $i -lt 120 -and -not (Get-Process explorer -ErrorAction SilentlyContinue); $i++) { Start-Sleep -Seconds 1 }
            if (-not (Get-Process explorer -ErrorAction SilentlyContinue)) { AutoLogon; Say 'KILLX-REARM'; shutdown.exe /r /t 5 | Out-Null; return }
        }
        # Left by an attempt that failed while the history was blocked.
        foreach ($o in $obstacles) { if (Test-Path $o) { Remove-Item $o -Recurse -Force } }
        # Windows skipped the Startup folder after a hard power-off of this VM
        # (2026-10-17): the desktop came up and the user half never ran. If it
        # has not started 90 s after the desktop, start it in that session.
        $booted = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime()
        function UserStarted { (Test-Path "$work\user.started") -and (Get-Item "$work\user.started").LastWriteTimeUtc -gt $booted }
        $wait = [DateTime]::UtcNow.AddMinutes(5)
        while (-not (Get-Process explorer -ErrorAction SilentlyContinue) -and [DateTime]::UtcNow -lt $wait) { Start-Sleep -Seconds 2 }
        for ($i = 0; $i -lt 90 -and -not (UserStarted); $i++) { Start-Sleep -Seconds 1 }
        if (-not (UserStarted) -and (Get-Process explorer -ErrorAction SilentlyContinue)) {
            $action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File $lab\killx-guest.ps1 -User"
            $principal = New-ScheduledTaskPrincipal -UserId "$env:COMPUTERNAME\lab" -LogonType Interactive -RunLevel Limited
            Register-ScheduledTask -TaskName 'OwnBackupKillxUser' -Action $action -Principal $principal -Force | Out-Null
            Start-ScheduledTask -TaskName 'OwnBackupKillxUser'
            Say 'KILLX-NOTE the Startup item did not run; the user half was started by a task'
        }
        # Requests one after another until the user half says there are no
        # more. A request is JSON: the name of the record to write, and
        # optionally a file pattern in the store to wait for and a delay after
        # it appears - the consolidation says nothing over the event stream,
        # so its own marker file is the signal.
        $deadline = [DateTime]::UtcNow.AddMinutes(240)
        while (-not (Test-Path "$work\kills.done")) {
            Step 'waiting for a kill request'
            while (-not (Test-Path "$work\kill.request") -and -not (Test-Path "$work\kills.done")) {
                if ([DateTime]::UtcNow -gt $deadline) { throw 'the user half never finished asking' }
                Start-Sleep -Milliseconds 100
            }
            if (Test-Path "$work\kills.done") { break }
            Start-Sleep -Milliseconds 50
            $request = Get-Content "$work\kill.request" -Raw | ConvertFrom-Json
            if ($request.action -eq 'sxs') {
                # The medium's runtime started from the read-only CD and from a
                # copy on C:, under sxstrace; with the file lists of both.
                $name = $request.name
                $out = [ordered]@{}
                try {
                    $img = Mount-DiskImage -ImagePath $request.source -PassThru
                    $letter = ($img | Get-Volume).DriveLetter
                    $cd = "${letter}:\OwnBackup\WebView2"
                    $copy = 'C:\Lab\wv-copy'
                    if (Test-Path $copy) { Remove-Item $copy -Recurse -Force }
                    Copy-Item $cd $copy -Recurse
                    $installedRt = @(Get-ChildItem 'C:\Program Files (x86)\Microsoft\EdgeWebView\Application\*\msedgewebview2.exe')[0].DirectoryName
                    $rel = { param($root) @(Get-ChildItem $root -Recurse -File -Force | ForEach-Object { $_.FullName.Substring($root.Length + 1) + ' ' + $_.Length }) }
                    $cdFiles = & $rel $cd; $srcFiles = & $rel $installedRt
                    $out.cd_count = $cdFiles.Count; $out.src_count = $srcFiles.Count
                    $out.only_in_source = @($srcFiles | Where-Object { $cdFiles -notcontains $_ } | Select-Object -First 60)
                    $out.only_on_cd = @($cdFiles | Where-Object { $srcFiles -notcontains $_ } | Select-Object -First 60)
                    $out.manifests = @(Get-ChildItem $cd -Recurse -Filter *.manifest | ForEach-Object { $_.FullName })
                    $out.cd_attributes = "$((Get-Item "$cd\msedgewebview2.exe").Attributes)"
                    $etl = "$work\sxs.etl"
                    Start-Process sxstrace.exe -ArgumentList 'Trace', "-logfile:$etl", '-nostop' -WindowStyle Hidden
                    Start-Sleep -Seconds 3
                    $try = {
                        param($exe)
                        try {
                            $p = Start-Process $exe -ArgumentList '--headless', '--no-sandbox', '--disable-gpu', 'about:blank' -PassThru -ErrorAction Stop
                            Start-Sleep -Seconds 6
                            if ($p.HasExited) { "exited $($p.ExitCode)" } else { Stop-Process -Id $p.Id -Force; Get-Process msedgewebview2 -ErrorAction SilentlyContinue | Stop-Process -Force; 'running' }
                        } catch { "failed: $($_.Exception.Message)" }
                    }
                    $out.from_cd = & $try "$cd\msedgewebview2.exe"
                    $out.from_copy = & $try "$copy\msedgewebview2.exe"
                    $out.from_installed = & $try "$installedRt\msedgewebview2.exe"
                    & sxstrace.exe StopTrace | Out-Null
                    Start-Sleep -Seconds 2
                    & sxstrace.exe Parse "-logfile:$etl" "-outfile:$work\sxs.txt" | Out-Null
                    # ASCII only in this file: PowerShell 5.1 reads it as cp1251,
                    # where a UTF-8 Cyrillic byte can be a quote mark.
                    if (Test-Path "$work\sxs.txt") { $out.sxstrace = [string[]]@(Get-Content "$work\sxs.txt" | Where-Object { $_.Trim() } | Select-Object -First 200 | ForEach-Object { "$_" }) }
                    $out.events = [string[]]@(Get-WinEvent -FilterHashtable @{ LogName = 'Application'; ProviderName = 'SideBySide' } -MaxEvents 5 -ErrorAction SilentlyContinue | ForEach-Object { "$($_.TimeCreated.ToString('o')) $($_.Message)" })
                } catch { $out.error = "$($_.Exception.Message)" }
                finally { try { Dismount-DiskImage -ImagePath $request.source | Out-Null } catch { } }
                Remove-Item "$work\kill.request" -Force
                Report $name $out
                Step "sxs done ($name)"
                continue
            }
            if ($request.action -in @('qvol', 'qvol-detach')) {
                # Mode 'deletejob': Q: is a small NTFS volume in a VHDX on C:.
                $name = $request.name
                $vhd = 'C:\Lab\q-src.vhdx'
                $dp = "$work\q-diskpart.txt"
                if (Test-Path $vhd) {
                    "select vdisk file=$vhd`r`ndetach vdisk" | Set-Content $dp -Encoding ASCII
                    & diskpart.exe /s $dp | Out-Null
                    Remove-Item $vhd -Force
                }
                if ($request.action -eq 'qvol') {
                    "create vdisk file=$vhd maximum=128 type=expandable`r`nselect vdisk file=$vhd`r`nattach vdisk`r`ncreate partition primary`r`nformat fs=ntfs quick label=QSRC`r`nassign letter=Q" | Set-Content $dp -Encoding ASCII
                    & diskpart.exe /s $dp | Out-Null
                    $rng = New-Object Random 7
                    for ($i = 1; $i -le 5; $i++) { $bytes = New-Object byte[] 1MB; $rng.NextBytes($bytes); [IO.File]::WriteAllBytes("Q:\q$i.bin", $bytes) }
                }
                Remove-Item "$work\kill.request" -Force
                JsonFile "$work\$name.json" @{ action = $request.action; q = [bool](Get-Volume -DriveLetter Q -ErrorAction SilentlyContinue) }
                Step "$($request.action) done ($name)"
                continue
            }
            if ($request.action -in @('plant', 'svc-stop', 'svc-start')) {
                # Mode 'settings6a': a config.toml of the user's, written
                # while the service is down (as an older window left it).
                $name = $request.name
                $cfg = 'C:\ProgramData\OwnBackup\config.toml'
                $out = [ordered]@{ action = $request.action }
                if ($request.action -in @('plant', 'svc-stop')) {
                    Stop-Service OwnBackup -Force
                    for ($i = 0; $i -lt 60 -and (Get-Service OwnBackup).Status -ne 'Stopped'; $i++) { Start-Sleep -Seconds 1 }
                    Get-Process own-backup-service -ErrorAction SilentlyContinue | Wait-Process -Timeout 30 -ErrorAction SilentlyContinue
                }
                if ($request.action -eq 'plant') {
                    $planted = 'C:\Windows\Temp\obk-planted'
                    New-Item -ItemType Directory -Force $planted | Out-Null
                    Get-ChildItem $planted -Force | Remove-Item -Recurse -Force
                    Remove-Item $cfg, "$cfg.lock" -Force -ErrorAction SilentlyContinue
                    $text = "log_level = `"debug`"`r`nlog_mirror_dir = '$planted'`r`n"
                    [IO.File]::WriteAllText($cfg, $text, [Text.UTF8Encoding]::new($false))
                    # Not $user: PowerShell names are caseless, and -User is this script's switch.
                    $console = (Get-CimInstance Win32_ComputerSystem).UserName
                    & icacls.exe $cfg /setowner $console | Out-Null
                    & icacls.exe $cfg /grant "${console}:F" | Out-Null
                    $out.owner_before = (Get-Acl $cfg).GetOwner([Security.Principal.SecurityIdentifier]).Value
                }
                $out.alive_after_stop = @(Get-Process own-backup-service -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })
                $out.status_after_stop = "$((Get-Service OwnBackup).Status)"
                $out.exit_code = (Get-CimInstance Win32_Service -Filter "Name='OwnBackup'").ExitCode
                if ($request.action -in @('plant', 'svc-start')) {
                    try {
                        Start-Service OwnBackup
                        WaitService
                    } catch {
                        $out.start_error = "$($_.Exception.Message)"
                        $since = (Get-Date).AddMinutes(-15)
                        $out.events = [string[]]@(Get-WinEvent -FilterHashtable @{ LogName = 'System'; ProviderName = 'Service Control Manager'; StartTime = $since } -ErrorAction SilentlyContinue | Select-Object -First 10 | ForEach-Object { "$($_.Id) $($_.Message)" })
                        $out.crashes = [string[]]@(Get-WinEvent -FilterHashtable @{ LogName = 'Application'; Id = 1000, 1001, 1026; StartTime = $since } -ErrorAction SilentlyContinue | Select-Object -First 5 | ForEach-Object { "$($_.Id) $($_.Message)" })
                        $out.alive = @(Get-Process own-backup-service -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })
                        Report "$name-diag" $out
                        throw
                    }
                }
                if ($request.action -eq 'plant') {
                    $out.owner_after = (Get-Acl $cfg).GetOwner([Security.Principal.SecurityIdentifier]).Value
                    $out.text = [IO.File]::ReadAllText($cfg)
                }
                Remove-Item "$work\kill.request" -Force
                JsonFile "$work\$name.json" $out
                Step "$($request.action) done ($name)"
                continue
            }
            if ($request.action -in @('fvol', 'fvol-detach')) {
                # Mode 'fatx': F: is FAT32 in a VHDX on C:, with *.tmp files.
                $name = $request.name
                $vhd = 'C:\Lab\f-src.vhdx'
                $dp = "$work\f-diskpart.txt"
                $out = [ordered]@{ action = $request.action }
                if (Test-Path $vhd) {
                    "select vdisk file=$vhd`r`ndetach vdisk" | Set-Content $dp -Encoding ASCII
                    & diskpart.exe /s $dp | Out-Null
                    Remove-Item $vhd -Force
                }
                if ($request.action -eq 'fvol') {
                    "create vdisk file=$vhd maximum=600 type=expandable`r`nselect vdisk file=$vhd`r`nattach vdisk`r`ncreate partition primary`r`nformat fs=fat32 quick label=FATSRC`r`nassign letter=F" | Set-Content $dp -Encoding ASCII
                    & diskpart.exe /s $dp | Out-Null
                    [IO.File]::WriteAllText('F:\fat-marker.txt', 'Own Backup FAT32 exclusions acceptance source')
                    New-Item -ItemType Directory -Force 'F:\keep', 'F:\cache' | Out-Null
                    $rng = New-Object Random 20261020
                    $kept = [ordered]@{}
                    for ($i = 1; $i -le 12; $i++) {
                        $bytes = New-Object byte[] ($rng.Next(16KB, 3MB)); $rng.NextBytes($bytes)
                        $rel = 'keep\k{0:D2}.bin' -f $i
                        [IO.File]::WriteAllBytes("F:\$rel", $bytes)
                        $kept[$rel] = (Get-FileHash -LiteralPath "F:\$rel").Hash
                    }
                    for ($i = 1; $i -le 10; $i++) {
                        $rel = if ($i -le 7) { 'cache\c{0:D2}.tmp' -f $i } else { 'x{0:D2}.tmp' -f $i }
                        $bytes = New-Object byte[] ($rng.Next(1KB, 4MB)); $rng.NextBytes($bytes)
                        [IO.File]::WriteAllBytes("F:\$rel", $bytes)
                    }
                    $vol = Get-Volume -DriveLetter F
                    $out.filesystem = "$($vol.FileSystem)"; $out.cluster = $vol.AllocationUnitSize; $out.free = $vol.SizeRemaining
                    $out.tmp = @(Get-ChildItem 'F:\' -Recurse -File -Filter '*.tmp' | ForEach-Object { $_.Length })
                    $out.kept = $kept
                }
                Remove-Item "$work\kill.request" -Force
                JsonFile "$work\$name.json" $out
                Step "$($request.action) done ($name)"
                continue
            }
            if ($request.action -in @('refs-prepare', 'refs-unlock', 'refs-detach')) {
                # Mode 'refsbl': R: is ReFS in a VHDX on C:, under BitLocker;
                # mode 'ntfsbl': the same with NTFS (own FVE read, MFT scan).
                $innerFs = if ($format -eq 'ntfsbl') { 'ntfs' } else { 'refs' }
                $name = $request.name
                $vhd = 'C:\Lab\refs-src.vhdx'
                $dp = "$work\refs-diskpart.txt"
                $out = [ordered]@{ action = $request.action }
                $secure = ConvertTo-SecureString 'KillX-Refs-2026!' -AsPlainText -Force
                if ($request.action -eq 'refs-prepare') {
                    if (Test-Path $vhd) {
                        "select vdisk file=$vhd`r`ndetach vdisk" | Set-Content $dp -Encoding ASCII
                        & diskpart.exe /s $dp | Out-Null
                        Remove-Item $vhd -Force
                    }
                    "create vdisk file=$vhd maximum=1536 type=expandable`r`nselect vdisk file=$vhd`r`nattach vdisk`r`ncreate partition primary`r`nformat fs=$innerFs quick label=REFSSRC`r`nassign letter=R" | Set-Content $dp -Encoding ASCII
                    & diskpart.exe /s $dp | Out-Null
                    [IO.File]::WriteAllText('R:\refs-marker.txt', 'Own Backup ReFS-in-BitLocker acceptance source')
                    New-Item -ItemType Directory -Force 'R:\keep', 'R:\cache', 'R:\docs' | Out-Null
                    $rng = New-Object Random 20261018
                    $kept = [ordered]@{}
                    for ($i = 1; $i -le 20; $i++) {
                        $bytes = New-Object byte[] ($rng.Next(64KB, 2MB)); $rng.NextBytes($bytes)
                        $rel = 'keep\k{0:D2}.bin' -f $i
                        [IO.File]::WriteAllBytes("R:\$rel", $bytes)
                        $kept[$rel] = (Get-FileHash -LiteralPath "R:\$rel").Hash
                    }
                    [IO.File]::WriteAllText('R:\docs\notes.txt', ('kept text ' * 5000))
                    $kept['docs\notes.txt'] = (Get-FileHash -LiteralPath 'R:\docs\notes.txt').Hash
                    $secret = 'KILLX-REFS-SECRET-'
                    for ($i = 1; $i -le 12; $i++) {
                        $rel = if ($i -le 8) { 'cache\c{0:D2}.tmp' -f $i } else { 'x{0:D2}.tmp' -f $i }
                        $text = (($secret + $i + '-') * 16000)
                        [IO.File]::WriteAllText("R:\$rel", $text)
                    }
                    Enable-BitLocker -MountPoint 'R:' -PasswordProtector -Password $secure -EncryptionMethod XtsAes128 -UsedSpaceOnly -SkipHardwareTest -ErrorAction Stop | Out-Null
                    for ($t = 0; $t -lt 300 -and (Get-BitLockerVolume -MountPoint 'R:').VolumeStatus -ne 'FullyEncrypted'; $t++) { Start-Sleep -Seconds 2 }
                    $out.filesystem = (Get-Volume -DriveLetter R).FileSystem
                    $bl = Get-BitLockerVolume -MountPoint 'R:'
                    $out.method = "$($bl.EncryptionMethod)"; $out.status = "$($bl.VolumeStatus)"
                    Lock-BitLocker -MountPoint 'R:' -ForceDismount -ErrorAction Stop | Out-Null
                    $out.lock = "$((Get-BitLockerVolume -MountPoint 'R:').LockStatus)"
                    $out.kept = $kept
                } elseif ($request.action -eq 'refs-unlock') {
                    Unlock-BitLocker -MountPoint 'R:' -Password $secure -ErrorAction Stop | Out-Null
                    $out.lock = "$((Get-BitLockerVolume -MountPoint 'R:').LockStatus)"
                } else {
                    "select vdisk file=$vhd`r`ndetach vdisk" | Set-Content $dp -Encoding ASCII
                    & diskpart.exe /s $dp | Out-Null
                    Remove-Item $vhd -Force -ErrorAction SilentlyContinue
                }
                Remove-Item "$work\kill.request" -Force
                JsonFile "$work\$name.json" $out
                Step "$($request.action) done ($name)"
                continue
            }
            if ($request.action -in @('mountpoint', 'unmountpoint')) {
                # P: also reached through an empty folder on C: (mode 'aliases').
                $name = $request.name
                $mnt = 'C:\killx-mnt\'
                $part = Get-Partition -DriveLetter P
                if ($request.action -eq 'mountpoint') {
                    # A run that failed before 'unmountpoint' left the folder
                    # pointing at a data disk since replaced. mountvol /D and
                    # rmdir drop the link only, never what it led to.
                    & mountvol.exe $mnt /D 2>$null | Out-Null
                    if (Test-Path $mnt) { & cmd.exe /c rmdir $mnt.TrimEnd([char]92) | Out-Null }
                    New-Item -ItemType Directory -Force $mnt | Out-Null
                    if (@($part.AccessPaths) -notcontains $mnt) { Add-PartitionAccessPath -InputObject $part -AccessPath $mnt }
                } else {
                    Remove-PartitionAccessPath -InputObject $part -AccessPath $mnt -ErrorAction SilentlyContinue
                }
                Remove-Item "$work\kill.request" -Force
                $paths = @((Get-Partition -DriveLetter P).AccessPaths)
                Say "KILLX-NOTE $($request.action): P: is at $($paths -join ', ')"
                JsonFile "$work\$name.json" @{ action = $request.action; paths = $paths }
                Step "$($request.action) done ($name)"
                continue
            }
            if ($request.action -eq 'purge') {
                # The stand's jobs, all of formats already passed: one of them
                # outgrew what a reply may carry (gRPC 4 MB), and every listing
                # failed. Emptied with the service stopped.
                $name = $request.name
                Stop-Service OwnBackup -Force
                [IO.File]::WriteAllText('C:\ProgramData\OwnBackup\jobs.json', '[]', [Text.UTF8Encoding]::new($false))
                Start-Service OwnBackup
                WaitService
                Remove-Item "$work\kill.request" -Force
                JsonFile "$work\$name.json" @{ action = 'purge' }
                Step "purge done ($name)"
                continue
            }
            if ($request.action -in @('measure', 'fill', 'unfill')) {
                $name = $request.name
                $m = [ordered]@{ action = $request.action }
                $fill = 'C:\killx-fill.bin'
                if ($request.action -eq 'fill') {
                    # fsutil sets the length without writing: nothing reaches the qcow2.
                    Remove-Item $fill -Force -ErrorAction SilentlyContinue
                    $free = (Get-Volume -DriveLetter C).SizeRemaining
                    $m.fsutil = "$(& fsutil.exe file createnew $fill ($free - 256KB) 2>&1)"
                    $m.free = (Get-Volume -DriveLetter C).SizeRemaining
                } elseif ($request.action -eq 'unfill') {
                    Remove-Item $fill -Force
                    $m.free = (Get-Volume -DriveLetter C).SizeRemaining
                } else {
                    $p = Get-Process own-backup-service
                    $m.handles = $p.HandleCount; $m.threads = $p.Threads.Count; $m.working_set = $p.WorkingSet64
                    $m.iscsi_sessions = @(Get-IscsiSession -ErrorAction SilentlyContinue).Count
                    $m.disks = @(Get-Disk).Count
                    $m.jobs_sha = (Get-FileHash 'C:\ProgramData\OwnBackup\jobs.json').Hash
                    $m.jobs_tmp = Test-Path 'C:\ProgramData\OwnBackup\jobs.json.tmp'
                    $m.jobs_bytes = (Get-Item 'C:\ProgramData\OwnBackup\jobs.json').Length
                    $m.c_free = (Get-Volume -DriveLetter C).SizeRemaining
                }
                Remove-Item "$work\kill.request" -Force
                JsonFile "$work\$name.json" $m
                Step "$($request.action) done ($name)"
                continue
            }
            if ($request.action -eq 'rawcopy') {
                # The built ISO, byte for byte from sector 0 of the blank 2 GiB
                # disk, where the host reads it without mounting anything.
                $name = $request.name
                $raw = @(Get-Disk | Where-Object { $_.PartitionStyle -eq 'RAW' -and $_.Size -eq 2GB })
                Check ($raw.Count -eq 1) 'one blank 2 GiB media disk attached'
                $src = [IO.File]::OpenRead($request.source)
                $dst = New-Object IO.FileStream "\\.\PhysicalDrive$($raw[0].Number)", 'Open', 'ReadWrite'
                try { $src.CopyTo($dst, 1MB); $dst.Flush() } finally { $dst.Dispose(); $src.Dispose() }
                Remove-Item "$work\kill.request" -Force
                JsonFile "$work\$name.json" @{ action = 'rawcopy'; disk = $raw[0].Number; length = (Get-Item $request.source).Length }
                Step "rawcopy done ($name)"
                continue
            }
            if ($request.action -in @('restart', 'block', 'unblock')) {
                $name = $request.name
                if ($request.action -eq 'restart') { Restart-Service OwnBackup -Force; WaitService }
                elseif ($request.action -eq 'block') { foreach ($o in $obstacles) { New-Item -ItemType Directory -Force $o | Out-Null } }
                else { foreach ($o in $obstacles) { Remove-Item $o -Recurse -Force } }
                Remove-Item "$work\kill.request" -Force
                JsonFile "$work\$name.json" @{ action = $request.action; utc = [DateTime]::UtcNow.ToString('o') }
                Step "$($request.action) done ($name)"
                continue
            }
            # Only a file written after the request counts: a marker left by
            # the previous kill is the resume's, not this run's fold.
            $asked = (Get-Item "$work\kill.request").LastWriteTimeUtc
            $name = if ($request.name) { $request.name } else { 'killed' }
            $seen = $null
            if ($request.pattern) {
                $until = [DateTime]::UtcNow.AddSeconds([int]$request.timeout_s)
                while (-not $seen -and [DateTime]::UtcNow -lt $until) {
                    $seen = Get-ChildItem -LiteralPath $store -Force -Filter $request.pattern -ErrorAction SilentlyContinue |
                        Where-Object { $_.LastWriteTimeUtc -gt $asked -or $_.CreationTimeUtc -gt $asked } | Select-Object -First 1
                    if (-not $seen) { Start-Sleep -Milliseconds 20 }
                }
                if ($seen) { Start-Sleep -Milliseconds ([int]$request.delay_ms) }
            }
            $service = Get-Process own-backup-service -ErrorAction Stop
            $at = [DateTime]::UtcNow.ToString('o')
            Stop-Process -Id $service.Id -Force
            # A process inside a driver call dies only when the call returns:
            # 13 s once (2026-10-18), while the service still showed Running.
            $exitWait = [Diagnostics.Stopwatch]::StartNew()
            Wait-Process -Id $service.Id -Timeout 180 -ErrorAction SilentlyContinue
            Say "KILLX-KILLED name=$name pid=$($service.Id) utc=$at exit_ms=$($exitWait.ElapsedMilliseconds) trigger=$(if ($seen) { $seen.Name } else { 'none' })"
            Step "killed ($name)"
            Start-Sleep -Seconds 2
            $right = [ordered]@{ utc = $at; pid = $service.Id; trigger = $(if ($seen) { $seen.Name } else { $null }); listing = (Listing) }
            # The service log names the step the work was in.
            $log = Get-ChildItem "$installed\Logs" -Filter 'own-backup-service-*.log' -ErrorAction SilentlyContinue |
                Sort-Object LastWriteTime | Select-Object -Last 1
            if ($log) {
                $copy = Join-Path $work "service-at-$name.log"
                Copy-Item -LiteralPath $log.FullName $copy -Force
                # Plain strings: Get-Content's carry PSDrive/PSProvider, which
                # ConvertTo-Json -Depth 20 walks for minutes (PowerShell 5.1).
                $right.log_tail = [string[]]@(Get-Content -LiteralPath $copy -Tail 25 | ForEach-Object { "$_" })
            }
            for ($i = 0; $i -lt 60 -and (Get-Service OwnBackup).Status -ne 'Running'; $i++) {
                try { Start-Service OwnBackup -ErrorAction Stop } catch { Start-Sleep -Seconds 1 }
            }
            WaitService
            $right.restarted = @(Get-Process own-backup-service | ForEach-Object { $_.Id })
            Remove-Item "$work\kill.request" -Force
            JsonFile "$work\$name.json" $right
            Step "$name.json written"
            Report $name $right
        }
        # Armed now for the next attempt's boot: a rearm needs a reboot of its
        # own, and an OVMF reset of this VM has hung once (2026-10-16).
        AutoLogon
        return
    }

    if ($User -and $phase -eq 'run') {
        [IO.File]::WriteAllText("$work\user.started", [DateTime]::UtcNow.ToString('o'))
        for ($i = 0; $i -lt 600 -and (Test-Path "$base\upgrading"); $i++) { Start-Sleep -Seconds 1 }
        # The installer's own start of the service is not the end of it.
        Start-Sleep -Seconds 5
        WaitService
        if ($mode -eq 'flush') { for ($i = 0; $i -lt 180 -and -not (Test-Path 'T:\'); $i++) { Start-Sleep -Seconds 1 } }
        New-Item -ItemType Directory -Force $store | Out-Null
        if ($format -like 'media*') { MediaBuild; return }
        if ($format -in @('checks9', 'nospace')) { Checks9; return }
        if ($format -eq 'aliases') { Aliases; return }
        if ($format -eq 'fatx') { FatX; return }
        if ($format -eq 'settings6a') { Settings6a; return }
        if ($format -in @('refsbl', 'ntfsbl')) { RefsBitLocker; return }
        if ($format -eq 'deletejob') { DeleteJob; return }
        $state = if (Test-Path "$work\job.json") { Get-Content "$work\job.json" -Raw | ConvertFrom-Json } else { $null }
        if (-not $state) {
            # Synthetic secret of a throwaway VM.
            [IO.File]::WriteAllText("$work\password.txt", 'KillX-Lab-2026-q9')
            $job = Rpc 'create' @{ op = 'create'; encrypted = $encrypted
                password_file = "$work\password.txt"; recovery_file = "$work\recovery.txt"; job = @{
                name = $jobName; source = 'P:'; sources = @('P:'); dest_folder = $store
                image_format = $(if ($container -eq 'vhdx') { 'vhdx' } else { 'qcow2' });
                schedule = @{ kind = 'none' };
                scheme = $(if ($mode -eq 'history') { @{ method = 'incremental'; only_incremental = $true; keep_chains = 5 } } elseif ($mode -eq 'flush') { @{ method = 'full'; keep_chains = 5 } } elseif ($mode -eq 'consol') {
                    @{ method = 'incremental'; only_incremental = $true; full_every_n = 100; keep_chains = 1; rolling_incrementals = 1 } } else {
                    @{ method = 'incremental'; only_incremental = $false; full_every_n = 1; keep_chains = 1 } })
                exclusions = @{ enabled = ($mode -eq 'exclusions'); patterns = @('*.tmp'); patterns_linux = @() };
                advanced = @{ validate_after = $false; retry_attempts = 0; after_backup = 'none' } } }
            JsonFile "$work\job.json" @{ id = $job.id }
            $state = Get-Content "$work\job.json" -Raw | ConvertFrom-Json
        }
        $id = $state.id
        if ($mode -eq 'consol') { Consolidation $id; return }
        if ($format -eq 'probe-io') { ProbeIo; return }
        if ($mode -eq 'history') { HistoryUnwritable $id; return }
        if ($format -like 'stall-*') { StallCancel $id; return }
        if ($mode -eq 'flush') { FlushFault $id; return }

        # Run 1, killed inside the deletion of the excluded files.
        if (-not (Test-Path "$work\killed.json")) {
            # What an earlier unfinished attempt left; this one starts clean.
            $stale = @(Names)
            if ($stale.Count) { Report 'stale-before-run1' @{ files = $stale }; Get-ChildItem -LiteralPath $store -Force | Remove-Item -Force }
            $at = Rpc "run1-at-$killPhase" @{ op = 'start-run-at-phase'; id = $id; phase = $killPhase; after_ms = $(if ($killPhase -eq 'exclusions') { 1500 } else { 0 }); timeout_secs = 1200 }
            [IO.File]::WriteAllText("$work\kill.request", '{"name":"killed"}')
            for ($i = 0; $i -lt 600 -and -not (Test-Path "$work\killed.json"); $i++) { Start-Sleep -Milliseconds 500 }
            if (-not (Test-Path "$work\killed.json")) {
                Say "SYS-DIAG work: $((Get-ChildItem $work -Force | ForEach-Object { $_.Name }) -join ', ')"
                if (Test-Path "$work\system-steps.txt") { Get-Content "$work\system-steps.txt" | ForEach-Object { Say "SYS-DIAG system: $_" } }
            }
            Check (Test-Path "$work\killed.json") "the service was killed $($at.in_phase_ms) ms into the $killPhase phase"
            [IO.File]::WriteAllText("$work\kills.done", 'done')
        }
        $killed = Get-Content "$work\killed.json" -Raw | ConvertFrom-Json
        $atKill = @($killed.listing | ForEach-Object { $_.name })
        Check (@($atKill | Where-Object { $_ -notlike $private }).Count -eq 0) "nothing under a public name at the kill ($($atKill -join ', '))"
        Check (@($atKill | Where-Object { $_ -like "$private.tmp.$imageExt" }).Count -eq 1) 'the unfinished image is private'
        $leftover = @($atKill | Where-Object { $_ -like $private })
        WaitService
        $after = Rpc 'job-after-kill' @{ op = 'get'; id = $id }
        Check ($null -eq $after.last_run) 'the killed run left no success in the history'
        if (-not $encrypted) {
            # The service's ledger proves a plain image ours; it sweeps the
            # connected destinations as it starts, before any run.
            for ($i = 0; $i -lt 60 -and @(Pending).Count; $i++) { Start-Sleep -Milliseconds 500 }
            Report 'after-restart' @{ files = (Listing) }
            Check (@(Pending).Count -eq 0) "the restarted service reclaimed the killed run's image ($((@(Pending)) -join ', '))"
        }

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
        # An OBK stage, its owner record and the exclusion scratch named from
        # it are proved by the job's key; a plain image by the service ledger.
        Check (@($leftover | Where-Object { $leftNow -contains $_ }).Count -eq 0) "the killed run's proved leftovers were reclaimed ($($leftover -join ', '))"
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
    if ($User) { Remove-Item "$work\killed.json", "$work\job.json", "$work\kills.done", "$work\consol-*.json" -ErrorAction SilentlyContinue }
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
