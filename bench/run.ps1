# Agent benchmark: the same task, run by headless Claude Haiku sessions, once per tool, N reps each.
# Every run starts from the same clean context (empty cwd, project settings only, no user MCP servers),
# so the token and time differences come from the tool. Results append to results.jsonl.
#   run.ps1 -Tools ghost,agent-browser,playwright,chrome,none -Tasks W1,W2,A1,N1 -Reps 3
param([string[]]$Tools = @('ghost', 'agent-browser'), [string[]]$Tasks = @('W1'), [int]$Reps = 3, [string]$Model = 'haiku',
      [string]$Web = 'http://127.0.0.1:8765/web.html', [string]$Tag = '')
$ErrorActionPreference = 'Continue'
$Tools = @($Tools | ForEach-Object { $_ -split "," }); $Tasks = @($Tasks | ForEach-Object { $_ -split "," })
$root = $PSScriptRoot
$ghostDir = Resolve-Path (Join-Path $root '..')
# Outside the user profile, so no ancestor CLAUDE.md loads; auto-memory off. Clean baseline is ~22k tokens.
$cwd = 'C:\ghost-bench'; New-Item -ItemType Directory -Force $cwd | Out-Null
$env:CLAUDE_CODE_DISABLE_AUTO_MEMORY = '1'
$out = Join-Path $root 'results.jsonl'
$fx = Join-Path $env:TEMP 'ghost-fixture.exe'
$csc = "$env:windir\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
& $csc /nologo /target:winexe /out:$fx (Join-Path $ghostDir 'test\fixture.cs') | Out-Null
$web = $Web   # default served by: python -m http.server 8765 --bind 127.0.0.1 (test dir); Playwright blocks file://
$pw = Join-Path $root 'playwright-mcp.json'
'{"mcpServers":{"playwright":{"command":"npx","args":["-y","@playwright/mcp@0.0.83","--browser","msedge","--headless","--isolated"]}}}' | Set-Content -Encoding ascii $pw

# One short usage brief per tool, kept to similar length so no tool gets a richer manual.
$brief = @{
  'ghost'         = 'Tool: `ghost` (on PATH, run it with Bash). Drives Windows apps and web pages in the background. Do the whole task in ONE call when you can: ghost run "launch <exe> [args]" "type <name> <text>" "click <name>" "read [name]" "close" ; each step prints what changed (a page load prints nav "title" url) and close prints "closed"; trust that output and exit codes, no need to re-check. Web pages: launch msedge <url> prints the page. Quote multi-word names inside a step: "type \"Search box\" hi". Other verbs: wait <text>, key <key>, go <url>, tree [--find <text>].'
  'agent-browser' = 'Tool: `agent-browser` (on PATH, run it with Bash). Chain commands with && in ONE call when you can: agent-browser open <url> && agent-browser snapshot -i ; snapshot -i lists elements with refs like @e1. Then fill @eN <text> && click @eN && get text <selector> && close ; trust that output, no need to re-check.Other verbs: press <key>, wait <selector|ms>, close. Use refs from the latest snapshot.'
  'playwright'    = 'Tool: the playwright MCP server tools (browser_navigate, browser_snapshot, browser_click, browser_type, browser_press_key, browser_close). Use refs from the latest snapshot. Trust tool output, no need to re-check.'
  'chrome'        = 'Tool: the Claude in Chrome browser tools (tabs_context_mcp, tabs_create_mcp, navigate, read_page, find, computer, form_input, get_page_text). Create a new tab first. Trust tool output, no need to re-check.'
  'none'          = 'No automation tool is installed. You may run PowerShell through Bash (powershell -NoProfile -Command "...") and use .NET UI Automation (System.Windows.Automation) or any built-in Windows means. Do not move the mouse. Do as much as you can in one command; trust its output, no need to re-check.'
}
$taskDefs = @{
  'W1' = @{ Prompt = "Open $web in a browser. Type bob into the Name field, press the Greet button, and reply with only the greeting text shown on the page. Close the browser when done."; Expect = 'hello bob' }
  'W2' = @{ Prompt = 'Open https://example.com in a browser, follow the "Learn more" link, and reply with only the title of the page it leads to. Close the browser when done.'; Expect = 'Example Domains' }
  'A1' = @{ Prompt = "Start the desktop test app $fx with the single argument {ID}; it opens a window titled {ID}. Type hello into its Body box, press its Settings button, and reply with only the text of the status label. Then close that window."; Expect = 'clicked' }
  'N1' = @{ Prompt = 'Open a File Explorer window at C:\Windows\Web and reply with only the names of the folders shown there, comma separated. Then close that window.'; Expect = 'Wallpaper' }
  'N2' = @{ Prompt = 'Open the Windows Calculator app, calculate 7 times 6 with its buttons, and reply with only the result shown on its display. Then close Calculator.'; Expect = '42' }
}
$basePath = $env:PATH

