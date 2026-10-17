# Prints the tool calls (and trimmed results) one benchmark run makes, to see where turns go.
#   trace.ps1 -Tool ghost -Task W1
# The W1 page comes from $env:GHOST_TRACE_WEB if set (a file:// URL to test/web.html works for ghost and agent-browser);
# the default, http://127.0.0.1:8765/web.html, needs a local http server serving test/.
param([string]$Tool = 'ghost', [string]$Task = 'W1', [string]$Model = 'haiku')
$ErrorActionPreference = 'Continue'
$root = $PSScriptRoot
$src = Get-Content (Join-Path $root 'run.ps1') -Raw
# Reuse run.ps1's briefs and tasks without running its loop.
$defs = $src.Substring(0, $src.IndexOf('foreach ($tool in $Tools)'))
# Invoke-Expression has no $PSScriptRoot and must not re-run run.ps1's param block.
$defs = ($defs -replace '(?ms)^param\(.*?\)\r?\n(?=\$ErrorActionPreference)', '').Replace('$PSScriptRoot', "'$root'")
$Web = if ($env:GHOST_TRACE_WEB) { $env:GHOST_TRACE_WEB } else { 'http://127.0.0.1:8765/web.html' }
$Tools = @($Tool); $Tasks = @($Task); $Reps = 0
Invoke-Expression $defs
$id = "trace-$Task-" + (Get-Random -Maximum 9999)
$prompt = $taskDefs[$Task].Prompt.Replace('{ID}', $id)
$bf = Join-Path $cwd "brief-$Tool.txt"; [IO.File]::WriteAllText($bf, $brief[$Tool])
$a = @('-p', '--model', $Model, '--setting-sources', 'project', '--output-format', 'stream-json', '--verbose', '--no-session-persistence',
       '--append-system-prompt-file', $bf, '--disallowedTools', 'Write,Edit,NotebookEdit,WebFetch,WebSearch,Agent,Task', '--strict-mcp-config', '--allowedTools', 'Bash')
if ($Tool -eq 'ghost') { $env:PATH = "$ghostDir;$env:PATH" }
Push-Location $cwd; $raw = $prompt | claude @a 2>&1; Pop-Location
foreach ($line in $raw) { try { $e = $line | ConvertFrom-Json } catch { continue }
  if ($e.type -eq 'assistant') { foreach ($c in $e.message.content) { if ($c.type -eq 'tool_use') { 'CALL: ' + ($c.input.command -replace "`r?`n", ' ; ') } elseif ($c.type -eq 'text' -and $c.text.Trim()) { 'SAY : ' + ($c.text.Trim() -replace "`r?`n", ' ').Substring(0, [Math]::Min(160, $c.text.Trim().Length)) } } }
  if ($e.type -eq 'user') { foreach ($c in $e.message.content) { if ($c.type -eq 'tool_result') { $t = ($c.content | Out-String).Trim(); '  -> ' + ($t -replace "`r?`n", ' | ').Substring(0, [Math]::Min(260, $t.Length)) } } }
  if ($e.type -eq 'result') { "RESULT turns=$($e.num_turns) total=$($e.usage.input_tokens + $e.usage.cache_creation_input_tokens + $e.usage.cache_read_input_tokens + $e.usage.output_tokens)" } }
