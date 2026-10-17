# Benchmarks

Last updated: 2026-10-17

These numbers compare what an agent spends to finish the same small task with ghost and with other tools. Every number below is printed by `node bench/summarize.mjs`, which reads `bench/results.public.jsonl`. Rerun it to check them.

## Method

- The agent is Claude Haiku, run headless through `claude -p` by `bench/run.ps1`. `bench/agent-gate.ps1` runs every task with ghost and then with the reference tool for that task.
- Each run starts from a clean context: an empty working folder outside the user profile, so no CLAUDE.md from a parent folder loads, project settings only, auto memory off, no MCP servers except the one under test, and no Write, Edit, WebFetch, WebSearch or subagent tools.
- Every tool gets the same task prompt and one short usage brief of similar length, so no tool gets a richer manual. The briefs and prompts are in `bench/run.ps1`.
- Each task and tool pair runs 3 times per round, on one Windows 11 PC.
- A run counts as correct when the agent's final reply contains the expected text.
- Total tokens are input plus cache creation plus cache reads plus output, as Claude Code reports them for the whole run. Turns are Claude Code's own count. Wall time is measured around the `claude` call.
- Tool tokens are what the tool put into the agent's context: the characters of every tool call input and tool result, divided by 4. For ghost this is ghost's own cost, its commands plus their output. Rows from before this round did not record it.

Claude Code's own fixed context is about 20k tokens per turn: over all 129 rows, total tokens divided by turns has a median of 20366 and never falls below 17540. Any tool needs at least 2 turns (one tool call, one reply), so about 40k tokens is the floor for every tool here. ghost reaches it on W2, A1, N1 and N2.

## Tasks

| Task | What the agent is asked to do | Expected reply |
|---|---|---|
| W1 | Open a local test page (`test/web.html`), type bob into Name, press Greet, report the greeting, close the browser | hello bob |
| W2 | Open https://example.com, follow "Learn more", report the title of the page it leads to, close the browser | Example Domains |
| A1 | Start the WinForms test app built from `test/fixture.cs`, type hello into Body, press Settings, report the status label, close it | clicked |
| N1 | Open File Explorer at `C:\Windows\Web`, list the folders shown, close it | Wallpaper |
| N2 | Open Calculator, compute 7 times 6 with its buttons, report the display, close it | 42 |

## Tools

- ghost: this repository, on PATH for the ghost runs only.
- agent-browser: the agent-browser CLI, run through Bash.
- Playwright MCP: `@playwright/mcp@0.0.83` with headless Edge, isolated profile.
- Claude in Chrome: Claude Code's `--chrome` browser tools.
- no tool: Haiku with Bash, allowed to run PowerShell and .NET UI Automation (System.Windows.Automation) or any built-in Windows means, told not to move the mouse.

## Current numbers

The current round is the rows tagged `gate-20261017-152449` (a run by an independent verifier), `gate-20261017-150207`, and `chrome-w2` for Claude in Chrome. In the headline each cell is the median total tokens, with correct runs over all runs in brackets. Lower is better.

### Headline

| Task | ghost | agent-browser | Playwright MCP (earlier round) | Claude in Chrome | no tool |
|---|---|---|---|---|---|
| W1 | 50.2k (6/6) | 70.5k (6/6) | 153.3k (3/3) |  |  |
| W2 | 40.7k (6/6) | 79.9k (6/6) | 128.5k (3/3) | 195.4k (3/3) |  |
| A1 | 40.4k (6/6) |  |  |  | 110.8k (4/6) |
| N1 | 39.7k (6/6) |  |  |  | 106.3k (6/6) |
| N2 | 39.9k (6/6) |  |  |  | 64.6k (6/6) |

### Current round, both gate runs and the Claude in Chrome run together

| Task | Tool | Correct | Median total tokens | Median turns | Median wall time | Median tool tokens |
|---|---|---|---|---|---|---|
| W1 | ghost | 6/6 | 50168 | 3 | 9.9 s | 151 |
| W1 | agent-browser | 6/6 | 70468 | 4 | 12.3 s | 210 |
| W2 | ghost | 6/6 | 40703 | 2 | 10.7 s | 783 |
| W2 | agent-browser | 6/6 | 79947 | 4 | 13.7 s | 141 |
| W2 | Claude in Chrome | 3/3 | 195421 | 11 | 29.3 s | 960 |
| A1 | ghost | 6/6 | 40386 | 2 | 9.8 s | 100 |
| A1 | no tool | 4/6 | 110766 | 5 | 37.6 s | 2679 |
| N1 | ghost | 6/6 | 39720 | 2 | 8.8 s | 86 |
| N1 | no tool | 6/6 | 106268 | 5 | 21.2 s | 639 |
| N2 | ghost | 6/6 | 39869 | 2 | 9.0 s | 132 |
| N2 | no tool | 6/6 | 64639 | 3 | 17.6 s | 891 |

ghost's tool tokens are higher on W2 because `launch msedge <url>` prints the page it opened, so the agent can follow the link in the same call.

### Gate run gate-20261017-152449 on its own

