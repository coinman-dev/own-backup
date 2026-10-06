# Boot 2 of scripts/lab/run-job-menu.py, as the active-console lab user: what
# the menu of a job asks of the SERVICE, through the command line. Two jobs on
# the laboratory's data volume -- one in qcow2, one in VHDX -- each with real
# chains; then the list of versions, both checks, "save as VHDX" and the
# cleanup of versions, every answer compared with the files on the disk.
#
# The acceptance of a saved VHDX is Windows itself: the file is attached by
# Mount-DiskImage, with no program of ours in between, and the control file
# inside must hash to what the source held when that version was made.
#
# ASCII only (PowerShell 5.1 reads a script without a byte-order mark in the
# ANSI code page). Every check is one MENU-OK line on the serial port; the
# first that fails ends the run with MENU-STAGE-FAIL.

function Say([string]$Text) {
    try { $p = New-Object IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'; $p.Open(); $p.WriteLine($Text); $p.Close() } catch { }
}
function Check([bool]$Ok, [string]$What) {
    if (-not $Ok) { throw "check failed: $What" }
    Say "MENU-OK $What"
}
# The command line: what it printed and how it ended. Its Russian text does
# not survive the pipe and is never looked at -- only exit codes, JSON and
# file names.
function Obk([string[]]$Arguments) {
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = (& 'C:\Lab\own-backup-cli.exe' @Arguments 2>&1 | Out-String).Trim()
        return [pscustomobject]@{ Code = [int]$LASTEXITCODE; Output = [string]$output }
    } finally { $ErrorActionPreference = $old }
}
function Must([string[]]$Arguments) {
    $r = Obk $Arguments
    if ($r.Code -ne 0) { throw "cli $($Arguments -join ' ') exit=$($r.Code) output=$($r.Output)" }
    return $r.Output
}
function NewJob([string]$Name, [string]$Format, [string]$Folder) {
    New-Item -ItemType Directory -Force -Path $Folder | Out-Null
    $created = Must @('jobs', 'new', '--name', $Name, '--source', $script:source, '--dest', $Folder,
        '--format', $Format, '--method', 'incremental', '--full-every-n', '2', '--keep-chains', '5',
        '--schedule', 'none', '--no-exclusions', '--validate-after=false')
    $id = [regex]::Match($created, '[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}').Value
    if (-not $id) { throw "new job id missing: $created" }
    return $id
}
# Run a job and wait for the run to end; returns its record.
function RunJob([string]$Id) {
    $before = ((Must @('jobs', 'show', $Id)) | ConvertFrom-Json).last_run
    $previous = if ($before) { $before.id } else { '' }
    $null = Must @('jobs', 'run', $Id)
    $deadline = [DateTime]::UtcNow.AddMinutes(10)
    do {
        Start-Sleep -Seconds 1
        $job = (Must @('jobs', 'show', $Id)) | ConvertFrom-Json
        if ($job.last_run -and $job.last_run.id -ne $previous) {
            if ($job.last_run.status -ne 'success') { throw "run failed: $($job.last_run.error)" }
            Say "MENU-RUN job=$($job.name) kind=$($job.last_run.backup_kind) image=$([IO.Path]::GetFileName($job.last_run.image_path))"
            return $job.last_run
        }
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'run timeout'
}
# Rewrite the first megabytes of the control file and say what it hashes to now.
function Mutate([int]$Seed) {
    $buffer = New-Object byte[] (1MB)
    ([Random]::new($Seed)).NextBytes($buffer)
    $stream = [IO.File]::Open($script:control, [IO.FileMode]::Open, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    try {
        for ($i = 0; $i -lt 24; $i++) { $stream.Write($buffer, 0, $buffer.Length) }
        $stream.Flush($true)
    } finally { $stream.Dispose() }
    return (Get-FileHash -LiteralPath $script:control -Algorithm SHA256).Hash
}
function Versions([string]$Id) { (Must @('jobs', 'versions', $Id, '--json')) | ConvertFrom-Json }
function AllImages($Listing) { @($Listing.chains | ForEach-Object { $_.images }) }
# Attach a VHDX with Windows alone, hash the control file inside, detach.
function HashInsideVhdx([string]$Path) {
    $null = Mount-DiskImage -ImagePath $Path -Access ReadOnly -PassThru
    try {
        $number = (Get-DiskImage -ImagePath $Path | Get-Disk).Number
        $deadline = [DateTime]::UtcNow.AddSeconds(60)
        do {
            foreach ($partition in @(Get-Partition -DiskNumber $number -ErrorAction SilentlyContinue)) {
                foreach ($root in @($partition.AccessPaths | Where-Object { $_ -like '\\?\Volume*' })) {
                    $file = $root + 'menu\data.bin'
                    if ([IO.File]::Exists($file)) {
                        $stream = [IO.File]::OpenRead($file)
                        try {
                            $sha = [Security.Cryptography.SHA256]::Create()
                            return [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '')
                        } finally { $stream.Dispose() }
                    }
                }
            }
            Start-Sleep -Seconds 1
        } while ([DateTime]::UtcNow -lt $deadline)
        throw "no volume with the control file in $Path"
    } finally { $null = Dismount-DiskImage -ImagePath $Path }
}

$ErrorActionPreference = 'Stop'
$ready = 'C:\Lab\job-menu-stage.ready'
if (Test-Path -LiteralPath $ready) { Say 'MENU-STAGE-SECOND-BOOT'; return }
try {
    Say "MENU-STAGE-BEGIN $([DateTime]::UtcNow.ToString('o'))"
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    Say "MENU-STAGE-CLIENT identity=$($identity.Name) session=$((Get-Process -Id $PID).SessionId)"
    for ($n = 0; $n -lt 90 -and (Obk @('jobs', 'status')).Code -ne 0; $n++) { Start-Sleep -Seconds 1 }
    $status = Obk @('jobs', 'status')
    if ($status.Code -ne 0) {
        # Why not, in the service's own words: sent as base64 because they are Russian.
        Say "MENU-STATUS service=$((Get-Service OwnBackup -ErrorAction SilentlyContinue).Status) exit=$($status.Code)"
        Say "MENU-TEXT status $([Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($status.Output)))"
    }
    Check ($status.Code -eq 0) 'the command line reaches the service'

    # Nothing of an earlier laboratory run in the list.
    foreach ($old in [regex]::Matches((Must @('jobs', 'list')), '(?m)^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}')) {
        $null = Obk @('jobs', 'delete', $old.Value)
    }

    $devices = Get-Content 'C:\Lab\audit-2026-09-27\devices.json' -Raw | ConvertFrom-Json
    $script:source = "$($devices.source_letter):"
    # A small source of our own: the fixture the image comes with is a gigabyte.
    Remove-Item -LiteralPath "$script:source\a07-data" -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path "$script:source\menu" | Out-Null
    $script:control = "$script:source\menu\data.bin"
    $buffer = New-Object byte[] (1MB)
    ([Random]::new(20261006)).NextBytes($buffer)
    $stream = [IO.File]::Create($script:control)
    try { for ($i = 0; $i -lt 160; $i++) { $stream.Write($buffer, 0, $buffer.Length) }; $stream.Flush($true) } finally { $stream.Dispose() }
    $hash = @{ 1 = (Get-FileHash -LiteralPath $script:control -Algorithm SHA256).Hash }
    Say "MENU-SOURCE volume=$script:source file_mib=160"

    # ---- qcow2: two chains, [full inc inc] and [full inc] -------------------
    $folder = 'C:\Lab\menu\qcow'
    Remove-Item -LiteralPath 'C:\Lab\menu' -Recurse -Force -ErrorAction SilentlyContinue
    $job = NewJob 'Alpha qcow' 'qcow2' $folder
    $kinds = @()
    for ($n = 1; $n -le 5; $n++) {
        if ($n -gt 1) { $hash[$n] = Mutate (20261006 + $n) }
        $kinds += (RunJob $job).backup_kind
    }
    Check (($kinds -join ',') -eq 'full,incremental,incremental,full,incremental') "five runs made two chains ($($kinds -join ','))"

    $listing = Versions $job
    $chains = @($listing.chains)
    Check ($chains.Count -eq 2 -and @($chains[0].images).Count -eq 3 -and @($chains[1].images).Count -eq 2) 'versions: two chains, of three and of two'
    Check (-not $chains[0].current -and $chains[1].current) 'versions: only the newer chain is the current one'
    $a = @($chains[0].images); $b = @($chains[1].images)
    Check ($a[0].role -eq 'full' -and -not $a[0].depends_on -and $a[1].depends_on -eq $a[0].image_path -and $a[2].depends_on -eq $a[1].image_path) 'versions: each increment names the copy under it'
    $onDisk = @(Get-ChildItem -LiteralPath $folder -Filter '*.qcow2' -File)
    Check ($onDisk.Count -eq 5 -and @(AllImages $listing | Where-Object { -not (Test-Path -LiteralPath $_.image_path) }).Count -eq 0) 'versions: the list is the files in the folder'
    Check (@(AllImages $listing | Where-Object { $_.size_bytes -le 0 }).Count -eq 0) 'versions: every copy has a size'

    # ---- both checks --------------------------------------------------------
    $r = Obk @('jobs', 'verify', $job)
    Check ($r.Code -eq 0 -and $r.Output.Contains([IO.Path]::GetFileName($b[1].image_path)) -and -not $r.Output.Contains([IO.Path]::GetFileName($a[2].image_path))) 'verify: the latest version is the newest copy, and it is whole'
    $r = Obk @('jobs', 'verify', $job, '--all')
    Check ($r.Code -eq 0 -and @(AllImages $listing | Where-Object { -not $r.Output.Contains($_.file_name) }).Count -eq 0) 'verify --all: all five copies read, all whole'

    # ---- save as VHDX: an increment, through its chain, into the job's own folder
    $target = Join-Path $folder 'saved-v3.vhdx'
    $r = Obk @('jobs', 'export', $a[2].image_path, $target)
    Check ($r.Code -eq 0 -and (Test-Path -LiteralPath $target) -and -not (Test-Path -LiteralPath ($target + '.partial'))) "export: a qcow2 increment saved as VHDX ($([int]((Get-Item $target).Length / 1MB)) MiB)"
    $inside = HashInsideVhdx $target
    Say "MENU-EXPORT qcow2 inside=$inside expected=$($hash[3])"
    Check ($inside -eq $hash[3]) 'export: Windows attaches the file by itself, and the data in it is version 3'
    Check ($inside -ne $hash[5]) 'export: and not the newest state of the source'
    $r = Obk @('verify', $target)
    Check ($r.Code -eq 0 -and $r.Output.Contains('VALID')) 'export: the saved file checks as a copy of its own'
    $stamp = (Get-Item -LiteralPath $target).LastWriteTimeUtc
    $r = Obk @('jobs', 'export', $a[2].image_path, $target)
    Check ($r.Code -ne 0 -and (Get-Item -LiteralPath $target).LastWriteTimeUtc -eq $stamp) 'export: an existing file is refused, not replaced'
    Check (@(AllImages (Versions $job)).Count -eq 5) 'export: a file saved into the job folder is not counted among its versions'
    $r = Obk @('jobs', 'verify', $job, '--all')
    Check ($r.Code -eq 0) 'export: and does not disturb the check of all versions'

    # ---- a damaged copy is found, and named ---------------------------------
    $victim = $a[1].image_path
    $stream = [IO.File]::Open($victim, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite)
    try {
        $junk = New-Object byte[] (64KB)
        ([Random]::new(7)).NextBytes($junk)
        $stream.Position = [long]($stream.Length / 2)
        $stream.Write($junk, 0, $junk.Length)
        $stream.Flush($true)
    } finally { $stream.Dispose() }
    $r = Obk @('jobs', 'verify', $job, '--all')
    Check ($r.Code -ne 0 -and $r.Output.Contains([IO.Path]::GetFileName($b[1].image_path))) 'verify --all: a damaged copy fails the check, and the others are still read'
    $r = Obk @('jobs', 'verify', $job)
    Check ($r.Code -eq 0) 'verify: the latest version does not stand on the damaged copy and stays whole'

    # ---- cleanup of versions ------------------------------------------------
    $r = Obk @('jobs', 'clean', $job, '--image', $a[0].image_path)
    Check ($r.Code -ne 0 -and @(Get-ChildItem -LiteralPath $folder -Filter '*.qcow2' -File).Count -eq 5) 'clean: a full copy is not deleted from under its increments'
    $r = Obk @('jobs', 'clean', $job, '--image', $a[1].image_path)
    Check ($r.Code -ne 0 -and (Test-Path -LiteralPath $a[1].image_path)) 'clean: nor an increment from under the next one'
    $r = Obk @('jobs', 'clean', $job, '--image', $target)
    Check ($r.Code -ne 0 -and (Test-Path -LiteralPath $target)) 'clean: a file that is not a version of the job is refused'
    $r = Obk @('jobs', 'clean', $job, '--image', $a[2].image_path)
    Check ($r.Code -eq 0 -and -not (Test-Path -LiteralPath $a[2].image_path) -and (Test-Path -LiteralPath $a[1].image_path)) 'clean: the last increment of a chain goes alone'
    $r = Obk @('jobs', 'clean', $job, '--image', $a[0].image_path, '--image', $a[1].image_path)
    Check ($r.Code -eq 0 -and -not (Test-Path -LiteralPath $a[0].image_path) -and -not (Test-Path -LiteralPath $a[1].image_path)) 'clean: a copy goes together with everything built on it'
    $left = @(Get-ChildItem -LiteralPath $folder -File | ForEach-Object { $_.Name } | Sort-Object)
    Say "MENU-FOLDER $($left -join ' ')"
    Check (@($left | Where-Object { $_ -like '*.qcow2' }).Count -eq 2 -and @($left | Where-Object { $_ -like '*.json' -or $_ -like '*.partial' }).Count -eq 0) 'clean: two copies left, nothing half-done beside them'
    $listing = Versions $job
    Check (@($listing.chains).Count -eq 1 -and $listing.chains[0].current -and @(AllImages $listing).Count -eq 2) 'clean: the list shows the one chain that is left'
    $r = Obk @('jobs', 'verify', $job, '--all')
    Check ($r.Code -eq 0) 'clean: what is left is whole'

    # ---- rename: the chain carries on under the new name ---------------------
    $edited = (Must @('jobs', 'show', $job)) | ConvertFrom-Json
    $edited.name = 'Alpha renamed'
    [IO.File]::WriteAllText('C:\Lab\menu\job.json', ($edited | ConvertTo-Json -Depth 32), ([Text.UTF8Encoding]::new($false)))
    $null = Must @('jobs', 'save', '--file', 'C:\Lab\menu\job.json')
    $hash[6] = Mutate 20261012
    $run = RunJob $job
    $listing = Versions $job
    $tail = @($listing.chains[0].images)
    Check ($run.backup_kind -eq 'incremental' -and @($listing.chains).Count -eq 1 -and $tail.Count -eq 3 -and $tail[2].depends_on -eq $tail[1].image_path) 'rename: the next run is an increment of the same chain'

    # ---- VHDX: one chain [full inc], saved from the increment ----------------
    $folderV = 'C:\Lab\menu\vhdx'
    $jobV = NewJob 'Beta vhdx' 'vhdx' $folderV
    $kinds = @((RunJob $jobV).backup_kind)
    $hash[7] = Mutate 20261013
    $kinds += (RunJob $jobV).backup_kind
    Check (($kinds -join ',') -eq 'full,incremental') 'vhdx: a full copy and an increment'
    $listingV = Versions $jobV
    $v = @($listingV.chains[0].images)
    Check ($v.Count -eq 2 -and $v[1].depends_on -eq $v[0].image_path -and $v[1].container -eq 'vhdx') 'vhdx: versions lists both, the second on the first'
    $r = Obk @('jobs', 'verify', $jobV, '--all')
    Check ($r.Code -eq 0) 'vhdx: both copies whole'
    New-Item -ItemType Directory -Force -Path 'C:\Lab\menu\out' | Out-Null
    $targetV = 'C:\Lab\menu\out\saved from vhdx.vhdx'
    $r = Obk @('jobs', 'export', $v[1].image_path, $targetV)
    Check ($r.Code -eq 0 -and (Test-Path -LiteralPath $targetV)) "vhdx: an increment saved as one file ($([int]((Get-Item $targetV).Length / 1MB)) MiB), into a folder with a space in its name"
    $inside = HashInsideVhdx $targetV
    Say "MENU-EXPORT vhdx inside=$inside expected=$($hash[7])"
    Check ($inside -eq $hash[7]) 'vhdx: Windows attaches it, and the data in it is that version'
    $r = Obk @('verify', $targetV)
    Check ($r.Code -eq 0 -and $r.Output.Contains('VALID')) 'vhdx: the saved file checks as a copy of its own'
    Check ($hash[6] -ne $hash[7]) 'the source did change between the versions compared'

    # What boot 3 needs: the two jobs, and the window's runtime.
    [IO.File]::WriteAllText('C:\Lab\menu\state.json', (@{
        qcow = $job; vhdx = $jobV; folder = $folder; folderV = $folderV
        latest = $tail[2].image_path; latest_hash = $hash[6]; previous_hash = $hash[5]
    } | ConvertTo-Json))
    $webviewKey = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
    if (-not (Get-ItemProperty $webviewKey -ErrorAction SilentlyContinue).pv) {
        $install = Start-Process 'C:\Lab\WebView2Runtime-x64.exe' -ArgumentList '/silent /install' -PassThru -Wait
        Check ($install.ExitCode -eq 0 -and [bool](Get-ItemProperty $webviewKey -ErrorAction SilentlyContinue).pv) 'WebView2 runtime installed for the window'
    }
    $startup = [Environment]::GetFolderPath('CommonStartup')
    Get-ChildItem $startup -Filter 'OwnBackup*.cmd' | Remove-Item -Force
    [IO.File]::WriteAllText((Join-Path $startup 'OwnBackupJobMenu.cmd'),
        "@echo off`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\Lab\job-menu-user.ps1`r`n")
    $answer = New-Object Xml.XmlDocument; $answer.Load('C:\Lab\autounattend.xml')
    $password = $answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $winlogon DefaultUserName 'lab'; Set-ItemProperty $winlogon DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $winlogon DefaultPassword $password; Set-ItemProperty $winlogon AutoAdminLogon '1'; Set-ItemProperty $winlogon AutoLogonCount 1
    [IO.File]::WriteAllText($ready, 'ready')
    Say 'MENU-STAGE-READY REBOOT'; shutdown.exe /r /t 5 | Out-Null
} catch {
    Say "MENU-STAGE-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    shutdown.exe /s /t 5 | Out-Null
}
