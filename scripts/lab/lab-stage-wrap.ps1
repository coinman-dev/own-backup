# ASCII only, on purpose: this file has to parse even when the one it calls
# does not.
#
# A PowerShell parse error is SILENT. Nothing runs, nothing reaches the serial
# port, and the phase is indistinguishable from a hung machine -- which is how
# an hour went into a script whose only fault was being saved as UTF-8 without
# a byte-order mark: PowerShell 5.1 then reads it in the ANSI codepage, and the
# Cyrillic inside its string literals stops being string literals.
#
# So: no Cyrillic here, the callee is parsed before it is run, and the parse
# errors are read out by line number. Whatever happens, the machine powers off.

function SayW {
    param([string]$Text)
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open(); $port.WriteLine($Text); $port.Close()
    } catch { }
}

$script = 'C:\Lab\lab-restore-obk.ps1'
SayW "LAB-WRAP-BEGIN $script"
try {
    if (-not (Test-Path -LiteralPath $script)) { throw "script not found: $script" }
    $errors = $null
    $null = [System.Management.Automation.Language.Parser]::ParseFile(
        $script, [ref]$null, [ref]$errors)
    if ($errors) {
        foreach ($e in $errors) {
            SayW ("LAB-WRAP-PARSE line {0}: {1}" -f $e.Extent.StartLineNumber, $e.Message)
        }
        throw 'the script does not parse -- check its encoding (UTF-8 needs a BOM)'
    }
    & $script
} catch {
    SayW ("LAB-WRAP-FAIL " + $_.Exception.Message)
}
SayW 'LAB-WRAP-END'
Start-Sleep -Seconds 3
shutdown /s /t 5
