# Browser mode gate: a ghost-launched Edge (throwaway profile) is driven fully in the background.
# Navigation, web form typing, clicks, in-page modifier keys, back, eval, all with the user's foreground untouched.
$ErrorActionPreference = 'Continue'
$g = Join-Path $PSScriptRoot '..\ghost.exe'
$edge = @("${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe", "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $edge) { 'BROWSER FAIL (Edge not found)'; exit 1 }
$u1 = ([Uri](Join-Path $PSScriptRoot 'web.html')).AbsoluteUri
$u2 = ([Uri](Join-Path $PSScriptRoot 'page2.html')).AbsoluteUri

Add-Type -AssemblyName System.Windows.Forms
Add-Type 'using System; using System.Runtime.InteropServices; public static class B { [StructLayout(LayoutKind.Sequential)] public struct RC { public int L, T, R, B; } [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow(); [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RC r); [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h); }'
$fg0 = [B]::GetForegroundWindow()
$fails = New-Object System.Collections.Generic.List[string]
function Fail($m) { $fails.Add($m); Write-Host "  x $m" }
function G { $o = & $g @args 2>&1 | Out-String; $c = $LASTEXITCODE; if ([B]::GetForegroundWindow() -ne $fg0 -and -not $script:stole) { $script:stole = $true; Fail "ghost $($args[0]) took the foreground" }; [pscustomobject]@{ Code = $c; Out = $o.Trim() } }
function Timed($sb) { $sw = [Diagnostics.Stopwatch]::StartNew(); $r = & $sb; $sw.Stop(); $r | Add-Member ms $sw.ElapsedMilliseconds -PassThru }

# No --user-data-dir: ghost must give a launched browser its own throwaway profile, never the user's.
$r = G launch $edge $u1
"launch -> [$($r.Code)] $($r.Out)"
if ($r.Code -ne 0 -or $r.Out -notmatch '^(\d+)\s') { Fail 'launch failed'; 'BROWSER FAIL'; exit 1 }
$h = $Matches[1]
$rc = New-Object B+RC; [B]::GetWindowRect([IntPtr][int64]$h, [ref]$rc) | Out-Null; $vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
if (($rc.L -lt $vs.Right) -and ($rc.R -gt $vs.Left) -and ($rc.T -lt $vs.Bottom) -and ($rc.B -gt $vs.Top)) { Fail 'browser window is on screen' }

$r = G wait Greet --timeout 10000
if ($r.Code -ne 0) { Fail "page never showed Greet: $($r.Out)" }

$r = G type Name bob
if ((G read Name).Out -ne 'bob') { Fail "web input did not get 'bob' (got: $((G read Name).Out))" }
$r = G click Greet
"click Greet -> $($r.Out -replace "`r?`n", ' | ')"
if ($r.Out -notmatch 'hello bob') { Fail 'click Greet diff does not show "hello bob"' }

# Modifier keys inside the page (impossible with posted messages, required in browser mode).
G type Name abc | Out-Null
$r = G key Name ctrl+a; if ($r.Code -ne 0) { Fail "key ctrl+a in the page failed: $($r.Out)" }
$r = G key Name bs;     if ($r.Code -ne 0) { Fail "key backspace in the page failed: $($r.Out)" }
$v = (G read Name).Out
if ($v -ne '' -and $v -notmatch '^""$') { Fail "ctrl+a then backspace left '$v' in the field" }

$r = Timed { G go $u2 }
"go -> [$($r.Code)] $($r.ms) ms $($r.Out -replace "`r?`n", ' | ')"
if ($r.Code -ne 0) { Fail "go failed: $($r.Out)" }
if ($r.ms -gt 3000) { Fail "go took $($r.ms) ms" }
if ((G tree --find 'second page').Out -notmatch 'second page') { Fail 'after go, page 2 text is not in the tree' }

$r = G eval 'document.title'
if ($r.Out -notmatch 'Page Two') { Fail "eval document.title gave '$($r.Out)'" }

$r = G back
if ($r.Code -ne 0) { Fail "back failed: $($r.Out)" }
$r = G wait Greet --timeout 5000
if ($r.Code -ne 0) { Fail 'back did not return to the first page' }

$r = G tree
"page tree: $($r.Out.Length) chars"
if ($r.Out.Length -gt 2500) { Fail "browser tree is $($r.Out.Length) chars, budget 2500" }

$r = G close
Start-Sleep -Milliseconds 1500
if ([B]::IsWindow([IntPtr][int64]$h)) { Fail 'close left the browser window open' }
if ([B]::GetForegroundWindow() -ne $fg0) { Fail 'foreground changed by the end' }

if ($fails.Count -eq 0) { 'BROWSER PASS'; exit 0 } else { "BROWSER FAIL ($($fails.Count))"; exit 1 }
