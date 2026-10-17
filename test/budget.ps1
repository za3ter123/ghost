# Context budget gate: each bench task done in ONE ghost call, counting every char the agent writes (the command,
# as "ghost ...") plus every char ghost prints. Budget: 4000 chars (~1000 tokens) per task, and the answer must be
# in the output. Closes only the windows it launches; checks the user's foreground after every call and with a
# 10 ms poller for the whole run. W2 needs the internet: offline it prints "W2 skipped: offline"; online it must pass.
$ErrorActionPreference = 'Continue'
$g = Join-Path $PSScriptRoot '..\ghost.exe'
$fx = Join-Path $env:TEMP 'ghost-fixture.exe'
$csc = "$env:windir\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
& $csc /nologo /target:winexe /out:$fx (Join-Path $PSScriptRoot 'fixture.cs') | Out-Null
$web = ([Uri](Join-Path $PSScriptRoot 'web.html')).AbsoluteUri
$id = 'budget-' + (Get-Random -Maximum 99999)
Add-Type 'using System; using System.Runtime.InteropServices; public static class BG { [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow(); [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h); [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindow(string c, string n); }'
$fg0 = [BG]::GetForegroundWindow()
Add-Type -AssemblyName System.Windows.Forms
Add-Type 'using System; using System.Runtime.InteropServices; public static class BR { [StructLayout(LayoutKind.Sequential)] public struct RC { public int L, T, R, B; } [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RC r); }'
function Parked($h) { $rc = New-Object BR+RC; [BR]::GetWindowRect($h, [ref]$rc) | Out-Null; $vs = [System.Windows.Forms.SystemInformation]::VirtualScreen; -not (($rc.L -lt $vs.Right) -and ($rc.R -gt $vs.Left) -and ($rc.T -lt $vs.Bottom) -and ($rc.B -gt $vs.Top)) }
# With no window in front (0, or the taskbar) ghost hands the foreground to the user's topmost window by design.
$none = ($fg0 -eq [IntPtr]::Zero) -or ($fg0 -eq [BG]::FindWindow('Shell_TrayWnd', $null))
function FgOk { $f = [BG]::GetForegroundWindow(); ($f -eq $fg0) -or ($none -and ($f -eq [IntPtr]::Zero -or -not (Parked $f))) }
$online = $true
try { Invoke-WebRequest -UseBasicParsing -Method Head -Uri 'https://example.com' -TimeoutSec 8 | Out-Null } catch { $online = $false }
$env:GHOST_SESSION = 'budget-' + $PID
$fails = New-Object System.Collections.Generic.List[string]
. (Join-Path $PSScriptRoot 'fgwatch.ps1')
$fgwErr = Start-FgWatch; if ($fgwErr) { $fails.Add($fgwErr) }

# name, steps, text the answer needs
$tasks = @(
  @('W1', @("launch msedge $web", 'type Name bob', 'click Greet', 'close'), 'hello bob'),
  @('W2', @('launch msedge https://example.com', 'click Learn more', 'close'), 'Example Domains'),
  @('A1', @("launch $fx $id", 'type Body hello', 'click Settings', 'read', 'close'), 'clicked'),
  @('N1', @('launch explorer.exe C:\Windows\Web', 'read', 'close'), @('4K', 'Screen', 'touchkeyboard', 'Wallpaper')),
  @('N2', @('launch calc.exe', 'click Seven', 'click Multiply by', 'click Six', 'click Equals', 'read', 'close'), 'Display is 42')
)
foreach ($t in $tasks) {
  $name, $steps, $want = $t
  if ($name -eq 'W2' -and -not $online) { 'W2 skipped: offline'; continue }
  $cmd = 'ghost run ' + (($steps | ForEach-Object { '"' + $_ + '"' }) -join ' ')
  $sw = [Diagnostics.Stopwatch]::StartNew()
  $out = (& $g run @steps 2>&1 | Out-String).Trim(); $code = $LASTEXITCODE
  $sw.Stop()
  $chars = $cmd.Length + $out.Length
  $h = [regex]::Match($out, '^(\d+) ').Groups[1].Value
  $ok = ($code -eq 0) -and -not @($want | Where-Object { $out -notmatch [regex]::Escape($_) }).Count -and ($out -match '(?m)^\d+ closed$')
  if ($h -and [BG]::IsWindow([IntPtr][int64]$h)) { $ok = $false; $fails.Add("$name left its window $h open") }
  if (-not (FgOk)) { $fails.Add("$name changed the foreground window") }
  '{0}  {1,5} chars  {2,5} ms  exit {3}  {4}' -f $name, $chars, $sw.ElapsedMilliseconds, $code, $(if ($ok) { 'ok' } else { 'WRONG' })
  if ($env:BUDGET_VERBOSE) { "  $cmd"; ($out -split "`r?`n") | ForEach-Object { "    $_" } }
  if (-not $ok) { $fails.Add("$name did not finish correctly: $($out -replace "`r?`n", ' | ')") }
  if ($chars -gt 4000) { $fails.Add("$name cost $chars chars > 4000") }
}
# The run rules on the app tasks: a usage error in any step launches nothing; a last close runs after a failure.
$before = @(Get-Process CalculatorApp -ErrorAction SilentlyContinue).Count
$out = (& $g run 'launch calc.exe' 'click Seven' 'read' 'go' 2>&1 | Out-String).Trim(); $code = $LASTEXITCODE
'N2 usage   exit {0}  {1}' -f $code, ($out -split "`r?`n")[0]
if ($code -ne 2 -or $out -notmatch 'step 4: usage: go') { $fails.Add("N2 with a bad step was not refused up front: $out") }
Start-Sleep -Milliseconds 1500
if (@(Get-Process CalculatorApp -ErrorAction SilentlyContinue).Count -gt $before) { $fails.Add('N2 with a bad step launched Calculator anyway') }
$out = (& $g run 'launch explorer.exe C:\Windows\Web' 'click No such item' 'close' 2>&1 | Out-String).Trim(); $code = $LASTEXITCODE
$h = [regex]::Match($out, '^(\d+) ').Groups[1].Value
'N1 finally exit {0}  {1}' -f $code, ($out -replace "`r?`n", ' | ')
if ($code -ne 1 -or $out -notmatch '(?m)^3 closed?$') { $fails.Add("N1 with a failed step did not close its window: $out") }
if ($h -and [BG]::IsWindow([IntPtr][int64]$h)) { $fails.Add("N1 with a failed step left its window $h open"); & $g close $h | Out-Null }
if (-not (FgOk)) { $fails.Add('the run-rule checks changed the foreground window') }
$fgw = @(Stop-FgWatch); $fgw[0]; $fgw | Select-Object -Skip 1 | ForEach-Object { $fails.Add($_) }
if ($fails.Count) { $fails | ForEach-Object { "  x $_" }; 'BUDGET FAIL'; exit 1 } else { 'BUDGET PASS'; exit 0 }
