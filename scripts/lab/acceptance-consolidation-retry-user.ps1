. 'C:\Lab\acceptance-common.ps1'
try {
    $ctx=Context;Report 'consolidation-user-context' $ctx
    Check (-not $ctx.elevated) 'encrypted consolidation runs under an ordinary desktop token'
    $id=(Get-Content "$work\bitlocker-job-id.txt" -Raw -Encoding UTF8).Trim()
    $folder='R:\stand\mixed'
    $before=Rpc 'consolidation-retry-before' @{op='versions';id=$id}
    $images=@($before.chains[0].images)
    Check ($images.Count -eq 4) 'full and three encrypted increments remain intact after UAC refusal'
    $base=$images[0].image_path;$hash=(Get-FileHash -LiteralPath $base).Hash;$latest=$images[-1].image_path
    $stdout="$work\consolidation-retry-out.txt";$stderr="$work\consolidation-retry-err.txt"
    $p=Start-Process -FilePath 'C:\Lab\own-backup-cli.exe' -ArgumentList @('consolidate',$folder,'--job',$id,'--keep','1') -RedirectStandardInput "$work\password.txt" -RedirectStandardOutput $stdout -RedirectStandardError $stderr -NoNewWindow -PassThru
    $null=$p.Handle
    Check ($p.WaitForExit(300000)) 'encrypted CLI operation finished within five minutes'
    $p.Refresh()
    $out=[IO.File]::ReadAllText($stdout,[Text.Encoding]::UTF8);$err=[IO.File]::ReadAllText($stderr,[Text.Encoding]::UTF8)
    Report 'consolidation-cli-result' @{exit=$p.ExitCode;stdout=$out;stderr=$err}
    Check ($p.ExitCode -eq 0 -and -not $out.Contains('UAC')) 'OBK consolidation did not request administrator rights'
    $after=Rpc 'consolidation-retry-after' @{op='versions';id=$id}
    $surviving=@($after.chains[0].images)
    Check ($surviving.Count -eq 2 -and $surviving[0].image_path -eq $base -and $surviving[1].image_path -eq $latest) 'the encrypted chain now has a base and its newest increment'
    Check ((Get-FileHash -LiteralPath $base).Hash -ne $hash) 'the encrypted base changed during the actual merge'
    Check (-not(Test-Path $images[1].image_path) -and -not(Test-Path $images[2].image_path)) 'two folded increments were removed after the merge'
    Check (@(Get-ChildItem $folder -Filter '*.consolidating.json').Count -eq 0) 'no transaction marker remains'
    $valid=Rpc 'consolidation-retry-verify' @{op='verify';image=$latest;secret='recovery';recovery_file="$work\stand-mixed-recovery.txt"}
    Check ($valid.valid -and $valid.blocks_checked -gt 0) 'the latest restore point verifies through the folded encrypted base'
    Check (-not ($out+$err).Contains((Get-Content "$work\password.txt" -Raw -Encoding UTF8).Trim())) 'CLI output contains no archive password'
    Report 'consolidation-accepted' @{pass=$true;before_versions=$images.Count;after_versions=$surviving.Count;blocks_checked=$valid.blocks_checked;old_versions_removed=$true;archive_password_not_logged=$true}
    Say 'ACCEPT-SUITE-PASS';shutdown.exe /s /t 5|Out-Null
}catch{Say "ACCEPT-FAIL consolidation-retry-user line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)";LogFailure;shutdown.exe /s /t 5|Out-Null}
