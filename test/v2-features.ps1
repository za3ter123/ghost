# ghost v2 feature checks on the throwaway fixture: stable refs, diff after action, batch, wait, OCR click.
$ErrorActionPreference = 'Continue'
$g = Join-Path $PSScriptRoot '..\ghost.exe'
$fx = Join-Path $env:TEMP 'ghost-fixture.exe'
$csc = "$env:windir\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
& $csc /nologo /target:winexe /out:$fx (Join-Path $PSScriptRoot 'fixture.cs') | Out-Null
$fails = New-Object System.Collections.Generic.List[string]
function Fail($m) { $fails.Add($m); Write-Host "  x $m" }
. (Join-Path $PSScriptRoot 'fgwatch.ps1')
$fgwErr = Start-FgWatch; if ($fgwErr) { Fail $fgwErr }
function G { $o = & $g @args 2>&1 | Out-String; [pscustomobject]@{ Code = $LASTEXITCODE; Out = $o.Trim() } }
function Launch($t) { $r = G launch $fx $t; if ($r.Out -match '^(\d+)\s') { $Matches[1] } else { Fail "launch: $($r.Out)"; $null } }
function RefOf($h, $pat) { $m = [regex]::Match((G tree $h).Out, "(?m)^\s*(e\d+) $pat"); if ($m.Success) { $m.Groups[1].Value } else { $null } }

# 1. Stable refs: same element keeps its ref across tree calls.
$h = Launch 'v2-a'
$b1 = RefOf $h '\w+ "?Settings"?(\s|$)'; $b2 = RefOf $h '\w+ "?Settings"?(\s|$)'
if (-not $b1 -or $b1 -ne $b2) { Fail "refs not stable ($b1 vs $b2)" }
$box = RefOf $h '(doc|edit|Document|Edit)\b'

# 2. Diff after action: clicking Settings must report the label change without a new tree call.
$r = G click $h $b1
"click -> $($r.Out)"
if ($r.Code -ne 0) { Fail "click failed: $($r.Out)" }
elseif ($r.Out -notmatch '(?m)^[+~] .*clicked') { Fail "click output has no +/~ diff line showing 'clicked'" }
if ((RefOf $h '\w+ "?Settings"?(\s|$)') -ne $b1) { Fail 'Settings ref changed after click' }

# 3. Wait: present text returns fast, missing text times out with exit 1.
$sw = [Diagnostics.Stopwatch]::StartNew(); $r = G wait $h clicked --timeout 2000; $sw.Stop()
if ($r.Code -ne 0) { Fail "wait for present text failed: $($r.Out)" }
$sw = [Diagnostics.Stopwatch]::StartNew(); $r = G wait $h no-such-text-xyz --timeout 300; $sw.Stop()
if ($r.Code -eq 0) { Fail 'wait for missing text returned success' }
if ($sw.ElapsedMilliseconds -gt 1500) { Fail "wait timeout took $($sw.ElapsedMilliseconds) ms for a 300 ms timeout" }

# 4. Batch: several steps in one process call, results in order.
$r = G run $h "type $box batch-one" "key $box x" "read $box"
"run -> $($r.Out -replace "`r?`n", ' | ')"
if ($r.Code -ne 0) { Fail "run failed: $($r.Out)" }
elseif ($r.Out -notmatch 'batch-onex') { Fail 'run did not type, key and read back batch-onex in one call' }
$r = G run $h "click e99999" "type $box never"
if ($r.Code -eq 0) { Fail 'run with a bad first step returned success' }
if ((G read $h $box).Out -match 'never') { Fail 'run kept going after a failed step' }
G close $h | Out-Null

# 5. OCR: read the fixture as text with coordinates, then click a word by its OCR ref.
$h = Launch 'v2-b'
Start-Sleep -Milliseconds 300
$r = G ocr $h
"ocr -> $($r.Out -replace "`r?`n", ' | ')"
$o = [regex]::Match($r.Out, '(?m)^(o\d+) "?Settings"?(\s|$)')
if (-not $o.Success) { Fail 'ocr did not find the Settings button text' }
else {
  $c = G click $h $o.Groups[1].Value
  "ocr click -> $($c.Out)"
  Start-Sleep -Milliseconds 300
  if ((G tree $h --find clicked).Out -notmatch 'clicked') { Fail 'clicking the OCR ref did not press the button' }
}
G close $h | Out-Null

