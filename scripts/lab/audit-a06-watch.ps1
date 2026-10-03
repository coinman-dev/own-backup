# SYSTEM: compare protected bytes around RPC and verify a subsequent Windows boot.
function Say([string]$Text) {
    try { $p=New-Object System.IO.Ports.SerialPort 'COM1',115200,'None',8,'One';$p.Open();$p.WriteLine($Text);$p.Close() } catch { }
}
function HashRange([int]$Disk,[long]$Offset,[long]$Length) {
    $file=[IO.FileStream]::new("\\.\PhysicalDrive$Disk",[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::ReadWrite)
    $hash=[Security.Cryptography.SHA256]::Create();$buffer=[byte[]]::new(1MB)
    try {
        $null=$file.Seek($Offset,[IO.SeekOrigin]::Begin);$left=$Length
        while ($left -gt 0) {$want=[int][Math]::Min($left,$buffer.Length);$n=$file.Read($buffer,0,$want);if ($n -le 0) {throw 'raw hash ended early'};$null=$hash.TransformBlock($buffer,0,$n,$buffer,0);$left-=$n}
        $null=$hash.TransformFinalBlock([byte[]]::new(0),0,0)
        return ([BitConverter]::ToString($hash.Hash)).Replace('-','')
    } finally {$file.Dispose();$hash.Dispose()}
}
function Snapshot {
    $c=Get-Partition -DriveLetter C
    return @{kernel=(Get-FileHash 'C:\Windows\System32\kernel32.dll' -Algorithm SHA256).Hash;system_head=(HashRange 0 0 1MB);system_boot=(HashRange 0 $c.Offset 64KB);esp=(HashRange 1 0 512MB)}
}
function Remount {
    Set-Disk -Number 2 -IsOffline $false
    $parts=@(Get-Partition -DiskNumber 2 | Where-Object {$_.Size -eq 64MB -and $_.GptType -eq '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}'})
    if ($parts.Count -ne 1) { throw 'unique restored data partition missing' }
    if ($parts[0].DriveLetter -ne 'T') { Add-PartitionAccessPath -DiskNumber 2 -PartitionNumber $parts[0].PartitionNumber -AccessPath 'T:\' }
}
$ErrorActionPreference='Stop'
$rebooting=$false
try {
    $root='C:\Lab\a06'
    $mount=cmd.exe /d /c 'mountvol.exe S: /s 2>&1' | Out-String
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path 'S:\EFI\Microsoft\Boot\bootmgfw.efi')) { throw "ESP mount failed: $mount" }
    if ((Get-Content "$root\phase.txt" -Raw).Trim() -eq 'reboot-proof') {
        $out=cmd.exe /d /c "C:\Lab\audit_a06_guard.exe live $root\after-reboot-guard.json 2>&1" | Out-String
        if ($LASTEXITCODE -ne 0) { throw "Windows reboot guard proof failed: $out" }
        $baseline=Get-Content "$root\baseline.json" -Raw | ConvertFrom-Json
        if ((Get-FileHash 'C:\Windows\System32\kernel32.dll' -Algorithm SHA256).Hash -ne $baseline.kernel) { throw 'kernel32 changed after refusal/reboot' }
        [IO.File]::WriteAllText("$root\reboot-proof.json",(@{pass=$true;windows_booted=$true;kernel_sha256=$baseline.kernel}|ConvertTo-Json),([Text.UTF8Encoding]::new($false)))
        Say 'A06-PASS cli_refusals=6 rpc_refusals=6 unused_cli=2 unused_rpc=2 windows_reboot=true'
        return
    }
    $hashes=$false;$remounted=@{}
    if ((Get-Content "$root\phase.txt" -Raw).Trim() -eq 'positive') {
        foreach ($kind in @('volume','disk')) {
            Remove-Item "$root\rpc-positive-$kind.json","$root\remounted-$kind.marker","$root\rpc-file-$kind.json" -Force -ErrorAction SilentlyContinue
        }
        Remove-Item "$root\rpc-all-done.marker" -Force -ErrorAction SilentlyContinue
        Remount;$hashes=$true
        [IO.File]::WriteAllText("$root\positive-ready.marker",'ready')
    } else {
        $out=cmd.exe /d /c "C:\Lab\audit_a06_guard.exe live $root\rpc-live-guard.json 2>&1" | Out-String
        if ($LASTEXITCODE -ne 0) { throw "RPC volume mapping/guard check failed: $out" }
        $before=Snapshot
        [IO.File]::WriteAllText("$root\rpc-ready.marker",'ready')
    }
    for ($tick=0;$tick -lt 600;$tick++) {
        if (Test-Path "$root\user-failure.txt") { throw (Get-Content "$root\user-failure.txt" -Raw) }
        if (-not $hashes -and (Test-Path "$root\rpc-results.json")) {
            $result=Get-Content "$root\rpc-results.json" -Raw | ConvertFrom-Json
            if (-not $result.pass -or $result.refusals.Count -ne 6) { throw 'RPC refusal matrix incomplete' }
            $after=Snapshot
            foreach ($key in $before.Keys) { if ($before[$key] -ne $after[$key]) { throw "RPC changed protected bytes: $key" } }
            [IO.File]::WriteAllText("$root\rpc-hashes.json",(@{pass=$true;before=$before;after=$after}|ConvertTo-Json -Depth 4),([Text.UTF8Encoding]::new($false)))
            [IO.File]::WriteAllText("$root\rpc-hashes-passed.marker",'ready')
            [IO.File]::WriteAllText("$root\phase.txt",'positive')
            $hashes=$true;Say 'A06-RPC-HASHES-PASS'
        }
        foreach ($kind in @('volume','disk')) {
            if (-not $remounted.ContainsKey($kind) -and (Test-Path "$root\rpc-positive-$kind.json")) {
                Remount
                [IO.File]::WriteAllText("$root\remounted-$kind.marker",'ready');$remounted[$kind]=$true
            }
        }
        if (Test-Path "$root\rpc-all-done.marker") { break }
        Start-Sleep -Seconds 1
    }
    if (-not (Test-Path "$root\rpc-all-done.marker")) { throw 'RPC matrix timeout' }
    New-Item -ItemType Directory -Force 'S:\a06-fixture' | Out-Null
    Copy-Item "$root\disk.qcow2" 'S:\a06-fixture\disk.qcow2' -Force
    $image=(Get-FileHash "$root\disk.qcow2" -Algorithm SHA256).Hash
    if ((Get-FileHash 'S:\a06-fixture\disk.qcow2' -Algorithm SHA256).Hash -ne $image) { throw 'PE source copy differs' }
    [IO.File]::WriteAllText('S:\a06-fixture\source.sha256',$image)
    [IO.File]::WriteAllText("$root\phase.txt",'reboot-proof')
    Say 'A06-MATRIX-PASS REBOOT-PROOF-NEXT'
    $rebooting=$true
    shutdown.exe /r /t 5 | Out-Null
} catch { Say "A06-WATCH-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)" }
finally { if (-not $rebooting) { Say 'A06-WATCH-DONE';shutdown.exe /s /t 5 | Out-Null } }
