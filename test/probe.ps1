# Independent acceptance probe for ghost.exe. Written by the orchestrator; the builder must not edit it.
# Drives a throwaway fixture window (never the user's apps) through ghost and checks the user's cursor and foreground never change.
$ErrorActionPreference = 'Stop'
$g = Join-Path $PSScriptRoot '..\ghost.exe'
if (-not (Test-Path $g)) { "FAIL ghost.exe missing at $g"; exit 1 }

$fx = Join-Path $env:TEMP 'ghost-fixture.exe'
$csc = "$env:windir\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
& $csc /nologo /target:winexe /out:$fx (Join-Path $PSScriptRoot 'fixture.cs') | Out-Null
if (-not (Test-Path $fx)) { 'FAIL fixture did not build'; exit 1 }

Add-Type -AssemblyName System.Drawing, System.Windows.Forms
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class P {
  [StructLayout(LayoutKind.Sequential)] public struct PT { public int X, Y; }
  [StructLayout(LayoutKind.Sequential)] public struct RC { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool GetCursorPos(out PT p);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RC r);
}
'@
function Snap { $p = New-Object P+PT; [P]::GetCursorPos([ref]$p) | Out-Null; [pscustomobject]@{ X = $p.X; Y = $p.Y; Fg = [P]::GetForegroundWindow() } }
$fails = New-Object System.Collections.Generic.List[string]
function Fail($m) { $fails.Add($m); Write-Host "  x $m" }
function Ghost { $o = & $g @args 2>&1 | Out-String; $code = $LASTEXITCODE; Start-Sleep -Milliseconds 400; FgSame "ghost $($args[0])"; [pscustomobject]@{ Code = $code; Out = $o.Trim() } }
# Checked after EVERY ghost call: a steal that is undone later (e.g. by close) still counts.
function FgSame($step) { $s = [P]::GetForegroundWindow(); if ($s -ne $before.Fg -and -not $script:stolen) { $script:stolen = $true; Fail "$step took the foreground window ($($before.Fg) -> $s)" } }

$before = Snap
. (Join-Path $PSScriptRoot 'fgwatch.ps1')
$fgwErr = Start-FgWatch; if ($fgwErr) { Fail $fgwErr }
"before: cursor=($($before.X),$($before.Y)) fg=$($before.Fg)"

$title = 'ghost-fixture-' + (Get-Random -Maximum 999999)
$r = Ghost launch $fx $title
"launch -> [$($r.Code)] $($r.Out)"
if ($r.Code -ne 0 -or $r.Out -notmatch '^(\d+)\s') { Fail 'launch did not print a hwnd'; "PROBE FAIL ($($fails.Count))"; exit 1 }
$hwnd = $Matches[1]
$h = [IntPtr][int64]$hwnd
if (-not [P]::IsWindow($h)) { Fail "launch hwnd $hwnd is not a window" }
if ($r.Out -notmatch [regex]::Escape($title)) { Fail "launch picked the wrong window (expected title $title)" }

$afterLaunch = Snap
if ($afterLaunch.Fg -ne $before.Fg) { Fail "launch changed the foreground window ($($before.Fg) -> $($afterLaunch.Fg))" }

$rc = New-Object P+RC; [P]::GetWindowRect($h, [ref]$rc) | Out-Null
$vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
$visible = ($rc.L -lt $vs.Right) -and ($rc.R -gt $vs.Left) -and ($rc.T -lt $vs.Bottom) -and ($rc.B -gt $vs.Top)
if ($visible) { Fail "launched window is on screen at ($($rc.L),$($rc.T),$($rc.R),$($rc.B)); expected parked off screen" }

$r = Ghost windows
if ($r.Code -ne 0 -or $r.Out -notmatch "(?m)^$hwnd\s") { Fail 'windows does not list the launched hwnd' }

