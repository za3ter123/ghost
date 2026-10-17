# Web loop gate on local file:// pages only (web.html, page2.html): multi-word names, errors that name the fix,
# browser launch with its own profile and the page printed, page-scoped wait/tree, navigation lines after click,
# page read, go, and name lookups that wait for late elements. Background only: the user's foreground is checked
# after every ghost call and by a 10 ms poller for the whole run (nothing else in front for over 50 ms), the
# browser must be parked off screen, its throwaway profile must hold no signed-in account, nothing may stay open.
$ErrorActionPreference = 'Continue'
$g = Join-Path $PSScriptRoot '..\ghost.exe'
$u1 = ([Uri](Join-Path $PSScriptRoot 'web.html')).AbsoluteUri
$u2 = ([Uri](Join-Path $PSScriptRoot 'page2.html')).AbsoluteUri
$env:GHOST_SESSION = 'web-features-' + $PID
Add-Type -AssemblyName System.Windows.Forms
Add-Type 'using System; using System.Runtime.InteropServices; public static class WF { [StructLayout(LayoutKind.Sequential)] public struct RC { public int L, T, R, B; } [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow(); [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RC r); [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h); [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindow(string c, string n); }'
$fg0 = [WF]::GetForegroundWindow()
$fails = New-Object System.Collections.Generic.List[string]
$opened = New-Object System.Collections.Generic.List[string]
function Fail($m) { $fails.Add($m); Write-Host "  x $m" }
. (Join-Path $PSScriptRoot 'fgwatch.ps1')
$fgwErr = Start-FgWatch; if ($fgwErr) { Fail $fgwErr }
# With no window in front (0, or the taskbar) ghost hands the foreground to the user's topmost window by design;
# a parked (ghost) window in front is never right.
$none = ($fg0 -eq [IntPtr]::Zero) -or ($fg0 -eq [WF]::FindWindow('Shell_TrayWnd', $null))
function FgOk { $f = [WF]::GetForegroundWindow(); ($f -eq $fg0) -or ($none -and ($f -eq [IntPtr]::Zero -or -not (Parked $f))) }
function G {
  $o = ((& $g @args 2>&1 | ForEach-Object { $_.ToString() }) -join "`n").Trim(); $c = $LASTEXITCODE
  if (-not (FgOk)) { Fail "ghost $($args -join ' ') took the foreground" }
  [pscustomobject]@{ Code = $c; Out = $o }
}
function Show($label, $r) { "{0} -> [{1}] {2}" -f $label, $r.Code, ($r.Out -replace "`r?`n", ' | ') }
function Parked($h) { $rc = New-Object WF+RC; [WF]::GetWindowRect([IntPtr][int64]$h, [ref]$rc) | Out-Null; $vs = [System.Windows.Forms.SystemInformation]::VirtualScreen; -not (($rc.L -lt $vs.Right) -and ($rc.R -gt $vs.Left) -and ($rc.T -lt $vs.Bottom) -and ($rc.B -gt $vs.Top)) }

# 3. Launch: bare `msedge`, own throwaway profile, accessibility on, page printed once loaded.
$r = G launch msedge $u1
Show 'launch' $r
if ($r.Code -ne 0 -or $r.Out -notmatch '^(\d+) (\d+) msedge') { Fail 'launch msedge failed'; 'WEB FAIL'; exit 1 }
$h = $Matches[1]; $bpid = $Matches[2]; $opened.Add($h)
if (-not (Parked $h)) { Fail 'browser window is on screen, expected parked' }
if ($r.Out -notmatch '(?m)^page "ghost web fixture"$') { Fail 'launch did not print the page line (title, no echoed url)' }
if ($r.Out -notmatch 'e\d+ btn Greet' -or $r.Out -notmatch 'link "Next page"') { Fail 'launch did not print the page elements' }
$cl = (Get-CimInstance Win32_Process -Filter "ProcessId=$bpid").CommandLine
$prof = [regex]::Match($cl, '--user-data-dir=([^" ]+)').Groups[1].Value
if (-not $prof.StartsWith((Join-Path $env:LOCALAPPDATA 'ghost\profiles'), 'OrdinalIgnoreCase')) { Fail "profile is not a ghost throwaway one: $prof" }
foreach ($f in '--no-first-run', '--no-default-browser-check', '--force-renderer-accessibility') { if ($cl -notmatch [regex]::Escape($f)) { Fail "launch command line lacks $f" } }

