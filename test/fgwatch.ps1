# Dot-source from a test: `$e = Start-FgWatch; if ($e) { <fail with $e> }` before the first ghost call, and
# `@(Stop-FgWatch)` at the end: a summary line, then one line per failure. The watcher (fgwatch.cs, its own
# process) logs every foreground change with its WinEvent time. Away time counts as:
# - the user had a window in front: any other window;
# - the user had none (0, the taskbar, or ghost's sink): any window but 0, the sink or that first one. Keys must
#   keep going nowhere; another window holding the foreground then (the owner's included) fails too.
# Fails on any single stretch over 50 ms, or more than 150 ms away in total.
$script:fgwDir = Join-Path $env:TEMP ('ghost-fgwatch-' + $PID)
function Start-FgWatch {
  $exe = Join-Path $env:TEMP 'ghost-fgwatch.exe'
  $csc = "$env:windir\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
  & $csc /nologo /target:winexe /out:$exe (Join-Path $PSScriptRoot 'fgwatch.cs') | Out-Null
  New-Item -ItemType Directory -Force $script:fgwDir | Out-Null
  $script:fgwLog = Join-Path $script:fgwDir 'fg.log'; $script:fgwStop = Join-Path $script:fgwDir 'stop'
  Remove-Item $script:fgwLog, "$($script:fgwLog).ready", $script:fgwStop -ErrorAction SilentlyContinue
  $script:fgwProc = Start-Process $exe -ArgumentList "`"$($script:fgwLog)`" `"$($script:fgwStop)`" $PID" -WindowStyle Hidden -PassThru
  $sw = [Diagnostics.Stopwatch]::StartNew()
  while ($sw.ElapsedMilliseconds -lt 2000 -and -not (Test-Path "$($script:fgwLog).ready")) { Start-Sleep -Milliseconds 20 }
  if (-not (Test-Path "$($script:fgwLog).ready")) { return 'foreground watcher was not ready within 2 s' }
}
function Stop-FgWatch {
  New-Item -ItemType File $script:fgwStop -Force | Out-Null
  $script:fgwProc.WaitForExit(3000) | Out-Null
  Read-FgLog $script:fgwLog
}
function Read-FgLog($log) {
  $raw = @(Get-Content $log -ErrorAction SilentlyContinue)
  if (-not $raw.Count -or $raw[-1] -notmatch ' END \d+$') { return @('fg watch: no log', 'foreground watcher log is missing or incomplete') }
  # Times on one clock: an event's own time (its WinEvent timestamp, 16 ms steps) and a poll's tick. Ties keep
  # the order they were logged in (a stable sort on (time, line)).
  $i = 0
  $rows = @($raw | ForEach-Object { $p = $_ -split ' '; if ($p[1] -eq 'END') { [pscustomobject]@{ T = [long]$p[2]; I = $i++; H = 'END' } } else { [pscustomobject]@{ T = [long]$p[8]; I = $i++; H = $p[2]; C = $p[3]; Off = $p[4] -eq 'P'; New = $p[6] -eq 'N'; Ghost = $p[7] -eq 'L' } } } | Sort-Object T, I)
  # Back-to-back events for one window are one stretch.
  $one = New-Object System.Collections.Generic.List[object]
  foreach ($r in $rows) { if ($one.Count -eq 0 -or $one[$one.Count - 1].H -ne $r.H) { $one.Add($r) } }
  $rows = $one.ToArray()
  if ($rows.Count -lt 2 -or $rows[-1].H -ne 'END') { return @('fg watch: unreadable log', "foreground watcher log could not be read ($($raw.Count) lines)") }
  $first = $rows[0]
  $none = $first.H -eq '0' -or $first.C -eq 'Shell_TrayWnd' -or $first.C -eq 'ghost-sink'
  $bad = @(); $max = 0; $total = 0
  for ($i = 0; $i -lt $rows.Count - 1; $i++) {
    $r = $rows[$i]; $d = $rows[$i + 1].T - $r.T
    $away = $r.H -ne $first.H -and -not ($none -and ($r.H -eq '0' -or $r.C -eq 'ghost-sink'))
    if (-not $away) { continue }
    $total += $d; if ($d -gt $max) { $max = $d }
    if ($d -gt 50) { $bad += "foreground was $($r.H) ($($r.C)) for $([int]$d) ms" }
  }
  $max = [int]$max; $total = [int]$total
  if ($total -gt 150) { $bad += "foreground was away $total ms in total, budget 150" }
  "fg watch: $($rows.Count - 1) changes, user had $(if ($none) { 'no window' } else { 'a window' }), longest away $max ms, total away $total ms"
  $bad
}
