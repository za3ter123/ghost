# ghost

Lets an agent read and drive Windows desktop apps in the background. It never moves the real cursor and never keeps the user's foreground window. UI Automation patterns come first, posted or sent window messages second. When neither can work in the background, it exits 1 with `background unavailable: <why>`.

## How it compares

Claude Haiku ran each task through `claude -p` from a clean context, with the same prompt and a short brief per tool, 3 times per round on one Windows 11 PC. Each cell is the median total tokens for the whole run, with correct runs over all runs in brackets. W1 and W2 are web tasks (a local form, then following a link on example.com), A1 is a WinForms test app, N1 is File Explorer and N2 is Calculator. Claude Code itself costs about 20k tokens per turn, so 2 turns, about 40k, is the lowest any tool can reach.

| Task | ghost | agent-browser | Playwright MCP (earlier round) | Claude in Chrome | no tool |
|---|---|---|---|---|---|
| W1 | 50.2k (6/6) | 70.5k (6/6) | 153.3k (3/3) |  |  |
| W2 | 40.7k (6/6) | 79.9k (6/6) | 128.5k (3/3) | 195.4k (3/3) |  |
| A1 | 40.4k (6/6) |  |  |  | 110.8k (4/6) |
| N1 | 39.7k (6/6) |  |  |  | 106.3k (6/6) |
| N2 | 39.9k (6/6) |  |  |  | 64.6k (6/6) |

"no tool" is Haiku with PowerShell and .NET UI Automation. Claude in Chrome ran on W2 only, and the Playwright MCP runs come from an earlier round. The method, every table, turns, wall time and the limits are in [docs/benchmarks.md](docs/benchmarks.md); `node bench/summarize.mjs` prints the numbers.

## Parts

It is one Rust binary built from `src/`. The only dependency is the official `windows` crate, pinned in `Cargo.toml` and `Cargo.lock`.

- `ghost.exe` is the thin client. It does no COM or UI work. It opens `\\.\pipe\ghost-<USERNAME>`, sends argv, cwd and a session key, prints the reply and exits with the daemon's code. If no daemon answers, it starts `ghost.exe --daemon` hidden and detached and retries for up to 2 s. It only talks to a pipe served by its own exe, with one exception: an older ghost daemon from the same folder (the C# `ghostd.exe`, or a build moved aside as `ghost.old*.exe`). It asks that one to stop and takes the pipe over; if it still holds the pipe after 3 s, the client exits 1 and says so.
- `ghost.exe --daemon` is the resident server. Each target window gets its own worker thread with native COM UI Automation (CUIAutomation8, MTA), cache requests, element maps and the last snapshot. Requests for one window run in order, and a hung app stalls only its own window: `windows`, `use` and `help` never wait on any app. Ref numbers live in the daemon, outside the workers, so a replacement worker keeps them. A separate thread pumps the WinEvent hooks. The pipe is created first-instance-only, with a DACL for the current user, network logons denied and remote clients rejected. The daemon exits after 15 idle minutes.

Build with `powershell -NoProfile -ExecutionPolicy Bypass -File build.ps1`, which prints `BUILD OK`. It builds with a static CRT and the GUI and COM DLLs delay-loaded, so the client path loads only kernel32 and ntdll. A running daemon locks `ghost.exe`, so the script moves the old exe aside, copies the new one in and stops the old daemon. The C# + Go v2 is kept for rollback in `legacy/ghost-v2-cs.zip`.

## Commands

`[win]` is a decimal HWND or a case-insensitive substring of a title or process name. `click`, `read` and `wait` join every word after the window into one name (`ghost read Next page`), so for them a leading word is the window only when it is a live HWND; the other commands take a fixed number of words, and there a title substring counts as `[win]` when there is one word more than the command takes. Leave it out to use the current window, which `launch`, `use` and any command given a window set. Each agent session has its own current window. The client keys it by the nearest non-shell ancestor process, or by `GHOST_SESSION` if set. Without `GHOST_SESSION`, every subagent of one Claude Code session shares that key (the Claude Code process), and so one current window. Agents that run in parallel should set `GHOST_SESSION` or always name the window.

