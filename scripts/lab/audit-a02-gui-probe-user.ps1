# Ordinary lab desktop: inspect the GUI's accessible controls before clicking.
function Say([string]$Text) {
    try {
        $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $p.Open(); $p.WriteLine($Text); $p.Close()
    } catch { }
}
$ErrorActionPreference = 'Stop'
try {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $admin = [Security.Principal.WindowsPrincipal]::new($identity).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
    Say "A02-GUI-USER name=$($identity.Name) session=$([Diagnostics.Process]::GetCurrentProcess().SessionId) admin=$admin"
    if ($admin) { throw 'lab user is an administrator' }
    $marker = Join-Path ([IO.Path]::GetTempPath()) 'own-backup-gui-up'
    Remove-Item -LiteralPath $marker -Force -ErrorAction SilentlyContinue
    $gui = Start-Process -FilePath 'C:\Lab\own-backup-gui.exe' -PassThru
    for ($tick = 0; $tick -lt 60; $tick++) {
        $gui.Refresh()
        if ($gui.HasExited -or (Test-Path -LiteralPath $marker)) { break }
        Start-Sleep -Seconds 2
    }
    $gui.Refresh()
    Say "A02-GUI-WINDOW pid=$($gui.Id) exited=$($gui.HasExited) title=$($gui.MainWindowTitle) rendered=$(Test-Path -LiteralPath $marker)"
    if ($gui.HasExited -or -not (Test-Path -LiteralPath $marker)) { throw 'GUI did not render' }
    Add-Type -AssemblyName UIAutomationClient
    Add-Type -AssemblyName UIAutomationTypes
    $root = [System.Windows.Automation.AutomationElement]::RootElement
    $condition = [System.Windows.Automation.PropertyCondition]::new(
        [System.Windows.Automation.AutomationElement]::ProcessIdProperty, $gui.Id)
    $window = $root.FindFirst([System.Windows.Automation.TreeScope]::Children, $condition)
    if (-not $window) { throw 'GUI window missing from UI Automation' }
    $nodes = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants,
        [System.Windows.Automation.Condition]::TrueCondition)
    $details = New-Object System.Collections.ArrayList
    for ($i = 0; $i -lt $nodes.Count; $i++) {
        $node = $nodes.Item($i)
        try {
            $name = [string]$node.Current.Name
            $kind = [string]$node.Current.ControlType.ProgrammaticName
            $id = [string]$node.Current.AutomationId
            $rect = $node.Current.BoundingRectangle
            if ($name -or $id -or $kind -match 'Button|CheckBox|TabItem') {
                $null = $details.Add([pscustomobject]@{
                    kind = $kind; name = $name; id = $id
                    x = [int]$rect.X; y = [int]$rect.Y
                    width = [int]$rect.Width; height = [int]$rect.Height
                })
            }
        } catch { }
    }
    $json = $details | ConvertTo-Json -Depth 4
    [IO.File]::WriteAllText('C:\Lab\a02-gui-tree.json', $json, ([Text.UTF8Encoding]::new($false)))
    Say "A02-GUI-TREE nodes=$($nodes.Count) named=$($details.Count)"
    Say "A02-GUI-TREE-BASE64 $([Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($json)))"
    Say 'A02-GUI-READY screenshot-and-inspect'
    Start-Sleep -Seconds 240
} catch {
    Say "A02-GUI-FAIL line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)"
} finally {
    Say 'A02-GUI-DONE'
    shutdown.exe /s /t 5 | Out-Null
}
