. 'C:\Lab\acceptance-common.ps1'
try {
    $id=(Get-Content "$work\bitlocker-job-id.txt" -Raw -Encoding UTF8).Trim()
    $job=Rpc 'consolidate-job-before' @{op='get';id=$id}
    Check ($job.name -eq 'stand-mixed') 'the encrypted fixture job is selected by identity'
    $null=Rpc 'consolidate-disable-schedules' @{op='save';id=$id;patch=@{schedule=@{kind='none'};verification=@{schedule=@{kind='none'}}}}
    $before=Rpc 'consolidate-versions-before' @{op='versions';id=$id}
    Check (@($before.chains).Count -eq 1) 'consolidation starts with one encrypted chain'
    $a=Rpc 'consolidate-extra-increment-1' @{op='run';id=$id}
    $b=Rpc 'consolidate-extra-increment-2' @{op='run';id=$id}
    Check ($a.status -eq 'success' -and $b.status -eq 'success' -and $a.backup_kind -eq 'incremental' -and $b.backup_kind -eq 'incremental') 'two more BitLocker-source encrypted increments are readable by the service'
    $all=Rpc 'consolidate-versions-four' @{op='versions';id=$id}
    $images=@($all.chains[0].images)
    Check ($images.Count -eq 4) 'full and three increments are present before consolidation'
    $basePath=$images[0].image_path;$baseHash=(Get-FileHash -LiteralPath $basePath).Hash
    $folder='R:\stand\mixed'
    $out="$work\consolidation-stdout.txt";$err="$work\consolidation-stderr.txt"
    $proc=Start-Process -FilePath 'C:\Lab\own-backup-cli.exe' -ArgumentList @('consolidate',$folder,'--job',$id,'--keep','1') -RedirectStandardInput "$work\password.txt" -RedirectStandardOutput $out -RedirectStandardError $err -NoNewWindow -PassThru
    $null=$proc.Handle
    Check ($proc.WaitForExit(300000)) 'encrypted CLI consolidation finished within five minutes'
    $proc.Refresh()
    if($proc.ExitCode -ne 0){Report 'consolidation-failed-output' @{exit=$proc.ExitCode;stdout=@(Get-Content $out -Tail 25 -Encoding UTF8);stderr=@(Get-Content $err -Tail 25 -Encoding UTF8)};throw "consolidate returned $($proc.ExitCode)"}
    $after=Rpc 'consolidate-versions-after' @{op='versions';id=$id}
    $remaining=@($after.chains[0].images)
    Check ($remaining.Count -eq 2 -and $remaining[0].image_path -eq $basePath -and $remaining[1].image_path -eq $b.image_path) 'the surviving encrypted chain is base plus latest increment'
    Check ((Get-FileHash -LiteralPath $basePath).Hash -ne $baseHash) 'the encrypted base was actually rewritten by consolidation'
    Check (-not(Test-Path $images[1].image_path) -and -not(Test-Path $images[2].image_path)) 'folded encrypted increment files were removed last'
    Check (-not @(Get-ChildItem $folder -Filter '*.consolidating.json').Count) 'consolidation has no unfinished marker'
    $summary=Rpc 'consolidate-verify-after' @{op='verify';image=$b.image_path;secret='recovery';recovery_file="$work\stand-mixed-recovery.txt"}
    Check ($summary.valid -and $summary.blocks_checked -gt 0) 'the newest encrypted restore point verifies through the folded base'
    $text=[IO.File]::ReadAllText($out,[Text.Encoding]::UTF8)+[IO.File]::ReadAllText($err,[Text.Encoding]::UTF8)
    Check (-not $text.Contains((Get-Content "$work\password.txt" -Raw -Encoding UTF8).Trim())) 'CLI output did not echo the archive password'
    Report 'consolidation-result' @{status='pass';exit=$proc.ExitCode;before_count=$images.Count;after_count=$remaining.Count;blocks_checked=$summary.blocks_checked;base_rewritten=$true;old_files_removed=$true;password_echoed=$false}
    Say 'ACCEPT-SUITE-PASS';shutdown.exe /s /t 5|Out-Null
}catch{Say "ACCEPT-FAIL consolidation-user line=$($_.InvocationInfo.ScriptLineNumber) message=$($_.Exception.Message)";LogFailure;shutdown.exe /s /t 5|Out-Null}
