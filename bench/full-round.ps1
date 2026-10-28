# One full comparison round on the current build, every tool against the same pages.
# Web: ghost, agent-browser, Playwright MCP, Claude in Chrome on W1 and W2 over a local http server
# (Playwright blocks file://). Apps: ghost and no tool on A1 and N2, ghost on N1 (the no-tool N1 run
# has closed unrelated Explorer windows by title, so it is left out here).
#   full-round.ps1 [-Reps 5]
param([int]$Reps = 5)
$ErrorActionPreference = 'Continue'
$run = Join-Path $PSScriptRoot 'run.ps1'
$test = Resolve-Path (Join-Path $PSScriptRoot '..\test')
$port = 8766
$srv = Start-Process python -ArgumentList '-m', 'http.server', $port, '--bind', '127.0.0.1', '--directory', $test -WindowStyle Hidden -PassThru
Start-Sleep 2
try {
  $web = "http://127.0.0.1:$port/web.html"
  $tag = 'round-' + (Get-Date -Format 'yyyyMMdd-HHmmss'); "tag $tag"
  & $run -Tools 'ghost,agent-browser,playwright,chrome' -Tasks 'W1,W2' -Reps $Reps -Web $web -Tag $tag | Out-Host
  & $run -Tools 'ghost,none' -Tasks 'A1,N2' -Reps $Reps -Web $web -Tag $tag | Out-Host
  & $run -Tools 'ghost' -Tasks 'N1' -Reps $Reps -Web $web -Tag $tag | Out-Host
} finally { Stop-Process -Id $srv.Id -ErrorAction SilentlyContinue }