Acting commands (click, type, key, park, show, close, and `run` steps) only act on a window ghost did not launch when `[win]` is named in that command. Through the implicit current window they exit 1 with `not launched by ghost; name the window explicitly`. Reads (tree, read, wait, ocr, shot, windows) are always allowed. Launched windows are recorded in `%LOCALAPPDATA%\ghost\launched.txt` by hwnd, pid and process creation time, so a daemon restart remembers them and a reused pid never matches. Entries are dropped on `close`, when the process exits, and at daemon start.

Inside a `run` step the window is never written: `click Learn more` clicks "Learn more". Double quotes group words, `\"` is a literal quote and `""` is an empty argument: `run 'key "Address and search bar" enter'`. Windows PowerShell 5.1 passes such arguments without escaping the inner quotes; when ghost's parent is powershell.exe the client rebuilds its arguments from the raw command line, so the same line works there.

A target is an `eN` or `oN` ref, or an element name (an exact case-insensitive match first, then a unique substring). A name that isn't there yet is looked up again each time the app reports a UI change, for up to 2 s, so an action right after another rarely needs a `wait`. Not found, the error lists the 3 nearest names with their refs. Names are looked up in the default view; only when nothing there matches is the full tree searched, which can resolve title bar buttons such as Minimize or Close. To close a window, use `ghost close`, which waits for it and reports a dialog that keeps it open. Exit codes: 0 ok, 1 error, 2 usage.

```
windows                                hwnd pid process "title"
launch <exe|uri|shell:AppsFolder\AUMID> [args...]   start unactivated, park, make current; a bare name not on PATH comes from App Paths
use <win>                              set the current window
park | show | close [win]
tree [win] [--find t] [--full] [--all] [--depth N]   --find matches names first, values only if no name does
click [win] <target> [--quiet]         BN_CLICKED > title bar > MSAA default action > invoke > toggle > select > expand > posted click
type [win] <target> <text> [--append] [--quiet]
key [win] [target] <key> [--quiet]     enter tab esc bs del arrows home end pgup f1-12, a, shift+a
read [win] [target]                    text, value or name, 1500 chars at most; bare: the window's text,
                                       edit values and list/grid item names, without title bar, menus,
                                       toolbars, nav tree, header, status and scroll bars, each line once
wait [win] <text> [--gone] [--timeout ms]   `wait 1s` / `wait 500ms` = plain delay (30 s max)
go [win] <url>                         browser: open the URL (see Web pages)
run [win] "<step>" "<step>" ...        one call; steps omit the window; every step is checked before any
                                       runs (a usage error launches nothing); stops at the first failure, but
                                       a last bare "close" still runs and prints its result
                                       a `launch` step sets the window for the steps after it
shot [win] <out.png> [--scale F] [--max px] [--gray]
ocr [win] [target]                     Windows OCR on the capture: o<N> "line" x,y
```

The tree prints one line per element, as `e12 btn Save ="value" [flags]` (a value equal to the name is left out), indented one space per level below the root's children. Names are quoted only when they contain a space or a quote, or start like a value or flag (`e3 btn "Save as"`). Roles are short (btn edit doc text link chk radio combo list li menu mi menubar tab ti tree tvi tbl row hdr slider prog tbar sbar win dlg pane group img split). Flags appear only when they apply: `[on]`/`[off]` for toggles, `[x]` for disabled, `[*]` for focused, `[+]`/`[-]` for expanded or collapsed. The default view drops title bars, scroll bars, unnamed structure, scrolled-away elements and text that repeats its parent's label. Runs of more than 12 same-role siblings show the first 10 plus `… N more li`. `--all` shows everything.

A ref keeps its number for as long as the daemon lives. Numbers are keyed by UIA RuntimeId and never reused. A repeated tree is served from memory until the app fires a UI event.

`click`, `type` and `key` wait until the app has been quiet for 15 ms (150 ms at most), then print what changed since the last look: `+ e9 text x`, `- e4 btn Cancel`, `~ e4 text clicked`. When nothing visible changed they print `no change`, and a window the action closed prints `window closed`, so a successful click, type or key always says something. `close` prints `closed` (`4 closed` inside `run`), so no turn is spent checking. `park`, `show` and `use` print nothing on success, to save tokens; their exit code is the answer. Inside `run`, each line starts with its step number (`2~ e4 text clicked`, `1 no change`). The diff stops at 30 lines. `--quiet` skips it. With `GHOST_DEBUG` set for the daemon, an `ok <method>` line shows how the action was sent.

