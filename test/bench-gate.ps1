# Speed + size gate for ghost v2. Runs bench.ps1 (warm daemon) and asserts medians and output sizes.
$ErrorActionPreference = 'Continue'
$g = Join-Path $PSScriptRoot '..\ghost.exe'
& $g windows | Out-Null   # warm the daemon so the gate measures steady state
# Latency under someone else's load says nothing about ghost: total CPU is sampled before (2 s) and during the run,
# and above a 50% mean the gate refuses to judge (exit 1, never a pass).
$cpu = { (Get-CimInstance Win32_Processor | Measure-Object LoadPercentage -Average).Average }
# First wait (15 min at most) for 10 consecutive seconds under 25% total CPU, one sample a second.
$qw = [Diagnostics.Stopwatch]::StartNew(); $quiet = 0
while ($quiet -lt 10 -and $qw.Elapsed.TotalMinutes -lt 15) {
  $t = $qw.ElapsedMilliseconds
  if ((& $cpu) -lt 25) { $quiet++ } else { $quiet = 0 }
  $rest = 1000 - ($qw.ElapsedMilliseconds - $t); if ($rest -gt 0) { Start-Sleep -Milliseconds $rest }
}
"cpu: $(if ($quiet -ge 10) { 'quiet' } else { 'no quiet' }) 10 s window $(if ($quiet -ge 10) { 'after' } else { 'in' }) $([int]$qw.Elapsed.TotalSeconds) s"
$pre = @(1..4 | ForEach-Object { & $cpu; Start-Sleep -Milliseconds 500 })
$stopf = Join-Path $env:TEMP ('ghost-bench-cpu-' + $PID)
$job = Start-Job -ArgumentList $stopf -ScriptBlock { param($f) while (-not (Test-Path $f)) { (Get-CimInstance Win32_Processor | Measure-Object LoadPercentage -Average).Average; Start-Sleep -Milliseconds 500 } }
$lines = & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'bench.ps1') -Reps 9 2>&1 | Out-String
New-Item -ItemType File $stopf -Force | Out-Null
$during = @(Receive-Job $job -Wait -AutoRemoveJob)
Remove-Item $stopf -ErrorAction SilentlyContinue
$lines
$all = @($pre + $during | Where-Object { $null -ne $_ })
$load = [int]($all | Measure-Object -Average).Average
"cpu: mean $load% over $($all.Count) samples (before: $($pre -join ' '); during: $($during -join ' '))"
if ($load -gt 50) { "BENCH SKIP: load $load%"; exit 1 }
# PowerShell's own cost to call a native exe here is ~7 ms (measured 2026-10-17); limits include it.
$limits = [ordered]@{ 'windows' = 12; 'tree fixture' = 15; 'tree --find' = 12; 'type' = 60; 'read' = 12; 'click' = 60; 'key' = 60; 'shot 0.5' = 40 }
$fails = @()
foreach ($k in $limits.Keys) {
  $m = [regex]::Match($lines, '(?m)^' + [regex]::Escape($k) + '\s+([\d,\.]+) ms')
  if (-not $m.Success) { $fails += "$k not measured"; continue }
  $v = [double]($m.Groups[1].Value -replace ',', '')
  if ($v -gt $limits[$k]) { $fails += "$k $v ms > $($limits[$k]) ms" }
}
# Baseline 2026-10-17 (v1): explorer tree 3251 chars, fixture tree 302 chars. v2 budget: explorer 1200, fixture 150.
$e = [regex]::Match($lines, '(?m)^tree explorer\s+[\d,\.]+ ms\s+(\d+) chars')
if ($e.Success -and [int]$e.Groups[1].Value -gt 1200) { $fails += "explorer tree $($e.Groups[1].Value) chars > 1200" }
$f = [regex]::Match($lines, '(?m)^tree fixture\s+[\d,\.]+ ms\s+(\d+) chars')
if ($f.Success -and [int]$f.Groups[1].Value -gt 150) { $fails += "fixture tree $($f.Groups[1].Value) chars > 150" }
if ($fails.Count) { $fails | ForEach-Object { "  x $_" }; 'BENCH FAIL'; exit 1 } else { 'BENCH PASS'; exit 0 }
