# G9 live, opportunistic: wait (up to 20 min) for the foreground to become 0 on its own, never forcing it, then
# immediately run the web task with the watcher on and the daemon's decisions logged. Prints one of:
#   FG0 live: pass          - a real fg 0 occurred and the task's keys stayed off the browser (sink or 0)
#   FG0 live: fail <why>     - fg 0 occurred but a ghost-launched window held the foreground during the task
#   no fg-0 occurred in 20 min
# Trial files go under test/out/ (gitignored): the watcher log, the GHOST_DEBUG decisions, and a transcript.
$ErrorActionPreference = 'Continue'
$g = Join-Path $PSScriptRoot '..\ghost.exe'
$u1 = ([Uri](Join-Path $PSScriptRoot 'web.html')).AbsoluteUri
$env:GHOST_SESSION = 'fg0-live-' + $PID
$out = Join-Path $PSScriptRoot 'out'; New-Item -ItemType Directory -Force $out | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$log = Join-Path $out "fg0-live-$stamp.txt"
$dbg = Join-Path $out "fg0-live-$stamp.dbg.log"
$env:GHOST_DEBUG = $dbg
. (Join-Path $PSScriptRoot 'fgwatch.ps1')
Add-Type 'using System; using System.Runtime.InteropServices; using System.Text; public static class FL { [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow(); [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindow(string c, string n); [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n); public static string C(IntPtr h){ var b = new StringBuilder(64); GetClassName(h, b, 64); return b.ToString(); } }'
function Say($m) { $line = "{0} {1}" -f (Get-Date -Format 'HH:mm:ss'), $m; $line; Add-Content $log $line }

Say "waiting up to 20 min for a natural foreground 0 (never forced); session $env:GHOST_SESSION"
$sink = [FL]::FindWindow('ghost-sink', $null); $tray = [FL]::FindWindow('Shell_TrayWnd', $null)
$deadline = (Get-Date).AddMinutes(20)
$hit = $false
while ((Get-Date) -lt $deadline) {
  $f = [FL]::GetForegroundWindow()
  if ($f -eq [IntPtr]::Zero -or $f -eq $tray -or $f -eq $sink) { $hit = $true; break }
  Start-Sleep -Milliseconds 100
}
if (-not $hit) { Say 'no fg-0 occurred in 20 min'; 'no fg-0 occurred in 20 min'; exit 0 }

$fnow = [FL]::GetForegroundWindow()
Say ("foreground 0 seen (hwnd {0} class '{1}'); running the web task now" -f $fnow, [FL]::C($fnow))
$fgwErr = Start-FgWatch; if ($fgwErr) { Say "watcher error: $fgwErr"; 'FG0 live: fail (watcher not ready)'; exit 1 }
$o = ((& $g run "launch msedge $u1" 'type Name bob' 'click Greet' 'close' 2>&1 | ForEach-Object { $_.ToString() }) -join "`n")
$f2 = [FL]::GetForegroundWindow()
$fgw = @(Stop-FgWatch)
Add-Content $log "task output:`n$o"
Add-Content $log ("watcher: " + ($fgw -join ' | '))
Say ("foreground after task: {0} '{1}'" -f $f2, [FL]::C($f2))
$ran = $o -match '(?m)^\d+ closed\r?$'
$keysSafe = ($f2 -eq [IntPtr]::Zero -or $f2 -eq [FL]::FindWindow('ghost-sink', $null)) -and -not ($fgw | Select-Object -Skip 1)
if ($ran -and $keysSafe) { Say 'FG0 live: pass'; 'FG0 live: pass'; exit 0 }
$why = if (-not $ran) { "the task did not finish: $o" } else { "keys could have reached $f2 ('$([FL]::C($f2))'); watcher: $($fgw | Select-Object -Skip 1)" }
Say "FG0 live: fail $why"; "FG0 live: fail $why"; exit 1
