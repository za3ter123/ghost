# Dot-source from a test: `$e = Start-FgWatch; if ($e) { <fail with $e> }` before the first ghost call, and
# `@(Stop-FgWatch)` at the end: a summary line, then one line per failure. The watcher (fgwatch.cs, its own
# process) logs every foreground change seen by a 1 ms GetForegroundWindow poll (P lines; durations come only from
# these) and by a WinEvent hook (E lines, a cross-check whose hand-back times run ~100 ms late), with the owner:
# process, title, and G (ghost started
# it: ghost.exe, the fixture, a browser on a ghost throwaway profile, a child of those, or a launched.txt window)
# or F (foreign). Away time counts only stretches held by G windows other than ghost's sink; the owner switching
# apps, or another session's console popping up, is not ghost taking the foreground. Every stretch over 50 ms by a
# window other than the starting one is listed with its owner in the summary line.
# Fails on any single stretch over 50 ms (the only stretch a user could see as a flash), or more than 300 ms away
# in total across the whole run. Each foreground steal the guard reverses costs a few to a few tens of ms; a session that navigates several times (every `go` opens a tab in the running
# browser, which activates once) accrues a few of those, so the cumulative budget allows for them while the
# per-stretch cap keeps any single one invisible.
$script:fgwDir = Join-Path $env:TEMP ('ghost-fgwatch-' + $PID)
function Start-FgWatch {
  $exe = Join-Path $env:TEMP 'ghost-fgwatch.exe'
  $csc = "$env:windir\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
  $src = Join-Path $PSScriptRoot 'fgwatch.cs'
  if (-not (Test-Path $exe) -or (Get-Item $exe).LastWriteTime -lt (Get-Item $src).LastWriteTime) { & $csc /nologo /target:winexe /r:System.Management.dll /out:$exe $src | Out-Null }
  New-Item -ItemType Directory -Force $script:fgwDir | Out-Null
  $script:fgwLog = Join-Path $script:fgwDir 'fg.log'; $script:fgwStop = Join-Path $script:fgwDir 'stop'
  Remove-Item $script:fgwLog, "$($script:fgwLog).ready", $script:fgwStop -ErrorAction SilentlyContinue
  $script:fgwProc = Start-Process $exe -ArgumentList "`"$($script:fgwLog)`" `"$($script:fgwStop)`" $PID" -WindowStyle Hidden -PassThru
  $sw = [Diagnostics.Stopwatch]::StartNew()
  while ($sw.ElapsedMilliseconds -lt 2000 -and -not (Test-Path "$($script:fgwLog).ready")) { Start-Sleep -Milliseconds 20 }
  if (-not (Test-Path "$($script:fgwLog).ready")) { return 'foreground watcher was not ready within 2 s' }
}
# True when the window in front right now is ghost's (and not its sink): in launched.txt by hwnd or pid, or its
# process is ghost.exe, the fixture, or a browser on a ghost throwaway profile. Same rule as the watcher.
$script:fgwOwn = @{}
function Test-GhostForeground {
  if (-not ('FGW' -as [type])) { Add-Type 'using System; using System.Runtime.InteropServices; using System.Text; public static class FGW { [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow(); [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint p); [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n); }' }
  $f = [FGW]::GetForegroundWindow(); if ($f -eq [IntPtr]::Zero) { return $false }
  $b = New-Object System.Text.StringBuilder 64; [FGW]::GetClassName($f, $b, 64) | Out-Null; if ($b.ToString() -eq 'ghost-sink') { return $false }
  [uint32]$fp = 0; [FGW]::GetWindowThreadProcessId($f, [ref]$fp) | Out-Null
  $lt = Join-Path $env:LOCALAPPDATA 'ghost\launched.txt'
  if (-not $script:fgwOwn.ContainsKey($fp)) {
    $wp = Get-CimInstance Win32_Process -Filter "ProcessId=$fp" -ErrorAction SilentlyContinue
    $n = "$($wp.Name)".ToLower()
    $script:fgwOwn[$fp] = @{ N = $n; G = ($n -eq 'ghost.exe') -or $n.StartsWith('ghost-fixture') -or ("$($wp.CommandLine)" -like '*\ghost\profiles\ghost-*') }
  }
  $o = $script:fgwOwn[$fp]
  # By hwnd always; by pid only outside Explorer, whose one process also hosts the owner's own Explorer windows.
  if (Test-Path $lt) { foreach ($l in Get-Content $lt) { $q = $l -split ' '; if ($q[0] -eq "$f" -or ($q[1] -eq "$fp" -and $o.N -ne 'explorer.exe')) { return $true } } }
  $o.G
}
function Stop-FgWatch {
  New-Item -ItemType File $script:fgwStop -Force | Out-Null
  $script:fgwProc.WaitForExit(3000) | Out-Null
  Read-FgLog $script:fgwLog
}
function Read-FgLog($log, [switch]$planted) {
  $raw = @(Get-Content $log -ErrorAction SilentlyContinue)
  if (-not $raw.Count -or $raw[-1] -notmatch ' END \d+$') { return @('fg watch: no log', 'foreground watcher log is missing or incomplete') }
  # Durations from the poll alone, on its high-resolution clock (the first field); hook lines are only counted.
  $i = 0; $hook = @($raw | Where-Object { ($_ -split ' ', 3)[1] -eq 'E' }).Count
  # Ghost's windows by hwnd (the GHOST line: launched.txt and park files during the run, plus their final state):
  # G whatever process hosts them.
  $gh = @{}
  foreach ($l in $raw) { $q = $l -split ' '; if ($q[1] -eq 'GHOST') { $q | Select-Object -Skip 2 | ForEach-Object { $gh[$_] = 1 } } }
  $st = Join-Path $env:LOCALAPPDATA 'ghost'
  if (-not $planted) {
    Get-Content (Join-Path $st 'launched.txt') -ErrorAction SilentlyContinue | ForEach-Object { $gh[($_ -split ' ')[0]] = 1 }
    Get-ChildItem $st -Filter 'park-*.txt' -ErrorAction SilentlyContinue | ForEach-Object { $gh[$_.BaseName.Substring(5)] = 1 }
  }
  $ci = [Globalization.CultureInfo]::InvariantCulture
  $rows = @($raw | Where-Object { ($_ -split ' ', 3)[1] -notin 'E', 'GHOST' } | ForEach-Object { $p = $_ -split ' ', 12; if ($p[1] -eq 'END') { [pscustomobject]@{ T = [double]::Parse($p[0], $ci); I = $i++; H = 'END' } } else {
        # Owner: G = ghost started it, F = foreign. A line without the field (older format) counts as ghost's.
        $own = if ($gh.ContainsKey($p[2])) { 'G' } elseif ($p.Count -gt 9) { $p[9] } else { 'G' }
        [pscustomobject]@{ T = [double]::Parse($p[0], $ci); I = $i++; H = $p[2]; C = $p[3]; Own = $own; Name = $(if ($p.Count -gt 10) { $p[10] } else { '?' }); Title = $(if ($p.Count -gt 11) { $p[11] } else { '' }); Pid = $p[5] } } } | Sort-Object T, I)
  # Back-to-back events for one window are one stretch.
  $one = New-Object System.Collections.Generic.List[object]
  foreach ($r in $rows) { if ($one.Count -eq 0 -or $one[$one.Count - 1].H -ne $r.H) { $one.Add($r) } }
  $rows = $one.ToArray()
  if ($rows.Count -lt 2 -or $rows[-1].H -ne 'END') { return @('fg watch: unreadable log', "foreground watcher log could not be read ($($raw.Count) lines)") }
  $first = $rows[0]
  $none = $first.H -eq '0' -or $first.C -eq 'Shell_TrayWnd' -or $first.C -eq 'ghost-sink'
  $bad = @(); $max = 0; $total = 0; $long = @()
  for ($i = 0; $i -lt $rows.Count - 1; $i++) {
    $r = $rows[$i]; $d = $rows[$i + 1].T - $r.T
    if ($d -gt 50 -and $r.H -ne $first.H) { $long += "$($r.Own) $($r.Name) pid $($r.Pid) $($r.C) '$($r.Title)' $([int]$d) ms" }
    # Rule: ghost's claim is that ghost never takes the foreground, so only stretches held by ghost-started pids
    # count; a foreign process in front (another session's console, a toast) is not ghost's doing. The sink is
    # ghost's on purpose only when the user had no window (keys go nowhere there); taken from a user window it counts.
    $away = $r.Own -eq 'G' -and -not ($none -and $r.C -eq 'ghost-sink')
    if (-not $away) { continue }
    $total += $d; if ($d -gt $max) { $max = $d }
    if ($d -gt 50) { $bad += "foreground was $($r.H) ($($r.C), $($r.Name), ghost's) for $([int]$d) ms" }
  }
  $max = [int]$max; $total = [int]$total
  if ($total -gt 300) { $bad += "foreground was away $total ms in total, budget 300" }
  $who = if ($long) { '; stretches over 50 ms: ' + ($long -join ' | ') } else { '' }
  "fg watch: $($rows.Count - 1) changes (1 ms poll; hook cross-check $hook events), user had $(if ($none) { 'no window' } else { 'a window' }), longest away $max ms, total away $total ms$who"
  $bad
}
