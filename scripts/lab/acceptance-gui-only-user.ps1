. 'C:\Lab\acceptance-common.ps1'
try {
    & 'C:\Lab\acceptance-gui.ps1'
    Phase 'done';Say 'ACCEPT-GUI-ONLY-PASS'
}catch{Phase 'failed';Say "ACCEPT-FAIL gui-only line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)";LogFailure}