| Task | Tool | Correct | Median total tokens | Median turns | Median wall time | Median tool tokens |
|---|---|---|---|---|---|---|
| W1 | ghost | 3/3 | 39902 | 2 | 9.6 s | 136 |
| W1 | agent-browser | 3/3 | 60230 | 3 | 12.2 s | 208 |
| W2 | ghost | 3/3 | 40703 | 2 | 10.9 s | 783 |
| W2 | agent-browser | 3/3 | 80145 | 4 | 14.2 s | 141 |
| A1 | ghost | 3/3 | 40259 | 2 | 9.0 s | 100 |
| A1 | no tool | 2/3 | 121872 | 5 | 37.8 s | 2814 |
| N1 | ghost | 3/3 | 39721 | 2 | 8.7 s | 86 |
| N1 | no tool | 3/3 | 61991 | 3 | 15.4 s | 314 |
| N2 | ghost | 3/3 | 39846 | 2 | 8.3 s | 133 |
| N2 | no tool | 3/3 | 64952 | 3 | 18.0 s | 891 |

### Gate run gate-20261017-150207 on its own

| Task | Tool | Correct | Median total tokens | Median turns | Median wall time | Median tool tokens |
|---|---|---|---|---|---|---|
| W1 | ghost | 3/3 | 60228 | 3 | 10.3 s | 162 |
| W1 | agent-browser | 3/3 | 80718 | 4 | 13.0 s | 212 |
| W2 | ghost | 3/3 | 40703 | 2 | 10.5 s | 784 |
| W2 | agent-browser | 3/3 | 79746 | 4 | 13.3 s | 140 |
| A1 | ghost | 3/3 | 40600 | 2 | 10.1 s | 101 |
| A1 | no tool | 2/3 | 99659 | 4 | 37.4 s | 2543 |
| N1 | ghost | 3/3 | 39719 | 2 | 9.3 s | 84 |
| N1 | no tool | 3/3 | 126907 | 6 | 24.4 s | 872 |
| N2 | ghost | 3/3 | 39891 | 2 | 9.5 s | 131 |
| N2 | no tool | 3/3 | 63982 | 3 | 16.3 s | 868 |

## Before this round

These untagged rows were measured with earlier ghost versions. They are kept because they hold the only Playwright MCP runs. Their ghost numbers do not describe the current build. `bench/run.ps1` serves W1's page from a local http server by default, which Playwright needs since it blocks `file://`; the gate runs above open it as a `file://` URL.

| Task | Tool | Correct | Median total tokens | Median turns | Median wall time | Median tool tokens |
|---|---|---|---|---|---|---|
| W1 | ghost | 1/3 | 125786 | 6 | 29.1 s | n/a |
| W1 | agent-browser | 3/3 | 100635 | 5 | 14.2 s | n/a |
| W1 | Playwright MCP | 3/3 | 153273 | 8 | 13.7 s | n/a |
| W2 | ghost | 2/3 | 190857 | 9 | 50.4 s | n/a |
| W2 | agent-browser | 3/3 | 59612 | 3 | 12.3 s | n/a |
| W2 | Playwright MCP | 3/3 | 128504 | 6 | 15.0 s | n/a |
| A1 | ghost | 3/3 | 61740 | 3 | 12.9 s | n/a |
| A1 | no tool | 2/3 | 159208 | 6 | 52.4 s | n/a |
| N1 | ghost | 3/3 | 63328 | 3 | 11.7 s | n/a |
| N1 | no tool | 2/3 | 107633 | 5 | 20.1 s | n/a |
| N2 | ghost | 3/3 | 148892 | 7 | 20.7 s | n/a |
| N2 | no tool | 3/3 | 66206 | 3 | 22.1 s | n/a |

The rows tagged `gate-20261017-144352` are an earlier gate run of this round and are not in any table here. They stay in the data file.

## Limits

- Claude in Chrome was measured on W2 only.
- The Playwright MCP numbers come from the earlier round above, run against an earlier ghost version, on W1 and W2 only. agent-browser's own numbers moved between rounds (W2 went from 59.6k to 79.9k), so treat that column as a rough guide.
- The "no tool" baseline is Haiku with PowerShell and .NET UI Automation. A person writing UI Automation code by hand, or a different model, may score differently.
- Correct means the reply contains the expected text. It does not check how the agent got there: in one N1 run without a tool, Haiku could not open the Explorer window, listed the folder from the file system instead, and still counts as correct.
- 3 runs per round, 6 for the current pairs that ran in both gate runs, is a small sample. The two gate runs differ noticeably on some pairs; compare their tables above.
- Everything ran on one Windows 11 PC with one model.
- The live check that ghost leaves the foreground alone when the user has no window in front (`FG0 live` in `test/web-features.ps1`) has not been run yet.

## Reproduce

Requires Claude Code on PATH, a built `ghost.exe`, and agent-browser for the web reference. `bench/run.ps1` appends to `bench/results.jsonl`, which is not committed; `node bench/summarize.mjs --export` writes `bench/results.public.jsonl` from it without the free text reply, and `node bench/summarize.mjs` prints the tables.

```
powershell -NoProfile -ExecutionPolicy Bypass -File bench/agent-gate.ps1 -Reps 3
node bench/summarize.mjs --export
node bench/summarize.mjs
```

The task list and the set of tags counted as current are at the top of `bench/summarize.mjs`.
