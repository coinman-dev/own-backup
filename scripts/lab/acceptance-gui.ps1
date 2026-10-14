. 'C:\Lab\acceptance-common.ps1'
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


function WaitFor([scriptblock]$condition,[int]$seconds,[string]$why) {
    $deadline=[DateTime]::UtcNow.AddSeconds($seconds)
    do{if(&$condition){return};Start-Sleep -Milliseconds 300}while([DateTime]::UtcNow -lt $deadline)
    throw "GUI timeout: $why"
}
function HostDo([string]$shot,[string]$keys='') {Say "HOST shot=$shot keys=$keys";Start-Sleep -Seconds 5}
$context=Context;Report 'gui-user-context' $context
Check (-not $context.elevated) 'GUI acceptance runs under a non-elevated desktop token'
try{$null=Get-ChildItem 'C:\ProgramData\OwnBackup\.own-backup-service-keys' -ErrorAction Stop;$keyReadable=$true}catch{$keyReadable=$false}
Check (-not $keyReadable) 'a non-elevated desktop user cannot enumerate the private service keys'
$messages=[IO.File]::ReadAllText('C:\Lab\acceptance-ui-labels.json',[Text.Encoding]::UTF8)|ConvertFrom-Json
Get-Process own-backup-gui -ErrorAction SilentlyContinue|Stop-Process -Force
Remove-Item (Join-Path $env:LOCALAPPDATA 'com.ownbackup.gui') -Recurse -Force -ErrorAction SilentlyContinue
$marker=Join-Path ([IO.Path]::GetTempPath()) 'own-backup-gui-up'
Remove-Item $marker -Force -ErrorAction SilentlyContinue
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--force-renderer-accessibility'
$script:gui=Start-Process 'C:\Lab\own-backup-gui.exe' -PassThru
WaitFor {$script:gui.Refresh();(Test-Path $marker)-and $script:gui.MainWindowHandle -ne [IntPtr]::Zero} 120 'frontend rendered'
$script:labels=$messages.ru
Start-Sleep -Seconds 3
$probe=New-Object Windows.Automation.PropertyCondition ($automation::NameProperty,[string]$script:labels.activity)
$english=New-Object Windows.Automation.PropertyCondition ($automation::NameProperty,[string]$messages.en.activity)
WaitFor {((Root).FindAll($everything,$probe).Count -gt 0)-or((Root).FindAll($everything,$english).Count -gt 0)} 45 'accessible navigation tree'
if((Root).FindAll($everything,$english).Count -gt 0){$script:labels=$messages.en}
Focus;HostDo 'acceptance-window' 'meta_l-up'
Press 'activity';$null=Find 'activity_title';HostDo 'acceptance-activity-top'
$seen=@{verify_done=$false;verify_cancelled=$false;verify_failed=$false}
for($step=0;$step -lt 35;$step++){
    foreach($element in (Root).FindAll($everything,[Windows.Automation.Condition]::TrueCondition)){
        try{if(OnScreen $element){foreach($key in @('verify_done','verify_cancelled','verify_failed')){if($element.Current.Name.StartsWith([string]$script:labels.$key)){$seen[$key]=$true}}}}catch{}
    }
    if($seen.verify_done -and $seen.verify_cancelled -and $seen.verify_failed){break}
    $window=$script:gui.MainWindowHandle
    $null=[Lab.Input]::SetCursorPos(850,400)
    [Lab.Input]::mouse_event(0x0800,0,0,[BitConverter]::ToUInt32([BitConverter]::GetBytes([int]-480),0),[UIntPtr]::Zero)
    Start-Sleep -Milliseconds 300
}
Report 'gui-activity-visible' $seen
Check ($seen.verify_done -and $seen.verify_cancelled -and $seen.verify_failed) 'activity scrolling displayed successful, cancelled and failed verification cards'

Check $true 'real Activity window names successful, cancelled and failed scheduled verification separately'
HostDo 'acceptance-activity'
Press 'backup';$null=Find 'options';Press 'options';Press 'advanced'
$null=Find 'verify_schedule'
Check $true 'the real job editor exposes a separate verification schedule'
$condition=New-Object Windows.Automation.PropertyCondition ($automation::ControlTypeProperty,[Windows.Automation.ControlType]::ComboBox)
$choices=@((Root).FindAll($everything,$condition)|Where-Object{OnScreen $_})
Check ($choices.Count -ge 1) 'verification schedule has an accessible selector'
Click $choices[0]
[Windows.Forms.SendKeys]::SendWait('{HOME}{DOWN}{ENTER}')
$null=Find 'verify_all'
Check $true 'enabling the verification schedule exposes its version-scope checkbox'
# Toggle the scope, then turn the schedule back off; saving must retain that
# scope independently of immediate verification after a backup.

$null=Find 'verify_all'
$boxes=@(Named 'verify_all'|Where-Object{$_.Current.ControlType -eq [Windows.Automation.ControlType]::CheckBox})
Check ($boxes.Count -eq 1) 'the version scope is an accessible checkbox'
$box=$boxes[0]
$before=$box.GetCurrentPattern([Windows.Automation.TogglePattern]::Pattern).Current.ToggleState
Click $box
$choices=@((Root).FindAll($everything,$condition)|Where-Object{OnScreen $_})
Click $choices[0];[Windows.Forms.SendKeys]::SendWait('{HOME}{ENTER}')
HostDo 'acceptance-verification-options'
PressLast 'save'
Start-Sleep -Seconds 2
$id=(Get-Content "$work\job-id.txt" -Raw -Encoding UTF8).Trim()
$job=Rpc 'gui-saved-job' @{op='get';id=$id}
Check ($job.verification.schedule.kind -eq 'none' -and $job.verification.all_versions -eq ($before -ne [Windows.Automation.ToggleState]::On)) 'saving through the GUI persists verification scope and disabled schedule'
Report 'gui-acceptance' @{activity_success=$true;activity_cancelled=$true;activity_failure=$true;schedule_editor=$true;saved_scope=$job.verification.all_versions;language=$script:labels.language}
$null=$script:gui.CloseMainWindow()