Success is checked where it can be. A click that toggles, selects, expands or resizes a window reads the state back after the app settles; if nothing changed, it exits 1 with `no effect: the control's state did not change`. `type` reads the text back (for `--append` and WM_CHAR, the end of it) and exits 1 with `no effect: the text did not arrive` when it differs; password fields can't be read and are not checked. `close`, and the title bar's Close, wait up to 1 s for the window to go. Gone prints `closed`. Still there exits 1 with `still open`, any changes, and each dialog the window opened, as `+ dlg "Save changes?" (hwnd 123)`.

A disabled control, or any control in a window that is disabled or has a standard modal dialog (#32770) open in front of it, is refused before anything is sent, with exit 1: `disabled: <name>`, or `disabled: <window>, dialog open: dlg "Confirm" (hwnd 123)` when there is a dialog to name. That covers click, type, key and close, as for a user. `type` into something that isn't editable (no writable value, and not an edit or document) exits 1 with `not editable: <name>`.

## Web pages

`launch` given msedge, chrome, brave or opera and no `--user-data-dir` adds a fresh profile `%LOCALAPPDATA%\ghost\profiles\ghost-<time>`, `--no-first-run`, `--no-default-browser-check` and `--force-renderer-accessibility`, so it always starts its own instance and never hands the URL to the owner's browser. Edge also gets `msImplicitSignin` disabled: without it, a new profile signs itself into the Windows user's Microsoft account (seen in its Local State). All `--disable-features` lists, the caller's included, are merged into one, since Chromium keeps only the last. Without LOCALAPPDATA such a launch is refused. Only `ghost-<digits>` folders whose browser has exited (their `lockfile` is gone or removable) are deleted, ten minutes on. With a URL, launch waits until the page has content and UIA has been quiet for 500 ms (10 s at most) and prints `hwnd pid msedge`, `page "title"` (plus the URL when it differs from the one asked for) and the page: link URLs dropped, repeated names shown once, 1600 chars at most, the cap stated when cut.

In a browser window (msedge, chrome, brave, opera, vivaldi):
- `tree` shows the address bar and the page document; `tree --full` the whole window. Content below the fold is included.
- Names resolve in the page first, then the browser's own controls (Back, tabs).
- `wait <text>` looks only inside the page, never at tab or window titles, which show a page's title before its content. Its timeout names the page.
- After `click` or `key`, a changed title or URL is a navigation: ghost waits until the new page settles (content, then 500 ms of UIA quiet, 10 s at most) and prints `nav "title" url` and the new page, 700 chars at most, instead of a diff. A link click watches 3 s for it to start (300 ms for a link to this page), a button 500 ms or until the page changes in place, Enter 3 s. Other diffs cover only the address bar and the page, and focus moves aren't reported.
- `read` with no target prints `page "title" url` and the page text, 1500 chars at most; `read title` and `read url` print just those.
- `go <url>` opens the URL. UIA SetValue on the address bar activates the browser (measured: Edge took the foreground, and the guard couldn't always take it back), so `go` starts a new background instance of the same browser at the URL, like `launch`, prints `hwnd pid msedge` and `nav "title" url` plus the page, and closes the old window. History and cookies don't carry over, and it works only in a browser ghost gave its own profile. `type` or `key` into the browser's own bars are refused and point to `go`.
- An empty page document is reported as not exposed, with how to recover.

```
ghost run "launch msedge https://example.com" "click Learn more" "close"
-> 123 456 msedge / page "Example Domain" / e28 link "Learn more" / 2 nav "Example Domains" https://www.iana.org/... / 3 closed
```

## Example

```
ghost launch fixture.exe tok        -> 590 1234 ghost-fixture "tok"
ghost run "type Body hi" "click Settings"
                                    -> 1~ e2 edit Body ="hi" / 2~ e8 text clicked
ghost read Body                     -> hi
ghost close
```

`ghost`, `ghost help` and `--help` print a 12-line summary that ends with the path of this README.

```
ghost run "launch C:\path\app.exe" "wait Greet" "type Name bob" "click Greet"
```

## Background safety

- A new top-level window is parked the moment it shows (WinEvent hook on EVENT_OBJECT_SHOW) only if its owner chain reaches a window ghost launched or adopted (a dialog, a popup), or it comes from a launched process (same pid and creation time) within 30 s of the launch (a splash handing over to the main window). It is then recorded as ghost's too. A window a long-running launched app opens later (a new browser or Office window) is left alone and not adopted; name it to act on it. Next to a parked owner a popup is centred on the owner, otherwise parked on its own. `show <hwnd>` brings one up.
- `launch` uses SW_SHOWNOACTIVATE and parks the window at once, off every monitor and at the bottom of the z-order, so Alt+Tab and closing windows don't hand it focus. Parked windows are never minimized, so they keep rendering and UIA keeps working.
- The focus guard is a WinEvent hook on foreground changes. Suppose a window becomes foreground that belongs to a process ghost acted on in the last second, or is a parked window. The daemon then gives the foreground straight back to the user's last window (AttachThreadInput plus SetForegroundWindow). It never undoes a switch the user made to an unrelated window. `GHOST_DEBUG=<file>` logs the reaction time.
- Once the user has been idle past ForegroundLockTimeout (200 s here), any process may take the foreground, and Chromium did so on every UIA action and navigation. So while ghost acts, and 1.5 s after, the daemon holds LockSetForegroundWindow, as Windows does while the user types; Windows grants it only in that idle state and drops it on the user's next Alt or click. One hold lasts 2 s at most, then the lock is free for 300 ms, so a slow launch or a long `run` never locks the owner out for longer.
- A new browser process can still activate itself once, on its first page action, past the lock; the guard takes it back within one 16 ms system tick (measured with a WinEvent watcher). Making it spend that at launch instead was tried and dropped: every launch and `go` then dipped the foreground to "no window" for 15 to 60 ms.
- The guard takes the foreground back from any parked window whenever it gets it, not only while ghost acts, and from windows of a launch still in progress (Explorer shows its new window active before launch finds it). When the user had no window in front (foreground 0), that window is gone, or it is the taskbar, the foreground goes to ghost's sink: a 1x1 tool window off every monitor, owned by the daemon, with no title (not on the taskbar, in Alt+Tab or in `ghost windows`), that discards every key and close request. Keys then go nowhere, as they did at foreground 0. Only if the sink can't be activated does it go to the taskbar, and that is logged. An action whose foreground was put back within 200 ms succeeds. A parked window is ghost's only while its park file (`%LOCALAPPDATA%\ghost\park-<hwnd>.txt`) names that window's process by pid and creation time; stale files are dropped at daemon start, so a reused hwnd is never pulled back. `ghost debug-fg0 <win>` (used by test/web-features.ps1) prints the guard's decision for a launched window taking the foreground with the prior foreground treated as 0, without moving the foreground; the live foreground-0 run there needs nothing of the user's in front, else it prints `FG0 live: untested (owner active)` (a separate manual gate). Only that decision is tested so far; the live run with no window in front is still pending, so the sink path itself is unverified.
- A command that fails leaves the current window as it was, unless it launched a window.
- UIA Invoke, Toggle, Select, Expand and SetValue run with the target's top-level window disabled (EnableWindow false, always re-enabled right after), so it can't be activated during the call. Windows that were already disabled, or whose app doesn't answer WM_NULL, are left alone. Windows' own proxies for Win32 controls refuse a disabled window, so those calls retry unshielded and the guard is the backstop. Each disabled window is journaled in `%LOCALAPPDATA%\ghost\shielded.txt` first. Daemon start, `daemon-stop` and idle exit re-enable whatever is listed there, after waiting up to 2 s for calls in flight, so a daemon that dies mid-call can't leave an app disabled. Each re-enable gets 500 ms on its own thread (EnableWindow waits for the app); a hung app stays listed for the next start. The shield always puts back the exact state from before the call, whatever the call opened: a popup must never leave the owner disabled. A modal dialog of a parked window is off screen too, so the user can't click past it.
- Elements served through Windows' MSAA proxy (WinForms, classic Win32) are clicked with their MSAA default action. The proxy's own Invoke and Toggle moved the foreground onto the control itself, where the guard could not take it back.
- Title bar buttons: minimize uses ShowWindow(SW_SHOWMINNOACTIVE) and restore uses SW_SHOWNOACTIVATE. Maximize uses SW_SHOWMAXIMIZED with the window disabled, since there's no non-activating maximize. Close posts SC_CLOSE. A posted SC_MAXIMIZE still let the app take the foreground for about 15 ms. The system menu can't be opened without activation, so it is refused with `background unavailable: system menu`.
- `type` on a native edit control uses WM_SETTEXT, then EM_SETSEL to put the caret at the end. It never uses the UIA proxy's SetValue there, which focuses the window and can activate it.
- Native push buttons (UIA control type Button) get a WM_COMMAND/BN_CLICKED posted to their parent, which can't steal focus. Owner-drawn check boxes and radios are BUTTON-class too but ignore it, so they never take this path.
- Before any action, the target gets WM_NULL with a 200 ms timeout. An app that doesn't answer gets nothing and the caller hears `busy: <app> not responding, nothing sent` right away.
- Every request runs under a deadline: 10 s, longer for `wait`, `launch` and `run`. Once it passes, nothing more is sent to the app, and requests queued behind it are dropped. If an app stops answering after an action was already sent, the reply is `busy: <app> stopped responding mid-action; it may still apply`. A read that times out gets `busy: <app> not responding`. The window's worker is then replaced, and other windows never wait.

## Limits

- ctrl, alt and win combos can't be posted, because the app reads the real keyboard state. On classic Win32 apps with a menu, `key ctrl+s` finds the item whose accelerator text matches and posts its WM_COMMAND. Everywhere else it exits 1 and posts nothing. Shift with a named key (`shift+tab`) is refused too.
- Title bar buttons are matched by their English names (Minimize, Maximize, Restore, Close). In another UI language they are refused, not guessed. Minimizing a parked window is refused, because a minimized window stops rendering; it is off screen already. Maximize puts the window on a monitor, behind the user's windows.
- Invoke and the other UIA patterns can still let an app activate itself for about 15 to 30 ms before the guard puts focus back. Keys the user types in that gap go to the hidden window. WPF `Focus()` self-steals aren't blocked; the guard undoes them.
- `type` without a ValuePattern needs a native edit HWND. Otherwise only `--append` works, and it types at the caret.
- Native UIA reports a Win32 multiline edit as `edit` with a value, where the old managed client showed `doc`. Its value appears in the tree and in diffs.
- `launch` only adopts windows from the launched process, its descendants, or a same-named process started after launch. A window handed to an instance that was already running isn't adopted. A launch that times out leaves the child running; the error prints its pid.
- `go` and launch's profile cleanup apply to ghost's own throwaway profiles only; a browser started with `--user-data-dir` can't `go`. `go` information lives in the daemon, so after a daemon restart `go` asks for a new launch.
- Chromium: ghost launches browsers with `--force-renderer-accessibility` and with the occlusion and backgrounding switches turned off. On a browser that is already open, the first `tree` wakes the renderer and re-polls for up to 2 s, printing `…page loading` if the page tree hasn't arrived. On a live browser window, UIA failed with "Unrecognized error" even one level at a time below the browser client. ghost falls back to a per-level walk and shows only the browser's own controls.
- `tree --depth N` only limits what is printed. Walks stop at depth 25, at 5000 elements, and at a RuntimeId that repeats on its own branch.
- The first walk of a window with a big subtree is slow; the desktop's Program Manager takes about 4 s here. Repeats come from the event-driven cache until the app fires an event.
- OCR covers the client area at 3x (2x dropped a button label beside a check box) and retries once if it reads nothing. Small UI text can still be missed.
- Elevated windows can't be driven from a non-elevated daemon (UIPI).
- Packaged (UWP) apps: `launch calc.exe`, `launch shell:AppsFolder\Microsoft.WindowsCalculator_8wekyb3d8bbwe!App` and protocol URIs such as `ms-settings:about` go through the shell. The window is an ApplicationFrameHost frame; ghost adopts a new one when the app in it (its CoreWindow's process) is the pid the shell reported or started after the launch, and records that app's pid and start time. Parked, Calculator kept rendering, answered UIA, and did not suspend (checked after 25 s idle). From the daemon, ActivateApplication answers "Access is denied", so AUMIDs fall back to ShellExecuteEx; an app that was already running (often Settings, which is single-instance and may be prelaunched) reuses its existing window, and that window is not adopted: launch times out and says so.