$r = Ghost tree $title
"tree -> [$($r.Code)] $($r.Out.Length) chars"
if ($r.Code -ne 0) { Fail "tree by title failed: $($r.Out)" }
if ($r.Out.Length -gt 6000) { Fail "tree output $($r.Out.Length) chars, cap is 6000" }
$edit = [regex]::Match($r.Out, '(?m)^\s*(e\d+) (Document|Edit|doc|edit)\b')
if (-not $edit.Success) { Fail 'tree shows no Document/Edit element'; $r.Out; Ghost close $hwnd | Out-Null; "PROBE FAIL ($($fails.Count))"; exit 1 }
$ref = $edit.Groups[1].Value

$token = 'ghost-probe-' + (Get-Random -Maximum 999999)
$r = Ghost type $hwnd $ref $token
"type -> [$($r.Code)] $($r.Out)"
if ($r.Code -ne 0) { Fail "type failed: $($r.Out)" }
$r = Ghost read $hwnd $ref
if ($r.Out -notmatch [regex]::Escape($token)) { Fail "read did not return the typed token (got: $($r.Out))" }

$r = Ghost key $hwnd x
if ($r.Code -ne 0) { Fail "key failed: $($r.Out)" }
$r = Ghost read $hwnd $ref
if ($r.Out -notmatch ([regex]::Escape($token) + 'x')) { Fail "key x did not reach the focused textbox (got: $($r.Out))" }

$r = Ghost tree $hwnd --find Settings
$btn = [regex]::Match($r.Out, '(?m)^\s*(e\d+) \w+ "?Settings"?(\s|$)')
if ($btn.Success) {
  $c = Ghost click $hwnd $btn.Groups[1].Value
  "click Settings -> [$($c.Code)] $($c.Out)"
  if ($c.Code -ne 0) { Fail "click failed: $($c.Out)" }
  Start-Sleep -Milliseconds 300
  $s = Ghost tree $hwnd --find clicked
  if ($s.Out -notmatch 'clicked') { Fail "click reported ok but the button handler did not run (got: $($s.Out))" }
} else { Fail "tree --find Settings found no Settings element (got: $($r.Out))" }

$png = Join-Path $env:TEMP 'ghost-probe.png'
Remove-Item $png -ErrorAction SilentlyContinue
$r = Ghost shot $hwnd $png --scale 0.5
"shot -> [$($r.Code)] $($r.Out)"
if ($r.Code -ne 0 -or -not (Test-Path $png)) { Fail 'shot produced no file' }
else {
  $bmp = [System.Drawing.Bitmap]::FromFile($png)
  $colors = New-Object System.Collections.Generic.HashSet[int]
  for ($x = 0; $x -lt $bmp.Width; $x += [Math]::Max(1, [int]($bmp.Width / 40))) { for ($y = 0; $y -lt $bmp.Height; $y += [Math]::Max(1, [int]($bmp.Height / 40))) { [void]$colors.Add($bmp.GetPixel($x, $y).ToArgb()) } }
  "shot: $($bmp.Width)x$($bmp.Height), $($colors.Count) sampled colors"
  $bmp.Dispose()
  if ($colors.Count -lt 3) { Fail "shot looks blank ($($colors.Count) colors)" }
}

Ghost close $hwnd | Out-Null
Start-Sleep -Milliseconds 1000
if ([P]::IsWindow($h)) { Fail 'close left the fixture window open' }

$after = Snap
if ($after.X -ne $before.X -or $after.Y -ne $before.Y) { Fail "cursor moved ($($before.X),$($before.Y)) -> ($($after.X),$($after.Y)); rerun without touching the mouse to rule out your own hand" }
if ($after.Fg -ne $before.Fg) { Fail "foreground window changed ($($before.Fg) -> $($after.Fg))" }

$fgw = @(Stop-FgWatch); $fgw[0]; $fgw | Select-Object -Skip 1 | ForEach-Object { Fail $_ }
if ($fails.Count -eq 0) { 'PROBE PASS'; exit 0 } else { "PROBE FAIL ($($fails.Count))"; exit 1 }