# 6. Ownership: a window ghost did not launch must not be acted on through the implicit current window.
$t = 'v2-foreign-' + (Get-Random -Maximum 99999)
# Style 7 = minimized without activation, so the test itself never takes the user's foreground.
(New-Object -ComObject WScript.Shell).Run(('"' + $fx + '" ' + $t), 7, $false) | Out-Null
$p = $null
$fh = $null
for ($i = 0; $i -lt 50 -and -not $fh; $i++) { Start-Sleep -Milliseconds 100; $m = [regex]::Match((G windows).Out, "(?m)^(\d+) .*$t"); if ($m.Success) { $fh = $m.Groups[1].Value } }
if (-not $fh) { Fail 'foreign fixture window never appeared' }
else {
  G use $fh | Out-Null
  $r = G close
  if ($r.Code -eq 0) { Fail 'implicit close on a window ghost did not launch returned success' }
  $r = G click Settings
  if ($r.Code -eq 0) { Fail 'implicit click on a window ghost did not launch returned success' }
  Start-Sleep -Milliseconds 300
  if (-not (G windows).Out.Contains($t)) { Fail 'foreign window was closed through the implicit current window' }
  $r = G tree
  if ($r.Code -ne 0) { Fail "read-only tree on the current foreign window failed: $($r.Out)" }
  $r = G close $fh
  if ($r.Code -ne 0) { Fail "explicit close of a named foreign window failed: $($r.Out)" }
}
if ($p -and -not $p.HasExited) { Start-Sleep -Milliseconds 500; if (-not $p.HasExited) { $p.Kill() } }

# 7. Truthful success: clicking a checkbox must really toggle it, or fail.
$h = Launch 'v2-c'
$r = G click $h Hold
if ($r.Code -ne 0) { Fail "click Hold failed: $($r.Out)" }
elseif ((G tree $h --find Hold).Out -notmatch '\[on\]') { Fail "click Hold exited 0 but the checkbox is not on (tree: $((G tree $h --find Hold).Out))" }

# 7b. Disabled controls are refused, and a success with no visible effect says so instead of staying silent.
$r = G click $h Locked
if ($r.Code -eq 0) { Fail 'click on a disabled button returned success' }
if ((G tree $h --find locked-fired).Out -match 'locked-fired') { Fail 'click on a disabled button ran its handler' }
$r = G click $h Noop
if ($r.Code -ne 0) { Fail "click Noop failed: $($r.Out)" }
elseif ($r.Out -notmatch 'no change') { Fail "click with no visible effect printed '$($r.Out)' instead of 'no change'" }
$r = G type $h Body typed-check
if ((G read $h Body).Out -notmatch 'typed-check') { Fail 'type exited without the text arriving' }

# 8. Hung app: ghost answers busy fast, never runs the refused action later, and other windows stay responsive.
G click $h Freeze --quiet | Out-Null
Start-Sleep -Milliseconds 300
$sw = [Diagnostics.Stopwatch]::StartNew(); $r = G click $h Settings; $sw.Stop()
if ($r.Code -eq 0) { Fail 'click on a frozen app returned success' }
if ($sw.ElapsedMilliseconds -gt 1500) { Fail "click on a frozen app took $($sw.ElapsedMilliseconds) ms to answer" }
$sw = [Diagnostics.Stopwatch]::StartNew(); $w = G windows; $sw.Stop()
if ($sw.ElapsedMilliseconds -gt 500) { Fail "windows took $($sw.ElapsedMilliseconds) ms while another app was hung" }
Start-Sleep -Milliseconds 5000   # fixture unfreezes after 4 s
if ((G tree $h --find clicked).Out -match 'clicked') { Fail 'the click refused as busy ran later anyway' }
G close $h | Out-Null

# 9. Modal dialog: it opens without taking focus, close reports it, its button can be pressed, then close works.
Add-Type 'using System; using System.Runtime.InteropServices; public static class F9 { [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow(); }'
$fg0 = [F9]::GetForegroundWindow()
$h = Launch 'v2-d'
G click $h Ask | Out-Null
$dlg = $null
for ($i = 0; $i -lt 30 -and -not $dlg; $i++) { Start-Sleep -Milliseconds 100; $m = [regex]::Match((G windows).Out, '(?m)^(\d+) .*Confirm'); if ($m.Success) { $dlg = $m.Groups[1].Value } }
if (-not $dlg) { Fail 'the Confirm dialog never appeared in windows' }
else {
  if ([F9]::GetForegroundWindow() -ne $fg0) { Fail 'the dialog took the foreground window' }
  Add-Type -AssemblyName System.Windows.Forms
  Add-Type 'using System; using System.Runtime.InteropServices; public static class F9b { [StructLayout(LayoutKind.Sequential)] public struct RC { public int L, T, R, B; } [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RC r); }'
  Start-Sleep -Milliseconds 300
  $rc = New-Object F9b+RC; [F9b]::GetWindowRect([IntPtr][int64]$dlg, [ref]$rc) | Out-Null; $vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
  if (($rc.L -lt $vs.Right) -and ($rc.R -gt $vs.Left) -and ($rc.T -lt $vs.Bottom) -and ($rc.B -gt $vs.Top)) { Fail 'the modal dialog appeared on the user''s screen instead of parked' }
  $r = G close $h
  if ($r.Code -eq 0) { Fail 'close returned success while a modal dialog was open' }
  elseif ($r.Out -notmatch 'Confirm') { Fail "close did not report the open dialog (got: $($r.Out))" }
  $r = G click $dlg OK
  if ($r.Code -ne 0) { Fail "pressing OK on the dialog failed: $($r.Out)" }
  Start-Sleep -Milliseconds 300
  if ((G tree $h --find answered).Out -notmatch 'answered') { Fail 'the dialog did not close after OK' }
  $r = G click $h Settings
  if ($r.Code -ne 0) { Fail "the main window stayed disabled after the dialog closed: $($r.Out)" }
}
G close $h | Out-Null
if ([F9]::GetForegroundWindow() -ne $fg0) { Fail 'section 9 left the foreground changed' }

