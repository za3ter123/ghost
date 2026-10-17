# Head-to-head on the same pages: ghost (UIA on a hidden Edge, throwaway profile) vs agent-browser (its own Chromium).
# Counts chars the agent writes + reads, and wall time, for (a) one page read and (b) a small form task.
param([int]$Reps = 3)
$ErrorActionPreference = 'Continue'
$g = Join-Path $PSScriptRoot '..\ghost.exe'
$edge = @("${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe", "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
$form = ([Uri](Join-Path $PSScriptRoot 'web.html')).AbsoluteUri
$pages = [ordered]@{ 'form fixture' = $form; 'example.com' = 'https://example.com'; 'wikipedia' = 'https://en.wikipedia.org/wiki/Microsoft_Windows' }
$prof = Join-Path $env:TEMP 'ghost-compare-profile'

function Run($cmd, [string[]]$argv) { $sw = [Diagnostics.Stopwatch]::StartNew(); $o = (& $cmd @argv 2>&1 | Out-String).Trim(); $sw.Stop(); [pscustomobject]@{ Out = $o; Ms = $sw.ElapsedMilliseconds; Code = $LASTEXITCODE } }
function Med($xs) { ($xs | Sort-Object)[[int][Math]::Floor($xs.Count / 2)] }

"## Page read (median of $Reps): chars returned, ms"
'{0,-14} {1,22} {2,22} {3,22}' -f 'page', 'ghost tree', 'agent-browser snap -i', 'agent-browser snap'
foreach ($k in $pages.Keys) {
  $u = $pages[$k]
  $l = & $g launch $edge "--user-data-dir=$prof" --no-first-run --no-default-browser-check $u 2>&1 | Out-String
  $h = ($l.Trim() -split ' ')[0]
  & $g wait $h ($(if ($k -eq 'form fixture') { 'Greet' } elseif ($k -eq 'example.com') { 'Learn more' } else { 'Windows' })) --timeout 15000 | Out-Null
  Start-Sleep -Milliseconds 1500
  $gr = 1..$Reps | ForEach-Object { Run $g @('tree', $h) }
  & $g close $h | Out-Null
  agent-browser open $u 2>&1 | Out-Null
  $ai = 1..$Reps | ForEach-Object { Run 'agent-browser' @('snapshot', '-i') }
  $af = 1..$Reps | ForEach-Object { Run 'agent-browser' @('snapshot') }
  '{0,-14} {1,12} ch {2,5} ms {3,12} ch {4,5} ms {5,12} ch {6,5} ms' -f $k, $gr[-1].Out.Length, (Med ($gr | ForEach-Object Ms)), $ai[-1].Out.Length, (Med ($ai | ForEach-Object Ms)), $af[-1].Out.Length, (Med ($af | ForEach-Object Ms))
}
agent-browser close 2>&1 | Out-Null

"`n## Form task: type 'bob' into Name, press Greet, confirm 'hello bob'"
# ghost: act by name, read the diff. Commands are counted as written by an agent (exe path shown as 'ghost', url as 'URL').
$l = & $g launch $edge "--user-data-dir=$prof" --no-first-run $form 2>&1 | Out-String; $h = ($l.Trim() -split ' ')[0]
& $g wait $h Greet --timeout 15000 | Out-Null
$steps = @(@('ghost launch msedge URL', $l.Trim()))
$r = Run $g @('run', $h, 'type Name bob', 'click Greet'); $steps += , @('ghost run "type Name bob" "click Greet"', $r.Out)
$ok = $r.Out -match 'hello bob'
& $g close $h | Out-Null
$gc = ($steps | ForEach-Object { $_[0].Length + $_[1].Length } | Measure-Object -Sum).Sum
"ghost:         $gc chars, confirmed=$ok"
# agent-browser: open, snapshot -i to get refs, fill, click, snapshot -i to confirm (its documented loop).
$a = @(); $o = (agent-browser open $form 2>&1 | Out-String).Trim(); $a += , @('agent-browser open URL', $o)
$s = (agent-browser snapshot -i 2>&1 | Out-String).Trim(); $a += , @('agent-browser snapshot -i', $s)
$nref = [regex]::Match($s, '(?m)textbox "Name".*?\[ref=(e\d+)\]').Groups[1].Value
$bref = [regex]::Match($s, '(?m)button "Greet".*?\[ref=(e\d+)\]').Groups[1].Value
$o = (agent-browser fill "@$nref" bob 2>&1 | Out-String).Trim(); $a += , @("agent-browser fill @$nref bob", $o)
$o = (agent-browser click "@$bref" 2>&1 | Out-String).Trim(); $a += , @("agent-browser click @$bref", $o)
$s2 = (agent-browser snapshot 2>&1 | Out-String).Trim(); $a += , @('agent-browser snapshot', $s2)
agent-browser close 2>&1 | Out-Null
$ac = ($a | ForEach-Object { $_[0].Length + $_[1].Length } | Measure-Object -Sum).Sum
"agent-browser: $ac chars, refs=$nref/$bref, confirmed=$($s2 -match 'hello bob')"
