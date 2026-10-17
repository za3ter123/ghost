# ghost v2 research: steal-the-best techniques

Scope: how other background UI drivers avoid focus theft, deliver clicks/keys to background
windows, handle Chromium a11y, and keep snapshots token-cheap. Sources are the
upstream repos (cloned read-only for this study) and small throwaway tests. Each item marked
VERIFIED (saw the code / ran it) or CLAIMED (docs only).

Repos read: trycua/cua (`libs/cua-driver/rust/crates/platform-windows`), vercel-labs/agent-browser
(`cli/src/native`), microsoft/playwright (`packages/injected/src/ariaSnapshot*.ts`, fetched via gh),
microsoft/playwright-mcp, CursorTouch/Windows-MCP (`src/windows_mcp/tree`), mediar-ai/terminator
(`crates/terminator/src/platforms/windows`), browser-use (`browser_use/dom/serializer`, fetched).

---

## A. Empirical findings (my own throwaway tests)

**A1. LockSetForegroundWindow from a background process does NOT work. VERIFIED.**
`tests/locktest.cs`. `LockSetForegroundWindow(LSFW_LOCK)` from a non-foreground process returns
false, GetLastError=5 (ACCESS_DENIED), including when attached to the foreground thread. Only the
current foreground process may set the lock. So a resident ghost daemon cannot pre-lock the
foreground to stop a target self-activating. Do not build on it.

**A2. What actually stops a target stealing foreground. VERIFIED.** Same test, fresh victim window
per mode, victim calls `SetForegroundWindow(self)`:
- A *plain* `SetForegroundWindow` from an app whose process holds the "foreground grant" (fresh
  process, or just received input) succeeds — this is the steal ghost sees.
- The grant is one-shot: the victim's *second* `SetForegroundWindow` in the same process already
  fails (err 1008/no grant) once the grant is spent.
- `EnableWindow(target,false)` and `WS_EX_NOACTIVATE` on the target both block an *attach*-style
  SFW but NOT the first plain-grant SFW (err still returns true, window still came foreground in my
  WinForms victim). Matches cua's own note that NOACTIVATE/EnableWindow are not categorical against
  a renderer's async self-SFW.
- Restoring the user's window with AttachThreadInput+SetForegroundWindow worked every time in my
  tests (`Sfw(user, attach:true)` → true). This is the reliable recovery path and is what ghost
  already does.

**A3. UIA InvokePattern DOES ride through to the target's self-activation. VERIFIED.**
`tests/fgtest2.cs`: a UIA `InvokePattern.Invoke` on a button whose handler calls
`SetForegroundWindow(self)` let the target take foreground in 11–116 ms (11–18 ms in-process,
~110 ms cross-process), in every variant including with EnableWindow(false) and WS_EX_NOACTIVATE
pre-applied. So Invoke is not safe by itself — the steal is the *app's* SFW running on the grant
the Invoke delivered, and the ex-styles don't stop it. This reproduces ghost's 15–32 ms steal.
- Compare: posting/sending `BM_CLICK` (WM_COMMAND BN_CLICKED, 0xF5) to the button's native HWND did
  NOT steal foreground (target took fg=false, 300+ ms) — the message runs the click handler but the
  app's SFW silently no-ops because the posting process holds no grant. This is why ghost's native
  push-button path (BN_CLICKED to parent) is already steal-free and should stay the preferred route.