# 10. A non-modal popup opened during a shielded action must not leave the owner disabled, and app popups stay off screen.
Add-Type -AssemblyName System.Windows.Forms
Add-Type 'using System; using System.Runtime.InteropServices; public static class F10 { [StructLayout(LayoutKind.Sequential)] public struct RC { public int L, T, R, B; } [DllImport("user32.dll")] public static extern bool IsWindowEnabled(IntPtr h); [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RC r); }'
function OnScreen($hw) { $rc = New-Object F10+RC; [F10]::GetWindowRect([IntPtr][int64]$hw, [ref]$rc) | Out-Null; $vs = [System.Windows.Forms.SystemInformation]::VirtualScreen; ($rc.L -lt $vs.Right) -and ($rc.R -gt $vs.Left) -and ($rc.T -lt $vs.Bottom) -and ($rc.B -gt $vs.Top) }
$h = Launch 'v2-e'
$r = G click $h Maximize
if ($r.Code -ne 0) { Fail "click Maximize failed: $($r.Out)" }
$tip = $null
for ($i = 0; $i -lt 20 -and -not $tip; $i++) { Start-Sleep -Milliseconds 100; $m = [regex]::Match((G windows).Out, '(?m)^(\d+) .*\bTip\b'); if ($m.Success) { $tip = $m.Groups[1].Value } }
Start-Sleep -Milliseconds 500
if (-not [F10]::IsWindowEnabled([IntPtr][int64]$h)) { Fail 'owner window left disabled after a non-modal popup opened' }
$r = G click $h Settings
if ($r.Code -ne 0) { Fail "owner not actable after the popup opened: $($r.Out)" }
if (-not $tip) { Fail 'the Tip popup never appeared' } elseif (OnScreen $tip) { Fail 'the app popup Tip appeared on the user''s screen instead of parked' }
$r = G type $h Settings RENAMED
if ($r.Code -eq 0) { Fail 'type into a button (not editable) returned success' }
G close $h | Out-Null

# 4b. run as an agent writes it: bare read in an app window; every step checked before any runs (a usage error
# launches nothing); a last close runs even after a failed step and says so.
$r = G run "launch $fx v2-read" 'type Body hello' 'click Settings' 'read' 'close'
"run read -> [$($r.Code)] $($r.Out -replace "`r?`n", ' | ')"
if ($r.Code -ne 0 -or $r.Out -notmatch '(?m)^clicked?$' -or $r.Out -notmatch '(?m)^Body: hello?$' -or $r.Out -notmatch '(?m)^5 closed?$') { Fail "bare read in run: $($r.Out)" }
$r = G run "launch $fx v2-usage" 'type Body hi' 'go'
"run usage -> [$($r.Code)] $($r.Out -replace "`r?`n", ' | ')"
if ($r.Code -ne 2 -or $r.Out -notmatch 'step 3: usage: go <url>') { Fail "a bad step was not refused up front: $($r.Out)" }
Start-Sleep -Milliseconds 1500
if ((G windows).Out -match '"v2-usage"') { Fail 'a run with a usage error launched its window anyway'; G close ([regex]::Match((G windows).Out, '(?m)^(\d+) .*"v2-usage"').Groups[1].Value) | Out-Null }
$r = G run "launch $fx v2-final" 'click No such button' 'close'
"run finally -> [$($r.Code)] $($r.Out -replace "`r?`n", ' | ')"
$hf = [regex]::Match($r.Out, '^(\d+) ').Groups[1].Value
if ($r.Code -ne 1 -or $r.Out -notmatch 'step 2: ' -or $r.Out -notmatch '(?m)^3 closed?$') { Fail "a last close did not run after a failed step: $($r.Out)" }
if ($hf -and ((G windows).Out -match "(?m)^$hf ")) { Fail 'the failed run left its window open'; G close $hf | Out-Null }

$fgw = @(Stop-FgWatch); $fgw[0]; $fgw | Select-Object -Skip 1 | ForEach-Object { Fail $_ }
if ($fails.Count -eq 0) { 'V2 PASS'; exit 0 } else { "V2 FAIL ($($fails.Count))"; exit 1 }
