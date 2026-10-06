# Boot 3 of scripts/lab/run-job-menu.py, in the desktop session of `lab`: the
# menu of a job in the real window, pressed the way a person presses it.
#
# The window's page is found through UI Automation -- every button by the name
# a screen reader would say -- and pressed with the mouse at the place where it
# is drawn. Nothing is asked of the page's internals, so a button that is there
# but covered, off the screen or unnamed fails here as it would for a person.
# After each press the result is looked for OUTSIDE the window: files in the
# folder, the job as the service stores it, a disk with the right data on it.
#
# ASCII only (see job-menu-stage.ps1). The names of the buttons are Russian;
# they come from job-menu-labels.json, which the driver cuts out of the
# window's own message file, and are read as UTF-8 explicitly.

function Say([string]$Text) {
    try { $p = New-Object IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'; $p.Open(); $p.WriteLine($Text); $p.Close() } catch { }
}
function Check([bool]$Ok, [string]$What) {
    if (-not $Ok) { throw "check failed: $What" }
    Say "MENU-OK $What"
}
# The host needs about three seconds for a photograph and a key.
function HostDo([string]$Shot, [string]$Keys = '') {
    Say "HOST shot=$Shot keys=$Keys"
    Start-Sleep -Seconds 5
}
function WaitFor([scriptblock]$Condition, [int]$Seconds, [string]$What) {
    for ($n = 0; $n -lt $Seconds * 2; $n++) {
        if (& $Condition) { return }
        Start-Sleep -Milliseconds 500
    }
    throw "timeout: $What"
}
function Obk([string[]]$Arguments) {
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = (& 'C:\Lab\own-backup-cli.exe' @Arguments 2>&1 | Out-String).Trim()
        return [pscustomobject]@{ Code = [int]$LASTEXITCODE; Output = [string]$output }
    } finally { $ErrorActionPreference = $old }
}
function Versions([string]$Id) {
    $r = Obk @('jobs', 'versions', $Id, '--json')
    if ($r.Code -ne 0) { throw "versions failed: $($r.Output)" }
    @(($r.Output | ConvertFrom-Json).chains | ForEach-Object { $_.images })
}

Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Windows.Forms
Add-Type -Namespace Lab -Name Input -MemberDefinition @'
[DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
'@
$automation = [Windows.Automation.AutomationElement]
$everything = [Windows.Automation.TreeScope]::Descendants

function Root { $automation::FromHandle($script:gui.MainWindowHandle) }
function OnScreen($Element) {
    try { -not $Element.Current.IsOffscreen -and -not $Element.Current.BoundingRectangle.IsEmpty } catch { $false }
}
# Every element with this name that is drawn right now.
function Named([string]$Label) {
    $name = $script:labels.$Label
    if (-not $name) { throw "no label: $Label" }
    $condition = New-Object Windows.Automation.PropertyCondition ($automation::NameProperty, [string]$name)
    @((Root).FindAll($everything, $condition) | Where-Object { OnScreen $_ })
}
function Find([string]$Label, [int]$Seconds = 25, [int]$Index = 0) {
    for ($n = 0; $n -lt $Seconds * 2; $n++) {
        $found = @(Named $Label)
        if ($found.Count -gt $Index) { return $found[$Index] }
        Start-Sleep -Milliseconds 500
    }
    throw "not on screen: $Label"
}
function Gone([string]$Label, [int]$Seconds, [string]$What) {
    WaitFor { @(Named $Label).Count -eq 0 } $Seconds $What
}
# A text on the page that begins like this (a sentence with numbers in it).
function Beginning([string]$Label, [int]$Seconds = 25) {
    $prefix = [string]$script:labels.$Label
    for ($n = 0; $n -lt $Seconds * 2; $n++) {
        foreach ($e in (Root).FindAll($everything, [Windows.Automation.Condition]::TrueCondition)) {
            try { if ($e.Current.Name.StartsWith($prefix) -and (OnScreen $e)) { return $e } } catch { }
        }
        Start-Sleep -Milliseconds 500
    }
    throw "no text beginning like: $Label"
}
function Click($Element) {
    $r = $Element.Current.BoundingRectangle
    $null = [Lab.Input]::SetCursorPos([int]($r.X + $r.Width / 2), [int]($r.Y + $r.Height / 2))
    Start-Sleep -Milliseconds 250
    [Lab.Input]::mouse_event(0x02, 0, 0, 0, [UIntPtr]::Zero)
    [Lab.Input]::mouse_event(0x04, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 700
}
function Press([string]$Label, [int]$Index = 0) { Click (Find $Label 25 $Index) }
# The button at the bottom of a window, when the cross in its corner is called
# the same.
function PressLast([string]$Label) { $null = Find $Label; Click (@(Named $Label)[-1]) }
# The menu of the first job in the list, and one item of it.
function Menu([string]$Item) {
    Press 'menu'
    Press $Item
}
# A window started from this hidden script opens behind the desktop; Alt+Tab on
# the real keyboard is allowed to bring it forward.
function Focus {
    for ($n = 0; $n -lt 4; $n++) {
        $script:gui.Refresh()
        if ([Lab.Input]::GetForegroundWindow() -eq $script:gui.MainWindowHandle) { return }
        Say 'HOST shot= keys=alt-tab'
        Start-Sleep -Seconds 3
    }
    throw 'the window is not in front'
}

$ErrorActionPreference = 'Stop'
try {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $elevated = [Security.Principal.WindowsPrincipal]::new($identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    Say "MENU-USER-BEGIN identity=$($identity.Name) elevated=$elevated ui=$((Get-UICulture).Name)"
    $script:labels = [IO.File]::ReadAllText('C:\Lab\job-menu-labels.json', [Text.Encoding]::UTF8) | ConvertFrom-Json
    $state = Get-Content 'C:\Lab\menu\state.json' -Raw | ConvertFrom-Json
    WaitFor { (Get-Service OwnBackup -ErrorAction SilentlyContinue).Status -eq 'Running' } 180 'service running'
    WaitFor { (Obk @('jobs', 'status')).Code -eq 0 } 60 'service answering'
    $before = @(Versions $state.qcow)
    Check ($before.Count -eq 3) 'the job of the CLI stage is here with its three versions'

    Remove-Item -LiteralPath (Join-Path $env:LOCALAPPDATA 'com.ownbackup.gui') -Recurse -Force -ErrorAction SilentlyContinue
    $marker = Join-Path ([IO.Path]::GetTempPath()) 'own-backup-gui-up'
    Remove-Item $marker -Force -ErrorAction SilentlyContinue
    $script:gui = Start-Process 'C:\Lab\own-backup-gui.exe' -PassThru
    WaitFor { $script:gui.Refresh(); (Test-Path $marker) -and $script:gui.MainWindowHandle -ne [IntPtr]::Zero } 180 'window rendered'
    Start-Sleep -Seconds 6
    Focus
    Say 'HOST shot= keys=meta_l-up'
    Start-Sleep -Seconds 5
    HostDo 'window'

    # ---- the menu itself ----------------------------------------------------
    $null = Find 'menu' 60
    Check (@(Named 'menu').Count -eq 2) 'each of the two jobs has its menu button, and a screen reader can name it'
    Press 'menu'
    $items = 'rename', 'verify_latest', 'verify_all', 'clean', 'clone', 'mount', 'open', 'export', 'delete'
    foreach ($item in $items) { $null = Find $item 10 }
    Check $true "the menu shows all $($items.Count) items"
    HostDo 'menu'

    # ---- cleanup of versions: delete the newest through the window ----------
    Press 'clean'
    $null = Find 'clean_title'
    $boxes = New-Object Windows.Automation.PropertyCondition ($automation::ControlTypeProperty, [Windows.Automation.ControlType]::CheckBox)
    WaitFor { @((Root).FindAll($everything, $boxes) | Where-Object { OnScreen $_ }).Count -eq 4 } 30 'one chain and three versions listed'
    HostDo 'clean'
    $ticks = @((Root).FindAll($everything, $boxes) | Where-Object { OnScreen $_ })
    Click $ticks[3]
    HostDo 'clean-ticked'
    Press 'delete_button'
    HostDo 'clean-confirm'
    Check (Test-Path -LiteralPath $state.latest) 'asking is not deleting: the file is there until the second press'
    Press 'delete_button'
    WaitFor { -not (Test-Path -LiteralPath $state.latest) } 60 'the ticked version deleted from the disk'
    $null = Beginning 'deleted_prefix'
    HostDo 'clean-done'
    $after = @(Versions $state.qcow)
    Check ($after.Count -eq 2 -and @($after | Where-Object { $_.image_path -eq $state.latest }).Count -eq 0) 'clean: the window deleted exactly the version that was ticked'
    PressLast 'close'
    Gone 'clean_title' 15 'cleanup window closed'

    # ---- both checks --------------------------------------------------------
    Menu 'verify_latest'
    $null = Find 'verify_latest_title'
    $null = Beginning 'verify_ok_prefix' 300
    HostDo 'verify-latest'
    Check $true 'verify: the latest version read through the window, no damage'
    PressLast 'close'
    Gone 'verify_latest_title' 15 'check window closed'
    Menu 'verify_all'
    $null = Find 'verify_all_title'
    $null = Beginning 'verify_ok_prefix' 300
    HostDo 'verify-all'
    Check $true 'verify: all versions read through the window, no damage'
    PressLast 'close'
    Gone 'verify_all_title' 15 'check window closed'

    # Losing an allocation entry is damage too. A check that reads only the
    # clusters still marked present would silently skip the missing data.
    # Preserve GPT (the first MiB) and the sidecar (beyond 512 MiB); temporarily
    # mark the data in the first L2 table absent, then restore that table.
    $base = $after[0].image_path
    $stream = [IO.File]::Open($base, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite)
    try {
        $bytes = New-Object byte[] 8
        $stream.Position = 20; $null = $stream.Read($bytes, 0, 4)
        $bits = ([int]$bytes[0] -shl 24) -bor ([int]$bytes[1] -shl 16) -bor ([int]$bytes[2] -shl 8) -bor [int]$bytes[3]
        $cluster = 1 -shl $bits
        $stream.Position = 40; $null = $stream.Read($bytes, 0, 8); [Array]::Reverse($bytes)
        $l1 = [BitConverter]::ToUInt64($bytes, 0)
        $stream.Position = [long]$l1; $null = $stream.Read($bytes, 0, 8); [Array]::Reverse($bytes)
        $l2 = [BitConverter]::ToUInt64($bytes, 0) -band [UInt64]0x00FFFFFFFFFFFE00
        $savedTable = New-Object byte[] $cluster
        $stream.Position = [long]$l2
        Check ($stream.Read($savedTable, 0, $cluster) -eq $cluster) 'the full copy allocation table can be read'
        $skip = [int](1MB / $cluster) * 8
        $stream.Position = [long]$l2 + $skip
        $stream.Write((New-Object byte[] ($cluster - $skip)), 0, $cluster - $skip)
        $stream.Flush($true)
    } finally { $stream.Dispose() }
    try {
        Menu 'verify_all'
        $null = Find 'verify_all_title'
        $null = Beginning 'verify_bad_prefix' 300
        HostDo 'verify-map-damaged'
        Check $true 'verify all: missing allocation entries are detected, not skipped'
        PressLast 'close'
        Gone 'verify_all_title' 15 'damaged-copy check window closed'
    } finally {
        $stream = [IO.File]::Open($base, [IO.FileMode]::Open, [IO.FileAccess]::Write)
        try { $stream.Position = [long]$l2; $stream.Write($savedTable, 0, $savedTable.Length); $stream.Flush($true) } finally { $stream.Dispose() }
    }

    # ---- save as VHDX -------------------------------------------------------
    Menu 'export'
    $null = Find 'export_title'
    HostDo 'export-form'
    Press 'save'
    $null = Beginning 'export_done_prefix' 300
    HostDo 'export-done'
    $saved = @(Get-ChildItem -LiteralPath $state.folder -Filter 'Alpha renamed *.vhdx' -File)
    Check ($saved.Count -eq 1 -and @(Get-ChildItem -LiteralPath $state.folder -Filter '*.partial').Count -eq 0) "export: the window saved $($saved[0].Name)"
    $r = Obk @('verify', $saved[0].FullName)
    Check ($r.Code -eq 0 -and $r.Output.Contains('VALID')) 'export: and the file checks as a copy of its own'
    Check (@(Versions $state.qcow).Count -eq 2) 'export: the job still has two versions'
    PressLast 'close'
    Gone 'export_title' 15 'save window closed'

    # ---- mount as a disk ----------------------------------------------------
    $drives = @([IO.DriveInfo]::GetDrives() | ForEach-Object { $_.Name })
    Menu 'mount'
    $null = Find 'mount_title'
    HostDo 'mount-form'
    Press 'mount_button'
    $null = Find 'tools_title' 30
    $inside = $null
    WaitFor {
        foreach ($drive in [IO.DriveInfo]::GetDrives()) {
            $file = $drive.Name + 'menu\data.bin'
            if ($drives -notcontains $drive.Name -and (Test-Path -LiteralPath $file)) { $script:mounted = $file; return $true }
        }
        $false
    } 180 'the version appeared as a disk with a letter'
    $inside = (Get-FileHash -LiteralPath $script:mounted -Algorithm SHA256).Hash
    Say "MENU-MOUNT file=$script:mounted inside=$inside expected=$($state.previous_hash)"
    HostDo 'mounted'
    Check ($inside -eq $state.previous_hash) 'mount: the disk the window attached holds the newest version left'
    $r = Obk @('unmount', $after[1].image_path)
    Check ($r.Code -eq 0) 'mount: and it detaches'
    Press 'nav_backup'

    # ---- rename -------------------------------------------------------------
    Menu 'rename'
    $null = Find 'rename_title'
    Start-Sleep -Seconds 1
    [Windows.Forms.SendKeys]::SendWait('Alpha in window')
    HostDo 'rename'
    [Windows.Forms.SendKeys]::SendWait('{ENTER}')
    WaitFor { ((Obk @('jobs', 'show', $state.qcow)).Output | ConvertFrom-Json).name -eq 'Alpha in window' } 30 'the service stores the new name'
    Check $true 'rename: typed over the old name, stored by the service'
    Check (@(Versions $state.qcow).Count -eq 2) 'rename: the versions are still the job''s'

    # ---- clone settings -----------------------------------------------------
    Menu 'clone'
    $null = Find 'other_disk' 30
    HostDo 'clone'
    Check $true 'clone: the wizard opens with the disk already chosen'
    Press 'cancel'

    # ---- open location ------------------------------------------------------
    Menu 'open'
    $shell = New-Object -ComObject Shell.Application
    WaitFor { @($shell.Windows() | Where-Object { try { $_.Document.Folder.Self.Path -eq $state.folder } catch { $false } }).Count -gt 0 } 60 'Explorer opened the folder'
    HostDo 'opened'
    Check $true 'open: Explorer shows the job folder'
    @($shell.Windows()) | ForEach-Object { try { $_.Quit() } catch { } }
    Start-Sleep -Seconds 2

    # ---- delete: the question, and no more ----------------------------------
    Focus
    Menu 'delete'
    $null = Find 'delete_title'
    HostDo 'delete'
    Press 'cancel'
    Check ((Obk @('jobs', 'show', $state.qcow)).Code -eq 0) 'delete: the menu asks first, and Cancel keeps the job'
    Say 'MENU-PASS'
} catch {
    Say "MENU-USER-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
    try {
        $log = Get-ChildItem 'C:\Lab\Logs\own-backup-service-*.log' |
            Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1
        if ($log) {
            $tail = (Get-Content -LiteralPath $log.FullName -Tail 60 | Out-String)
            Say "MENU-TEXT service-log $([Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($tail)))"
        }
    } catch { }
    HostDo 'failure'
} finally {
    Say 'MENU-DONE'
    shutdown.exe /s /t 5 | Out-Null
}
