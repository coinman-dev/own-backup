. 'C:\Lab\acceptance-common.ps1'
$work='C:\Lab\reauthorize'
$installed='C:\Program Files\OwnBackup'
Add-Type -AssemblyName UIAutomationClient,UIAutomationTypes,System.Windows.Forms
Add-Type -Namespace Reauth -Name Input -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x,int y);
[DllImport("user32.dll")] public static extern void mouse_event(uint f,uint x,uint y,uint d,UIntPtr e);
[DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
'@
function WaitReady {
    for($i=0;$i -lt 60;$i++){try{$r=Rpc 'reauth-ready' @{op='status'};if($r){return}}catch{};Start-Sleep -Seconds 1}
    throw 'service not ready'
}
function Images {return @(Get-ChildItem 'T:\live' -Filter '*.obk'|ForEach-Object{[ordered]@{name=$_.Name;sha256=(Get-FileHash $_.FullName).Hash}}|Sort-Object {$_.name})}
function Visible($element){try{-not $element.Current.IsOffscreen -and -not $element.Current.BoundingRectangle.IsEmpty}catch{$false}}
function Root {[Windows.Automation.AutomationElement]::FromHandle($script:gui.MainWindowHandle)}
function Named([string]$name){
    $condition=[Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty,$name)
    return @((Root).FindAll([Windows.Automation.TreeScope]::Descendants,$condition)|Where-Object{Visible $_})
}
function Find([string]$name,[int]$seconds=30) {
    $deadline=[DateTime]::UtcNow.AddSeconds($seconds)
    do{$items=@(Named $name);if($items.Count){return $items[-1]};Start-Sleep -Milliseconds 300}while([DateTime]::UtcNow -lt $deadline)
    throw "GUI element missing: $name"
}
function FindJob([string]$name) {
    $deadline=[DateTime]::UtcNow.AddSeconds(30)
    do {
        $items=@((Root).FindAll([Windows.Automation.TreeScope]::Descendants,[Windows.Automation.Condition]::TrueCondition)|Where-Object{
            try{(Visible $_) -and $_.Current.Name.StartsWith($name) -and $_.Current.BoundingRectangle.X -lt 330}catch{$false}
        }|Sort-Object{$_.Current.BoundingRectangle.Width})
        if($items.Count){return $items[0]}
        Start-Sleep -Milliseconds 500
    }while([DateTime]::UtcNow -lt $deadline)
    Report 'reauth-accessibility-tree' @((Root).FindAll([Windows.Automation.TreeScope]::Descendants,[Windows.Automation.Condition]::TrueCondition)|Where-Object{Visible $_}|ForEach-Object{$_.Current.Name}|Select-Object -First 250)
    throw 'synthetic job row missing from the accessibility tree'
}
function Click($element){
    $r=$element.Current.BoundingRectangle
    [Reauth.Input]::SetCursorPos([int]($r.X+$r.Width/2),[int]($r.Y+$r.Height/2))|Out-Null
    [Reauth.Input]::mouse_event(2,0,0,0,[UIntPtr]::Zero);[Reauth.Input]::mouse_event(4,0,0,0,[UIntPtr]::Zero)
    Start-Sleep -Milliseconds 500
}
function Capture([string]$name,[string]$keys=''){Say "HOST shot=$name keys=$keys";Start-Sleep -Seconds 4}
function GuiPasswordRepair($job) {
    Get-Process own-backup-gui -ErrorAction SilentlyContinue|Stop-Process -Force
    $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--force-renderer-accessibility'
    $script:gui=Start-Process "$installed\own-backup-gui.exe" -PassThru
    $deadline=[DateTime]::UtcNow.AddSeconds(120)
    do{$script:gui.Refresh();if($script:gui.MainWindowHandle -ne [IntPtr]::Zero){break};Start-Sleep -Milliseconds 500}while([DateTime]::UtcNow -lt $deadline)
    Check ($script:gui.MainWindowHandle -ne [IntPtr]::Zero) 'the current GUI opened in the active console'
    for($i=0;$i -lt 4;$i++){if([Reauth.Input]::GetForegroundWindow() -eq $script:gui.MainWindowHandle){break};Capture 'reauth-focus' 'alt-tab'}
    $labels=Get-Content 'C:\Lab\reauthorize-labels.json' -Raw -Encoding UTF8|ConvertFrom-Json
    $deadline=[DateTime]::UtcNow.AddSeconds(60)
    do {
        if(@(Named $labels.en.backup).Count){$script:labels=$labels.en;break}
        if(@(Named $labels.ru.backup).Count){$script:labels=$labels.ru;break}
        Start-Sleep -Milliseconds 500
    }while([DateTime]::UtcNow -lt $deadline)
    Check ([bool]$script:labels) 'GUI accessibility navigation is available'
    Click (Find $script:labels.backup)
    Capture 'reauth-job-list'
    $title=FindJob $job.name
    Click $title
    $y=$title.Current.BoundingRectangle.Y
    $menus=@(Named $script:labels.menu|Sort-Object { [Math]::Abs($_.Current.BoundingRectangle.Y-$y) })
    Check ($menus.Count -gt 0 -and [Math]::Abs($menus[0].Current.BoundingRectangle.Y-$y) -lt 30) 'the menu belongs to the synthetic encrypted job'
    Click $menus[0];Click (Find $script:labels.action)
    $null=Find $script:labels.title
    Capture 'reauth-dialog'
    $edits=@((Root).FindAll([Windows.Automation.TreeScope]::Descendants,[Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::ControlTypeProperty,[Windows.Automation.ControlType]::Edit))|Where-Object{Visible $_})
    Check ($edits.Count -gt 0) 'the archive password field is available'
    Click $edits[-1];[Windows.Forms.SendKeys]::SendWait('deliberately-wrong-lab-password')
    Click (Find $script:labels.confirm)
    Start-Sleep -Seconds 6
    Check (@(Named $script:labels.title).Count -gt 0) 'a wrong GUI password keeps the authorization dialog open'
    $unchanged=Rpc 'gui-wrong-after' @{op='get';id=$job.id}
    Check ($unchanged.archive_encryption.local_key.id -eq $job.archive_encryption.local_key.id) 'a wrong GUI password leaves local authorization unchanged'
    Capture 'reauth-refused-password'
    Click $edits[-1];[Windows.Forms.SendKeys]::SendWait('^a')
    $password=([IO.File]::ReadAllText('C:\Lab\installer-live\password.txt')).Trim()
    [Windows.Forms.SendKeys]::SendWait($password)
    Click (Find $script:labels.confirm)
    $deadline=[DateTime]::UtcNow.AddSeconds(30)
    do{if(@(Named $script:labels.title).Count -eq 0){break};Start-Sleep -Milliseconds 500}while([DateTime]::UtcNow -lt $deadline)
    Check (@(Named $script:labels.title).Count -eq 0) 'the accepted archive password closes the GUI authorization dialog'
    Capture 'reauth-password-accepted'
    Stop-Process -Id $script:gui.Id -Force
}
try {
    WaitReady
    $id=(Get-Content 'C:\Lab\installer-live\live-create.request.result.json' -Raw -Encoding UTF8|ConvertFrom-Json).result.id
    $phase=(Get-Content "$work\phase.txt" -Raw).Trim()
    $job=Rpc "$phase-before" @{op='get';id=$id};$portable=ConvertTo-Json -InputObject $job.archive_encryption.portable_key -Depth 20 -Compress
    $before=Images;Report "$phase-images-before" $before
    $failed=Rpc "$phase-blocked-run" @{op='run';id=$id}
    Check ($failed.status -eq 'error') 'a damaged DPAPI authorization refuses an automatic backup'
    Check ((ConvertTo-Json -InputObject (Images) -Compress) -eq (ConvertTo-Json -InputObject $before -Compress)) 'a refused run did not modify any archive'
    if($phase -eq 'password') {
        $null=Rpc 'reauth-no-secret' @{op='reauthorize';id=$id;secret='none';expect_error=$true}
        $null=Rpc 'reauth-wrong-password' @{op='reauthorize';id=$id;secret='wrong-password';expect_error=$true}
        GuiPasswordRepair $job
    }elseif($phase -eq 'recovery') {
        $null=Rpc 'reauth-wrong-recovery' @{op='reauthorize';id=$id;secret='wrong-recovery';expect_error=$true}
        $null=Rpc 'reauth-correct-recovery' @{op='reauthorize';id=$id;secret='recovery';recovery_file='C:\Lab\installer-live\recovery.txt'}
    }else{throw "invalid user phase $phase"}
    $saved=Rpc "$phase-repaired-job" @{op='get';id=$id}
    Check ($saved.archive_encryption.local_key.id -ne $job.archive_encryption.local_key.id) 'repair issued a fresh local authorization reference'
    Check ((ConvertTo-Json -InputObject $saved.archive_encryption.portable_key -Depth 20 -Compress) -eq $portable) 'portable password and recovery envelope are byte-for-byte unchanged'
    Check ((ConvertTo-Json -InputObject (Images) -Compress) -eq (ConvertTo-Json -InputObject $before -Compress)) 'reauthorization does not rewrite archived images'
    $run=Rpc "$phase-repair-backup" @{op='run';id=$id}
    Check ($run.status -eq 'success' -and $run.backup_kind -eq 'incremental') 'repair resumed the same encrypted chain'
    $valid=Rpc "$phase-portable-verify" @{op='verify';image=$run.image_path;secret='recovery';recovery_file='C:\Lab\installer-live\recovery.txt'}
    Check $valid.valid 'the original recovery key still opens the new increment'
    if($phase -eq 'password'){Phase 'password-done'}else{
        [IO.File]::WriteAllText("$work\cold-before-run.txt",$run.id)
        $null=Rpc 'reauth-cold-schedule' @{op='save';id=$id;patch=@{schedule=@{kind='daily';time=(Get-Date).AddMinutes(3).ToString('HH:mm');run_missed=$false}}}
        Phase 'cold'
    }
    NoLogon;Say "REAUTHORIZE-REBOOT phase=$phase-finished";shutdown.exe /r /t 5|Out-Null
}catch{Say "REAUTHORIZE-FAIL user phase=$phase line=$($_.InvocationInfo.ScriptLineNumber) reason=$($_.Exception.Message)";LogFailure;shutdown.exe /s /t 5|Out-Null}