foreach ($tool in $Tools) { foreach ($task in $Tasks) { for ($i = 1; $i -le $Reps; $i++) {
  $id = "bench-$task-$tool-$i-" + (Get-Random -Maximum 9999)
  $prompt = $taskDefs[$task].Prompt.Replace('{ID}', $id)
  $bf = Join-Path $cwd "brief-$tool.txt"; [IO.File]::WriteAllText($bf, $brief[$tool])
  $a = @('-p', '--model', $Model, '--setting-sources', 'project', '--output-format', 'stream-json', '--verbose', '--no-session-persistence',
         '--append-system-prompt-file', $bf, '--disallowedTools', 'Write,Edit,NotebookEdit,WebFetch,WebSearch,Agent,Task')
  switch ($tool) {
    'playwright' { $a += @('--strict-mcp-config', '--mcp-config', $pw, '--allowedTools', 'mcp__playwright__*') }
    'chrome'     { $a += @('--strict-mcp-config', '--chrome', '--allowedTools', 'mcp__claude-in-chrome__*') }
    default      { $a += @('--strict-mcp-config', '--allowedTools', 'Bash') }
  }
  $env:PATH = if ($tool -eq 'ghost') { "$ghostDir;$basePath" } else { $basePath }   # only the ghost runs can find ghost
  Push-Location $cwd
  $sw = [Diagnostics.Stopwatch]::StartNew()
  $raw = ($prompt | claude @a 2>&1 | Out-String)
  $sw.Stop(); Pop-Location
  # stream-json: the 'result' event carries usage; tool_use inputs + tool_result contents are what the tool added to context.
  $j = $null; $toolChars = 0
  foreach ($line in ($raw -split "`r?`n")) { if (-not $line.StartsWith('{')) { continue }; try { $e = $line | ConvertFrom-Json } catch { continue }
    if ($e.type -eq 'result') { $j = $e }
    elseif ($e.type -in 'assistant', 'user') { foreach ($c in $e.message.content) {
      if ($c.type -eq 'tool_use') { $toolChars += ($c.input | ConvertTo-Json -Compress -Depth 6).Length }
      elseif ($c.type -eq 'tool_result') { $toolChars += ($c.content | ConvertTo-Json -Compress -Depth 6).Length } } } }
  $u = if ($j) { $j.usage } else { $null }
  $rec = [ordered]@{
    time = (Get-Date).ToString('s'); tag = $Tag; tool = $tool; task = $task; rep = $i; model = $Model
    ok = [bool]($j -and ([string]$j.result) -match [regex]::Escape($taskDefs[$task].Expect))
    result = if ($j) { ([string]$j.result).Trim() } else { $raw.Trim().Substring(0, [Math]::Min(300, $raw.Trim().Length)) }
    wall_ms = $sw.ElapsedMilliseconds; api_ms = if ($j) { $j.duration_api_ms } else { $null }; turns = if ($j) { $j.num_turns } else { $null }
    input = $u.input_tokens; cache_create = $u.cache_creation_input_tokens; cache_read = $u.cache_read_input_tokens; output = $u.output_tokens
    tool_chars = $toolChars; tool_tokens = [int]($toolChars / 4)
    total = if ($u) { $u.input_tokens + $u.cache_creation_input_tokens + $u.cache_read_input_tokens + $u.output_tokens } else { $null }
  }
  ($rec | ConvertTo-Json -Compress) | Add-Content -Encoding utf8 $out
  '{0,-14} {1} #{2} ok={3,-5} total={4,7} tool={9,5} out={5,5} turns={6,2} wall={7,6}ms  {8}' -f $tool, $task, $i, $rec.ok, $rec.total, $rec.output, $rec.turns, $rec.wall_ms, ($rec.result -replace "`r?`n", ' ').Substring(0, [Math]::Min(60, $rec.result.Length)), $rec.tool_tokens
}}}