**A4. Background/minimized Chromium exposes only chrome to UIA; a normal window builds the DOM.
VERIFIED.** `tests/chrome_a11y.cs`, my own throwaway msedge + temp profile (never the user's):
- Launched **minimized**: UIA tree stays ~37 nodes (toolbar/omnibox), Document node present but
  empty, forever — even with `--force-renderer-accessibility` and even with
  `--disable-features=CalculateNativeWinOcclusion --disable-backgrounding-occluded-windows
  --disable-renderer-backgrounding`. Minimize suppresses the renderer harder than occlusion does.
- Launched **normal** (then it can be parked offscreen like ghost does): tree jumps 50 → ~1161
  nodes at ~1.75 s (page-load + a11y build) and stabilises. Confirmed with and without the flag.
- Key mechanism: simply running `AutomationElement.FromHandle(top).FindAll(Subtree,...)` from the
  UIA client is itself what makes Chromium build the renderer a11y tree — I did not need an explicit
  WM_GETOBJECT; the client query is the wake. `--force-renderer-accessibility` only makes it eager
  at launch so the first snapshot isn't empty.
- Implication for ghost: keep parking windows with SW_SHOWNOACTIVATE (normal), never minimized. For
  a browser ghost launches itself, add the three anti-throttling flags + optionally
  `--force-renderer-accessibility`. For an already-open browser, the first `tree` will be
  chrome-only; re-run after ~1–2 s, or cache and poll until Document gets children.
- Note: `ghost.exe tree <hwnd>` on a live browser window returned "UIA could not read window:
  Unrecognized error" (ElementFromHandle on that top-level failed) — worth a look, but the DOM-wake
  timing above is the substantive answer.

---

## B. Ranked techniques to steal

### 1. EnableWindow shield for UWP/XAML/Chromium UIA activation. VERIFIED (saw code). High win.
cua `platform-windows/src/uia/fg_bypass.rs:9-128`. XAML/WinUI/UWP hosts (and Chromium content
windows) call `SetForegroundWindow(self)` inside their Invoke/Expand/Toggle/SelectionItem.Select
handler. cua wraps the pattern call in `EnableWindow(host,false) / call / EnableWindow(host,true)`
(RAII guard, walks to GA_ROOT). A *disabled* top-level cannot become foreground, but the UIA call
still lands because it is delivered over the kernel accessibility channel, not the input queue that
EnableWindow gates. Their measured result: UWP Calculator Invoke dropped the user's window in 91% of
samples baseline → 0/507 z-drops with the shield; Chromium 7/8 → 0.
Gate it on `is_xaml_host_hwnd || is_chromium_target_window` (class names
`ApplicationFrameWindow`, `WinUIDesktopWin32WindowClass`, `Windows.UI.Core.CoreWindow`,
`Microsoft.UI.Content.DesktopChildSiteBridge`, `Chrome_WidgetWin_*`; or exe basename
notepad/calc/etc). No-op for classic Win32 (they don't self-foreground).
Apply in C#: `EnableWindow(root,false)` before `InvokePattern.Invoke()` etc, restore in a finally.
Expected win: removes most of ghost's 15–32 ms steal on exactly the paths that cause it.
CAVEAT (VERIFIED by cua's own doc, fg_bypass.rs:86-95): WPF Buttons/TextBoxes call
`UIElement.Focus()` → SetForegroundWindow, which EnableWindow does NOT gate. My A3 test confirms the
ex-style shields don't stop a grant-backed SFW either. The only full fix cua has for WPF is a
UIAccess-manifested worker (see item 6).

### 2. Prefer the message-level activation that carries no foreground grant. VERIFIED (A3).
Route order for "click": native BN_CLICKED (WM_COMMAND 0xF5 to parent) → it runs the handler but
cannot self-foreground from a non-grant process (A3). Keep this ABOVE UIA Invoke for native push
buttons, which ghost already does (`ghost.cs` ~line 960). Only fall to Invoke (under the item-1
shield) when there is no native button HWND.

### 3. Synthetic pointer injection for Chromium/Electron/GTK/canvas, no SetForegroundWindow, no
cursor move. VERIFIED (saw code). cua `platform-windows/src/input/inject.rs`.
Chromium/Electron/WPF content drop posted `WM_*BUTTON` (they read the *system* input queue), so cua
uses `CreateSyntheticPointerDevice(PT_PEN|PT_TOUCH)` + `InjectSyntheticPointerInput` routed by screen
coordinate (`inject.rs:237-370` pen taps, `:592-687` touch drag). Kernel path
`NtUserInjectMouseInput/InjectTouchInput` gates on a per-process injection-enable, NOT on the target
being foreground. Guards: `NoActivateGuard` (WS_EX_NOACTIVATE) during the tap + re-assert the user's
foreground twice with a 12 ms gap after (`inject.rs:348-368`). Pen is an absolute *cursor* device so
it moves the user pointer — they use a **persistent** touch digitizer (`TOUCH_DEV`, created once,
never destroyed, `inject.rs:577-687`) so WPF's WISP stylus stack binds to it and promotes to mouse
*internally* without moving the system cursor; for the legacy promote-to-mouse case they snap the
cursor back with `SetCursorPos` twice. If the target is occluded at the point they bail
`background_unavailable` rather than raise (`target_visible_at_point`).
Apply in C#: P/Invoke `CreateSyntheticPointerDevice`/`InjectSyntheticPointerInput`. This is ghost's
missing path for Chromium/canvas content where PostMessage clicks silently no-op.

### 4. Keyboard: three honest tiers, and why ctrl+s needs the system queue. VERIFIED (saw code).
cua `platform-windows/src/input/keyboard.rs`.
- Text/keys to the **focused descendant** HWND, not the top-level: `focused_descendant()`
  (`:145-209`) enumerates child-thread ids via EnumChildWindows + GetGUIThreadInfo and picks the
  deepest focused HWND — required for WebView2/Electron/Scintilla/RichEdit where the top-level
  WindowProc won't forward WM_CHAR (`:86-99`, `:202-206`).
- `post_char` = WM_CHAR; Enter = a real WM_KEYDOWN/UP(VK_RETURN) pair with scancode in lParam
  (`:115-135`), because rich controls drop WM_CHAR(0x0A/0x0D). lParam bits: repeat=1, scan<<16,
  extended<<24, and `(1<<30)|(1<<31)` for key-up.
- `post_key` with modifiers posts WM_KEYDOWN/UP for each modifier then the key, `WM_SYSKEYDOWN/UP`
  when Alt is held (`:223-227`).
- **Why ctrl+s via PostMessage fails (this is ghost's documented limit):** PostMessage(WM_KEYDOWN,
  VK_CONTROL) does NOT update the system-wide modifier state that `GetKeyState`/`TranslateAccelerator`
  read, so accelerators never fire — the 's' arrives as text (`keyboard.rs:271-290`, cua's own
  comment). cua's only working path for accelerators is `send_key_synthesized` = SendInput on the
  **system input queue** after a confirmed foreground swap (`:291-345`), i.e. it accepts a brief
  focus cost and reports it. There is no PostMessage trick for real accelerators.
  Practical alternative for ghost: drive the menu item via the UIA tree (File→Save) under the item-1
  shield instead of sending ctrl+s — matches ghost's README advice. CLAIMED that this is always
  equivalent; VERIFIED that the modifier-state problem is real (cua code + Windows docs).

### 5. Capture: PrintWindow → WGC fallback on black, with occlusion honesty. VERIFIED (saw code).
cua `capture.rs` + `wgc.rs`. `PrintWindow(hwnd, dc, PW_RENDERFULLCONTENT=2)` for classic windows;
detect all-black result (`is_mostly_black_bgra`, >99.5% black) → fall back to
Windows.Graphics.Capture (`Direct3D11CaptureFramePool` + `GraphicsCaptureItem`,
`SetIsBorderRequired(false)`, `SetIsCursorCaptureEnabled(false)`), which reads DWM's composited frame
even when occluded. Crop to `DWMWA_EXTENDED_FRAME_BOUNDS` to drop the invisible shadow margin that
otherwise leaves black trim. WGC cannot capture minimized windows (no frame) — return AX-only.
ghost already does PrintWindow(PW_RENDERFULLCONTENT); add the WGC fallback for UWP/DirectComposition.

### 6. UIAccess worker for the cases nothing else fixes. CLAIMED (saw the worker, not its effect).
cua `cua-driver-uia/src/main.rs` + fg_bypass.rs:91-95: a separate `cua-driver-uia.exe` with a
`uiAccess=true` manifest, launched by the daemon, reachable over its own named pipe
`\\.\pipe\cua-driver-uia`. A UIAccess process can set foreground / restore it even against the
foreground lock, which is the only mitigation for WPF `UIElement.Focus()` self-steal and for
restoring the user's window when a plain process can't. Cost: the binary must be signed and in a
trusted location (Program Files) to get uiAccess — heavy for ghost, consider only if WPF steal
proves unacceptable.

---

## C. Token-cheap snapshots

### 7. Interactive-only tree with stable refs. VERIFIED (saw code). agent-browser
`cli/src/native/snapshot.rs`. Two knobs: `interactive` (skip any node without a ref, still recurse
children, `:1260-1266`) and `compact` (keep only lines containing `ref=` or `: ` plus their
ancestors, `:1355-1392`). Refs are `e<N>`; interactivity decided by `INTERACTIVE_ROLES` (~22 roles,
`:107-126`) plus CONTENT_ROLES and a JS scan for cursor:pointer/onclick elements that have no ARIA
role (`find_cursor_interactive_elements`, `:772-830`). Output line:
`- button "Name" [checked=false, ref=e12]`. ghost's existing `e12 Button "Settings" [invoke]` is the
same idea; add an interactive-only mode and a compact filter.

### 8. Durable refs keyed by element identity → cheap diffs. VERIFIED. agent-browser
`snapshot.rs:505-560` + `element.rs`. A ref is remembered per (session, frame, backendNodeId) and
re-emitted across snapshots (`durable_ref`/`remember_durable_ref`), so the *same* element keeps the
same `e12` between actions. Diff is then plain unified text diff over the two renderings
(`diff.rs:103-145`, `similar::TextDiff`, context_radius 3, fast-path identical). Playwright does the
equivalent in-page: `computeAriaRef` stashes `element._ariaRef = {role,name,ref}` and reuses it
unless role or name changed (`ariaSnapshot.ts:224-237`). ghost resolves refs by UIA RuntimeId
already — persist (RuntimeId → e<N>) across snapshots so the agent can diff and so a stale ref maps
to the same element. RuntimeId is the Windows analogue of backendNodeId; Windows-MCP confirms it is
the only exact identity UIA offers (`tree/cache_utils.py:154-161`).

### 9. Post-build distillation pass. VERIFIED (saw code). Playwright
`ariaSnapshotDistiller.ts`. After building the full tree, one traversal of visitor plugins:
drop leaf generics, unwrap single-child generics, merge/trim text tokens, drop decorative images,
and drop a node's accessible name when its content already appears (`removeRedundantNames`). This is
where most token savings beyond interactive-only come from. ghost could port the "unwrap
single-child Pane/Group" and "drop name that repeats child text" rules cheaply.

### 10. role:name nth de-dup for addressable, stable names. VERIFIED. agent-browser
`RoleNameTracker` (`snapshot.rs:290-320`) tracks `role:name` counts; only duplicated pairs get an
`nth` so a ref can be re-resolved by (role,name,nth) if the id is gone (`find_node_id_by_role_name`).
Cheap fallback that avoids coordinates.

### 11. One bulk CacheRequest, not per-property COM calls. VERIFIED (saw code). Everyone does this.
cua `uia/mod.rs:672-701` builds a CacheRequest with 10 properties + 8 patterns, sets
`TreeScope_Subtree` + ControlView filter, then `ElementFromHandle` + `BuildUpdatedCache` once and
walks `GetCachedChildren()` (cua's comment: per-property cross-process calls make Chrome's 5000-node
tree take >4 s). Windows-MCP `tree/cache_utils.py:126-173` and terminator
`tree_builder.rs:388-470` (`find_first_build_cache(Subtree)` then cached accessors) do the same.
ghost already uses `cr.TreeScope = Subtree; cr.TreeFilter = ControlViewCondition` — confirmed the
right design; make sure ghost reads only *cached* properties in the walk (no live property hits).

### 12. Pruning/safety heuristics worth copying. VERIFIED (saw code).
- Element/depth budget to cap runaway trees: cua `DEFAULT_MAX_DEPTH=25`, `MAX_TOTAL_ELEMENTS=5000`
  (`uia/mod.rs:39-49`); Windows-MCP has an element budget; terminator yields every N + batches.
- Cyclic-tree guard: Windows-MCP tracks ancestor RuntimeIds and cuts a branch whose RuntimeId repeats
  an ancestor (`tree/service.py` ~515-535) — a provider (expanded Edge <select>) can loop forever and
  budgets don't catch it (no output = no budget spent). Add this; it is a real hang.
- SAL/VCL (LibreOffice): UIA `BuildUpdatedCache(Subtree)` *hangs*; cua detects `SAL*` class and routes
  to MSAA via oleacc (`uia/mod.rs:132-159`). Firefox web DOM is MSAA/IA2 only — Windows-MCP uses
  `AccessibleObjectFromWindow(OBJID_CLIENT=0xFFFFFFFC)` (`tree/ia2.py`). ghost may want an MSAA
  fallback for these two families.
- browser-use DOM serializer extras (CLAIMED, different domain but portable ideas): bounding-box
  containment filter (drop children ≥99% inside a propagating clickable parent,
  `serializer.py:772+`), paint-order filtering, and marking new nodes with a `*` prefix
  (`is_new`, `:1032`) so the agent sees what changed without a diff.

---

## D. What NOT to do
- Don't rely on LockSetForegroundWindow from the daemon (A1).
- Don't rely on WS_EX_NOACTIVATE / EnableWindow to stop a grant-backed self-SFW (A2/A3); they only
  stop attach-style SFW. Pair the EnableWindow shield (item 1) with a reliable attach-restore of the
  user's window, and accept the brief WPF steal unless you ship a UIAccess worker.
- Don't launch a background browser minimized — the renderer never builds its a11y tree (A4). Park
  it normal + offscreen.
- Don't expect a PostMessage path for real accelerators (ctrl+s); drive the menu item instead (item 4).