# 4. Page-scoped tree, find and wait.
$r = G tree
if ($r.Out -notmatch 'Address and search bar' -or $r.Out -notmatch '(?m)^e\d+ doc "ghost web fixture"') { Fail "default tree lacks the address bar or the page: $($r.Out)" }
if ($r.Out -match 'Minimize|Tab bar|New Tab') { Fail 'default tree shows browser chrome beyond the address bar' }
if ((G tree --full).Out -notmatch 'Minimize|New Tab') { Fail 'tree --full does not show the whole window' }
$r = G tree --find page
if ($r.Out -notmatch 'Next page' -or @($r.Out -split "`n").Count -ne 1) { Fail "tree --find page should list the name match only, got: $($r.Out)" }
$r = G wait 'ghost web fixture' --timeout 800
if ($r.Code -eq 0) { Fail 'wait matched the tab/window title instead of the page content' }
elseif ($r.Out -notmatch 'page "ghost web fixture"') { Fail "wait timeout does not name the page: $($r.Out)" }

# 1. Multi-word names in run steps and outside them.
$r = G run 'type Name bob' 'click Greet' 'wait hello bob'
Show 'run type/click/wait' $r
if ($r.Code -ne 0 -or $r.Out -notmatch 'hello bob') { Fail 'run type Name / click Greet / wait hello bob failed' }
# Positive control first: the first word alone is on the page, so a first-word-only wait would succeed.
$r = G wait hello --timeout 1000
if ($r.Code -ne 0) { Fail "control: wait hello failed: $($r.Out)" }
$r = G wait hello Zebra --timeout 300
if ($r.Code -ne 1 -or $r.Out -notmatch '^timeout after \d+ms: "hello Zebra" not found; page "ghost web fixture"') { Fail "wait hello Zebra should time out on the whole phrase, got [$($r.Code)] $($r.Out)" }
$r = G read Next page
if ($r.Code -ne 0 -or $r.Out -notmatch 'page2\.html' -or $r.Out -match 'windows:') { Fail "read Next page (outside run) gave: $($r.Out)" }

# 2. Errors that name the fix.
$r = G run 'click'
if ($r.Code -ne 2 -or $r.Out -notmatch 'usage: click <name\|eN\|oN>' -or $r.Out -match '\[win\]' -or $r.Out -notmatch 'names with spaces') { Fail "step usage line: $($r.Out)" }
$r = G run 'frobnicate x'
if ($r.Out -notmatch 'not a step command: frobnicate; steps start with one of: tree click type key read wait go') { Fail "bad step verb message: $($r.Out)" }
$r = G frobnicate
if ($r.Code -ne 2 -or $r.Out -notmatch 'unknown command: frobnicate; commands: windows launch use park show close tree click type key read wait go shot ocr run help') { Fail "unknown command message: $($r.Out)" }
# A failed command must not move the current window (here: go on a window that isn't a ghost browser).
$pm = [regex]::Match((G windows).Out, '(?m)^(\d+) \d+ explorer "Program Manager"').Groups[1].Value
if ($pm) {
  $r = G go $pm $u2
  if ($r.Code -eq 0) { Fail 'go on Program Manager succeeded' }
  if ((G read title).Out -ne 'ghost web fixture') { Fail 'a failed go changed the current window' }
}
$r = G click Grete
if ($r.Code -eq 0 -or $r.Out -notmatch 'nearest: e\d+ btn "Greet"') { Fail "missing name gives no nearest names: $($r.Out)" }

# 5. Navigation after click: nav line plus the new page, no list of removed old-page lines.
$r = G run 'click Next page'
Show 'click Next page' $r
if ($r.Out -notmatch '(?m)^1 nav "Page Two" file:///.*page2\.html$' -or $r.Out -notmatch 'second page') { Fail 'click Next page did not print nav and the new page' }
if ($r.Out -match '(?m)^1?- ') { Fail 'navigation printed removed lines for the old page' }

# 6. Reading the page.
$r = G read
if ($r.Out -notmatch '^page "Page Two" file:///' -or $r.Out -notmatch 'second page') { Fail "bare read: $($r.Out)" }
if ((G read title).Out -ne 'Page Two') { Fail 'read title is not Page Two' }
if ((G read url).Out -notmatch 'page2\.html$') { Fail 'read url is not page2.html' }

