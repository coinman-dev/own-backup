# ASCII only: PowerShell 5.1 can parse this under any guest UI language.
$ErrorActionPreference = 'Stop'
$work = 'C:\Lab\acceptance'
function Say([string]$text) {
    $port = New-Object IO.Ports.SerialPort 'COM1',115200,'None',8,'One'
    try {
        for ($attempt=0;$attempt -lt 20;$attempt++) {
            try { $port.Open();break } catch {if ($attempt -eq 19) {throw};Start-Sleep -Milliseconds 100}
        }
        $port.WriteLine($text)
    } finally { $port.Dispose() }
}
function Check([bool]$ok,[string]$text) { if (-not $ok) { throw "check failed: $text" }; Say "ACCEPT-OK $text" }
function JsonFile([string]$path,$value) {
    $json=if($value -is [Array]){ConvertTo-Json -InputObject $value.PSObject.BaseObject -Depth 40}else{ConvertTo-Json -InputObject $value -Depth 40}
    [IO.File]::WriteAllText($path,$json,[Text.UTF8Encoding]::new($false))
}
function Report([string]$name,$value) {
    $path = Join-Path $work "$name.json"
    JsonFile $path $value
    Say "ACCEPT-REPORT $name $([Convert]::ToBase64String([IO.File]::ReadAllBytes($path)))"
}
function Rpc([string]$name,$request) {
    $path = Join-Path $work "$name.request.json"
    JsonFile $path $request
    # Keep native stderr from becoming a PowerShell terminating error before
    # the helper's sanitized JSON can be recorded.
    $stdout=Join-Path $work "$name.helper.out"
    $stderr=Join-Path $work "$name.helper.err"
    $process=Start-Process 'C:\Lab\audit_autonomous.exe' -ArgumentList ('"'+$path+'"') -RedirectStandardOutput $stdout -RedirectStandardError $stderr -NoNewWindow -PassThru
    $null=$process.Handle
    if(-not $process.WaitForExit(960000)){$process.Kill();throw "RPC helper timeout: $name"}
    $process.Refresh()
    $output=[IO.File]::ReadAllText($stdout)+[IO.File]::ReadAllText($stderr)
    if ($process.ExitCode -ne 0) {
        $errorFile = [IO.Path]::ChangeExtension($path,'result.json')
        if (Test-Path $errorFile) { Report "$name-error" (Get-Content $errorFile -Raw -Encoding UTF8 | ConvertFrom-Json) }
        throw "RPC helper failed: $name exit=$($process.ExitCode) output=$output"
    }
    $result = Get-Content ([IO.Path]::ChangeExtension($path,'result.json')) -Raw -Encoding UTF8 | ConvertFrom-Json
    Report $name $result
    if ($result.ok) { return $result.result }
    return $result
}
function Native([string]$exe,[string[]]$arguments) {
    $output = (& $exe @arguments 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) { throw "native failed exit=$LASTEXITCODE exe=$exe output=$output" }
    return $output
}
function Phase([string]$value) { [IO.File]::WriteAllText("$work\phase.txt",$value) }
function AutoLogon([string]$script) {
    $answer = New-Object Xml.XmlDocument; $answer.Load('C:\Lab\autounattend.xml')
    $password = $answer.SelectSingleNode("//*[local-name()='AutoLogon']/*[local-name()='Password']/*[local-name()='Value']").InnerText
    if (-not $password) { throw 'disposable guest logon password missing' }
    $key='HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $key DefaultUserName 'lab'; Set-ItemProperty $key DefaultDomainName $env:COMPUTERNAME
    Set-ItemProperty $key DefaultPassword $password; Set-ItemProperty $key AutoAdminLogon '1'; Set-ItemProperty $key AutoLogonCount 1
    Remove-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\RunOnce' -Name OwnBackupAcceptance -ErrorAction SilentlyContinue
    $startup=Join-Path ([Environment]::GetFolderPath('CommonStartup')) 'OwnBackupAcceptance.cmd'
    [IO.File]::WriteAllText($startup,"@echo off`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\Lab\$script`r`n",[Text.Encoding]::ASCII)
}
function NoLogon {
    $key='HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $key AutoAdminLogon '0'
    Remove-ItemProperty $key DefaultPassword -ErrorAction SilentlyContinue
}
function Context {
    $interactive=@{}
    Get-CimInstance Win32_LogonSession | Where-Object { $_.LogonType -in 2,10,11 } | ForEach-Object { $interactive[$_.LogonId]=$true }
    $humans=@(Get-CimInstance Win32_LoggedOnUser | Where-Object { $interactive.ContainsKey($_.Dependent.LogonId) } |
        ForEach-Object { $_.Antecedent.Domain+'\'+$_.Antecedent.Name } | Sort-Object -Unique | Where-Object {
            $pieces=$_.Split([char]92)
            $pieces[0] -notin @('NT AUTHORITY','Window Manager','Font Driver Host','NT SERVICE') -and
            -not $pieces[-1].StartsWith('DWM-') -and -not $pieces[-1].StartsWith('UMFD-')
        })
    return @{ elevated=[Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator); identity=[Security.Principal.WindowsIdentity]::GetCurrent().Name; humans=$humans; explorer=@(Get-Process explorer -ErrorAction SilentlyContinue).Count; boot=(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().ToString('o') }
}
function SetFutureSchedules([string]$id) {
    $null=Native 'C:\Lab\own-backup-service.exe' @('/stop')
    $path='C:\ProgramData\OwnBackup\jobs.json'
    $jobs=(Get-Content $path -Raw -Encoding UTF8 | ConvertFrom-Json).PSObject.BaseObject
    $job=@($jobs | Where-Object { $_.id -eq $id })[0]
    $job.schedule.kind='daily';$job.schedule.time=(Get-Date).AddMinutes(2).ToString('HH:mm');$job.schedule.run_missed=$false
    $job.verification.schedule.kind='daily';$job.verification.schedule.time=(Get-Date).AddMinutes(3).ToString('HH:mm');$job.verification.schedule.run_missed=$false
    JsonFile $path $jobs
    Report 'future-schedules' @{id=$id;backup_time=$job.schedule.time;verification_time=$job.verification.schedule.time;staged_while_service_stopped=$true}
}
function LogFailure {
    foreach ($dir in @('C:\Lab\Logs','C:\ProgramData\OwnBackup\Logs')) {
        $log=Get-ChildItem $dir -Filter 'own-backup-service-*.log' -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1
        if ($log) { Report 'service-tail' @([IO.File]::ReadAllLines($log.FullName,[Text.Encoding]::UTF8) | Select-Object -Last 30) }
    }
}
