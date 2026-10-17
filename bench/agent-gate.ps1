# Agent-level gate: Haiku runs every task with ghost, then with the reference tool for that task
# (agent-browser on the web tasks, no tool on the app and Windows tasks), and compares them.
# PASS when ghost is correct on every rep of every task and its median total tokens per task are
# below the reference tool's.   agent-gate.ps1 [-Reps 3] [-Tasks W1,W2,A1,N1,N2]
param([int]$Reps = 3, [string[]]$Tasks = @('W1', 'W2', 'A1', 'N1', 'N2'))
$ErrorActionPreference = 'Continue'
$Tasks = @($Tasks | ForEach-Object { $_ -split ',' })
$run = Join-Path $PSScriptRoot 'run.ps1'
$file = ([Uri](Resolve-Path (Join-Path $PSScriptRoot '..\test\web.html')).Path).AbsoluteUri   # no http server needed for ghost or agent-browser
$tag = 'gate-' + (Get-Date -Format 'yyyyMMdd-HHmmss')
foreach ($t in $Tasks) {
  $ref = if ($t -like 'W*') { 'agent-browser' } else { 'none' }
  & $run -Tools "ghost,$ref" -Tasks $t -Reps $Reps -Web $file -Tag $tag | Out-Host
}
$rows = Get-Content (Join-Path $PSScriptRoot 'results.jsonl') | ForEach-Object { try { $_ | ConvertFrom-Json } catch {} } | Where-Object { $_.tag -eq $tag }
function Med($xs) { $s = @($xs | Sort-Object); if ($s.Count) { $s[[int][Math]::Floor($s.Count / 2)] } else { $null } }
$fail = 0
foreach ($t in $Tasks) {
  $ref = if ($t -like 'W*') { 'agent-browser' } else { 'none' }
  $g = @($rows | Where-Object { $_.task -eq $t -and $_.tool -eq 'ghost' }); $r = @($rows | Where-Object { $_.task -eq $t -and $_.tool -eq $ref })
  $gok = @($g | Where-Object ok).Count; $gm = Med ($g | ForEach-Object total); $rm = Med ($r | ForEach-Object total)
  $gt = Med ($g | ForEach-Object tool_tokens)   # what ghost itself added to context: commands + outputs
  $pass = ($g.Count -eq $Reps) -and ($gok -eq $Reps) -and ($null -ne $gm) -and ($null -ne $rm) -and ($gm -lt $rm) -and ($gt -le 1000)
  if (-not $pass) { $fail++ }
  '{0}  ghost {1}/{2} ok, median {3} (tool {9})   {4} {5}/{6} ok, median {7} (tool {10})   {8}' -f $t, $gok, $g.Count, $gm, $ref, @($r | Where-Object ok).Count, $r.Count, $rm, $(if ($pass) { 'ok' } else { 'FAIL' }), $gt, (Med ($r | ForEach-Object tool_tokens))
}
if ($fail -eq 0) { 'AGENT PASS'; exit 0 } else { "AGENT FAIL ($fail of $($Tasks.Count) tasks)"; exit 1 }