# 1b. A quoted multi-word target that carries text.
$r = G run 'type "Search box" hello there'
if ($r.Code -eq 0) { Fail 'type with an unquoted extra word should be a usage error' }
$r = G run 'type "Search box" "hello there"' 'read Search box'
if ($r.Code -ne 0 -or $r.Out -notmatch '(?m)^hello there$') { Fail "quoted multi-word target: $($r.Out)" }

# 8. A name that shows up a moment later resolves without an explicit wait.
$r = G run 'click Later' 'click Late one' 'wait late clicked'
Show 'late element' $r
if ($r.Code -ne 0) { Fail "late element was not waited for: $($r.Out)" }

# 7. go: refused through the address bar (it takes the foreground), works with go.
$r = G type 'Address and search bar' $u1
if ($r.Code -eq 0 -or $r.Out -notmatch 'ghost go') { Fail "typing into the address bar is not redirected to go: $($r.Out)" }
$r = G go $u1
Show 'go' $r
if ($r.Code -ne 0 -or $r.Out -notmatch '^(\d+) \d+ msedge') { Fail "go failed: $($r.Out)" }
else {
  $h2 = $Matches[1]; $opened.Add($h2)
  if ($r.Out -notmatch '(?m)^nav "ghost web fixture"$' -or $r.Out -notmatch 'btn Greet') { Fail 'go did not print the nav line and page' }
  if ([WF]::IsWindow([IntPtr][int64]$h)) { Fail 'go left the old window open' }
  if (-not (Parked $h2)) { Fail 'the window go opened is on screen' }
}

# 2b. An empty page says it is not exposed and how to recover.
$r = G go about:blank
if ($r.Out -match '^(\d+) ') { $opened.Add($Matches[1]) }
$r = G click Anything
if ($r.Out -notmatch 'page not exposed') { Fail "empty page error: $($r.Out)" }

$lp = [regex]::Match((Get-CimInstance Win32_Process -Filter "ProcessId=$([regex]::Match((G windows).Out, "(?m)^$($opened[-1]) (\d+) ").Groups[1].Value)").CommandLine, '--user-data-dir=([^" ]+)').Groups[1].Value
$r = G close
if ($r.Out -ne 'closed') { Fail "close: $($r.Out)" }
Start-Sleep -Milliseconds 1000
# 3b. No account: Edge must not sign its throwaway profiles into the owner's Microsoft account. Local State is
# written on exit, so it is read after close, for the first profile and the last one go made.
foreach ($pp in @($prof, $lp) | Where-Object { $_ } | Select-Object -Unique) {
  for ($i = 0; $i -lt 30 -and -not (Test-Path (Join-Path $pp 'Local State')); $i++) { Start-Sleep -Milliseconds 200 }
  Start-Sleep -Milliseconds 1500
  $ls = Get-Content (Join-Path $pp 'Local State') -Raw -ErrorAction SilentlyContinue
  $pf = Get-Content (Join-Path $pp 'Default\Preferences') -Raw -ErrorAction SilentlyContinue
  if (-not $ls) { Fail "no Local State in $pp" }
  elseif ($ls -match '"(user_name|gaia_name|gaia_id)":"[^"]+"' -or $pf -match '"account_info":\[\{') { Fail "throwaway profile $pp holds a signed-in account ($($Matches[0]))" }
}
$fgw = @(Stop-FgWatch); $fgw[0]; $fgw | Select-Object -Skip 1 | ForEach-Object { Fail $_ }
foreach ($w in $opened) { if ([WF]::IsWindow([IntPtr][int64]$w)) { Fail "window $w is still open"; & $g close $w | Out-Null } }
if (-not (FgOk)) { Fail 'foreground changed by the end' }

