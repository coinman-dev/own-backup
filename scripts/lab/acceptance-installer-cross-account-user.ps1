$ErrorActionPreference='Stop'
$work='C:\Lab\installer-cross-account'
try {
    $identity=[Security.Principal.WindowsIdentity]::GetCurrent()
    $elevated=[Security.Principal.WindowsPrincipal]::new($identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if(-not $elevated -or $identity.Name -notlike '*\obk-alt-admin'){throw 'wrong uninstall token'}
    $run='HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
    New-Item $run -Force|Out-Null
    New-ItemProperty $run -Name OwnBackupTray -Value '"C:\Program Files\OwnBackup\own-backup-gui.exe" --tray' -PropertyType String -Force|Out-Null
    $p=Start-Process 'C:\Program Files\OwnBackup\uninstall.exe' -ArgumentList '/S' -PassThru
    $null=$p.Handle
    if(-not $p.WaitForExit(150000)){$p.Kill();throw 'uninstaller timeout'}
    $p.Refresh()
    if($p.ExitCode -ne 0){throw "uninstaller exit=$($p.ExitCode)"}
    $deadline=[DateTime]::UtcNow.AddSeconds(30)
    while((Test-Path 'C:\Program Files\OwnBackup\own-backup-gui.exe') -and [DateTime]::UtcNow -lt $deadline){Start-Sleep -Milliseconds 500}
    $removed=-not((Get-ItemProperty $run).PSObject.Properties.Name -contains 'OwnBackupTray')
    if(-not $removed -or (Test-Path 'C:\Program Files\OwnBackup\own-backup-gui.exe')){throw 'uninstall incomplete'}
    $result=@{pass=$true;identity=$identity.Name;sid=$identity.User.Value;elevated=$elevated;startup_removed=$removed;session=(Get-Process -Id $PID).SessionId;exit=$p.ExitCode}
}catch{$result=@{pass=$false;error=$_.Exception.Message;line=$_.InvocationInfo.ScriptLineNumber}}
[IO.File]::WriteAllText("$work\alternate-result.json",(ConvertTo-Json -InputObject $result -Depth 8),[Text.UTF8Encoding]::new($false))
