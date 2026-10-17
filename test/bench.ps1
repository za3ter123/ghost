# Latency + output-size benchmark for ghost. Fixture for actions; already-open real apps read-only (tree only).
# Usage: bench.ps1 [-Reps 7]   Prints one line per command: median ms, output chars, ~tokens (chars/4).
param([int]$Reps = 7)
$ErrorActionPreference = 'Continue'
$g = Join-Path $PSScriptRoot '..\ghost.exe'
$fx = Join-Path $env:TEMP 'ghost-fixture.exe'
$csc = "$env:windir\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
& $csc /nologo /target:winexe /out:$fx (Join-Path $PSScriptRoot 'fixture.cs') | Out-Null

function Time([string]$label, [scriptblock]$sb) {
  $ms = @(); $out = ''
  for ($i = 0; $i -lt $Reps; $i++) { $sw = [Diagnostics.Stopwatch]::StartNew(); $out = (& $sb 2>&1 | Out-String); $sw.Stop(); $ms += $sw.Elapsed.TotalMilliseconds }
  $med = ($ms | Sort-Object)[[int][Math]::Floor($ms.Count / 2)]
  '{0,-28} {1,7:N1} ms {2,7} chars {3,6} tok' -f $label, $med, $out.Trim().Length, [int]($out.Trim().Length / 4)
}

$l = & $g launch $fx bench-fixture; $h = ($l -split ' ')[0]
$ref = ([regex]::Match((& $g tree $h | Out-String), '(?m)^\s*(e\d+) (Document|Edit|doc|edit)\b')).Groups[1].Value
$btn = ([regex]::Match((& $g tree $h --find Settings | Out-String), '(?m)^\s*(e\d+) ')).Groups[1].Value
Time 'windows' { & $g windows }
Time 'tree fixture' { & $g tree $h }
Time 'tree --find' { & $g tree $h --find Settings }
Time 'type' { & $g type $h $ref hello }
Time 'read' { & $g read $h $ref }
Time 'click' { & $g click $h $btn }
Time 'key' { & $g key $h a }
Time 'shot 0.5' { & $g shot $h "$env:TEMP\ghost-bench.png" --scale 0.5 }
& $g close $h | Out-Null

# Real apps, read-only: biggest visible windows of a few common processes.
foreach ($p in 'opera', 'chrome', 'explorer', 'Code', 'WindowsTerminal') {
  # explorer row is pinned to the desktop shell (Program Manager) so a stray Explorer window can't change the measured size.
  $pat = if ($p -eq 'explorer') { '^\d+ (\d+ )?explorer "Program Manager"' } else { "^\d+ (\d+ )?$p " }
  $line = (& $g windows | Select-String -Pattern $pat | Select-Object -First 1)
  if ($line) { $hw = ($line.Line -split ' ')[0]; Time "tree $p" { & $g tree $hw } }
}