# 8b. The sink: off screen, a tool window (no taskbar or Alt+Tab entry), not in `ghost windows`, and keys or a
# close request don't touch it.
Add-Type 'using System; using System.Runtime.InteropServices; public static class SK { [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindow(string c, string n); [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr h, int i); [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, uint m, IntPtr w, IntPtr l); [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h); }'
$sk = [SK]::FindWindow('ghost-sink', $null)
if ($sk -eq [IntPtr]::Zero) { Fail 'ghost has no foreground sink window' }
else {
  if (-not (Parked $sk)) { Fail 'the sink is on screen' }
  if (([SK]::GetWindowLong($sk, -20) -band 0x80) -eq 0) { Fail 'the sink is not a tool window (it would show in the taskbar and Alt+Tab)' }
  if ((G windows).Out -match "(?m)^$sk ") { Fail 'the sink is listed by ghost windows' }
  [SK]::SendMessage($sk, 0x104, [IntPtr]0x73, [IntPtr]0x203E0001) | Out-Null   # Alt+F4 as WM_SYSKEYDOWN
  [SK]::SendMessage($sk, 0x10, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null      # WM_CLOSE
  if (-not [SK]::IsWindow($sk)) { Fail 'keys or WM_CLOSE closed the sink' }
}

# 9. Foreground 0: with no window in front, keys must keep going nowhere (ghost's sink), never into a window
# ghost launched. The owner's foreground is never moved for it: a real steal can't be given back in time (Windows
# refuses it, see daemon::debug_fg0).
# - The watcher's no-window rule, on a planted log: another window (the owner's) holding it then fails.
# - The guard's own decision, with the prior foreground treated as 0 (`debug-fg0`): the sink.
# - The real run (launch, type, click, navigate, go, close; sink activation and the guard) only when nothing of the
#   user's is in front (0, the taskbar, the sink). Otherwise it says so, as information: the live run is a
#   separate manual gate; the decision above is this gate's check.
$pl = Join-Path $script:fgwDir 'planted.log'
Set-Content $pl @('1.0 P 0 - V 0 O - 1000', '2.0 E 1769932 Chrome_WidgetWin_1 V 20984 O - 1100', '3.0 E 555 ghost-sink P 1 O - 3100', '4.0 END 3200')
if (@(Read-FgLog $pl).Count -lt 2) { Fail 'the watcher let a window other than the sink hold the foreground at foreground 0' }
$r = G launch msedge $u1
$w9 = if ($r.Out -match '^(\d+) ') { $Matches[1] } else { '' }
if ($w9) { $opened.Add($w9) }
$r = G debug-fg0 $w9
"fg-0 decision -> [$($r.Code)] $($r.Out)"
if ($r.Code -ne 0 -or $r.Out -notmatch '^fg0: the guard hands \d+ to the sink \d+$') { Fail "fg-0 decision: $($r.Out)" }
$r = G close $w9
$f = [WF]::GetForegroundWindow()
$sinkNow = [WF]::FindWindow('ghost-sink', $null)
if ($f -eq [IntPtr]::Zero -or $f -eq [WF]::FindWindow('Shell_TrayWnd', $null) -or $f -eq $sinkNow) {
  $fgwErr = Start-FgWatch; if ($fgwErr) { Fail $fgwErr }
  $o = ((& $g run "launch msedge $u1" 'type Name bob' 'click Greet' 'click Next page' "go $u1" 'click Greet' 'close' 2>&1 | ForEach-Object { $_.ToString() }) -join "`n")
  $code = $LASTEXITCODE
  $f2 = [WF]::GetForegroundWindow()
  "fg-0 run -> [$code] $($o -replace "`r?`n", ' | ') ; foreground after: $f2"
  if ($code -ne 0 -or $o -notmatch '(?m)^\d+ closed$') { Fail "web task at foreground 0 failed: $o" }
  foreach ($m in [regex]::Matches($o, '(?m)^(\d+) \d+ msedge')) { $opened.Add($m.Groups[1].Value) }
  if ($f2 -ne $f -and $f2 -ne [IntPtr]::Zero -and $f2 -ne [WF]::FindWindow('ghost-sink', $null)) { Fail "after the task at foreground 0, keys would go to $f2, not ghost's sink" }
  $fgw = @(Stop-FgWatch); 'fg-0 ' + $fgw[0]; $fgw | Select-Object -Skip 1 | ForEach-Object { Fail "fg-0: $_" }
}
else {
  "FG0 live: untested (owner active; the foreground is $f). The live run is a separate manual gate, run with the PC idle at the desktop"
}
foreach ($w in $opened) { if ([WF]::IsWindow([IntPtr][int64]$w)) { Fail "window $w is still open"; & $g close $w | Out-Null } }
if ($fails.Count -eq 0) { 'WEB PASS'; exit 0 } else { "WEB FAIL ($($fails.Count))"; exit 1 }
