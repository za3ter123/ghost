# Token gate: the cheapest command sequence an agent needs for a small fixture task, counted end to end.
# Task: open the fixture, type "hi" into Body, press Settings, confirm the label says clicked, close.
# Counts every char the agent writes (commands, as "ghost ...") plus every char it reads back.
# v1 needed launch + tree + type + click + tree --find (about 620 chars). Budget for v2: 220.
$ErrorActionPreference = 'Continue'
$g = Join-Path $PSScriptRoot '..\ghost.exe'
$fx = Join-Path $env:TEMP 'ghost-fixture.exe'
$csc = "$env:windir\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
& $csc /nologo /target:winexe /out:$fx (Join-Path $PSScriptRoot 'fixture.cs') | Out-Null
$fails = New-Object System.Collections.Generic.List[string]
$total = 0
. (Join-Path $PSScriptRoot 'fgwatch.ps1')
$fgwErr = Start-FgWatch; if ($fgwErr) { $fails.Add($fgwErr) }
function Step([string]$shown, [string[]]$argv) {
  $o = (& $g @argv 2>&1 | Out-String).Trim(); $code = $LASTEXITCODE
  $script:total += ('ghost ' + $shown).Length + $o.Length
  "  ghost $shown`n    -> [$code] $($o -replace "`r?`n", ' | ')"
  [pscustomobject]@{ Code = $code; Out = $o }
}
# The agent writes the fixture as a short name; the real path is passed but counted as 'fx'.
$r = Step 'launch fx tok' @('launch', $fx, 'tok')
if ($r.Code -ne 0) { $fails.Add('launch failed') }
$r = Step 'run "type Body hi" "click Settings"' @('run', 'type Body hi', 'click Settings')
if ($r.Code -ne 0) { $fails.Add("run failed: $($r.Out)") }
elseif ($r.Out -notmatch 'clicked') { $fails.Add('run output does not show the label changed to clicked') }
$v = (& $g read Body 2>&1 | Out-String)
if ($v -notmatch '\bhi\b') { $fails.Add("Body does not contain hi (got: $v)") }
$r = Step 'close' @('close')
if ($r.Code -ne 0) { $fails.Add("close (current window) failed: $($r.Out)") }
"task chars: $total (~$([int]($total / 4)) tokens), budget 220"
if ($total -gt 220) { $fails.Add("task cost $total chars > 220") }
$fgw = @(Stop-FgWatch); $fgw[0]; $fgw | Select-Object -Skip 1 | ForEach-Object { $fails.Add($_) }
if ($fails.Count) { $fails | ForEach-Object { "  x $_" }; 'TOKENS FAIL'; exit 1 } else { 'TOKENS PASS'; exit 0 }
