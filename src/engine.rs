//! Command engine. Lives on the daemon's worker thread (MTA), owns UIA and every per-window state.
use crate::daemon::{self, dbg, now_ms};
use crate::win::{self, hv, hwnd};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Instant;
use windows::core::{BSTR, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, SAFEARRAY};
use windows::Win32::System::Threading::*;
use windows::Win32::System::Variant::{VariantClear, VARIANT, VT_ARRAY, VT_BOOL, VT_BSTR, VT_I4};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled};
use windows::Win32::UI::WindowsAndMessaging::*;

pub struct Reply {
    pub code: i32,
    pub out: String,
    pub err: String,
    pub cur: isize,
}

pub enum Fail {
    Usage(String),
    Ghost(String),
}
pub type R<T> = Result<T, Fail>;
const UIA_E_ELEMENTNOTENABLED: u32 = 0x8004_0200;
const UIA_E_ELEMENTNOTAVAILABLE: u32 = 0x8004_0201;

impl From<windows::core::Error> for Fail {
    fn from(e: windows::core::Error) -> Self {
        if e.code().0 as u32 == UIA_E_ELEMENTNOTAVAILABLE {
            return Fail::Ghost("element is gone (UI changed); run tree again".into());
        }
        let m = e.message();
        if m.trim().is_empty() {
            return Fail::Ghost(format!("error: HRESULT 0x{:08X}", e.code().0 as u32));
        }
        Fail::Ghost(format!("error: {m}"))
    }
}
fn ghost<T>(m: impl Into<String>) -> R<T> {
    Err(Fail::Ghost(m.into()))
}
fn usage<T>(m: impl Into<String>) -> R<T> {
    Err(Fail::Usage(m.into()))
}

/// Per request.
pub struct Ctx {
    pub out: String,
    pub err: String,
    pub cur: isize,
    pub cwd: String,
    pub implicit_run: bool,
    step: Option<usize>, // inside `run`: diff lines get this step number
    launched: bool,      // this request launched the current window
}
impl Ctx {
    fn line(&mut self, s: &str) {
        self.out.push_str(s);
        self.out.push('\n');
    }
}

#[derive(Clone)]
pub struct Node {
    pub n: u32,
    pub depth: u32,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub flags: String,
    pub more: Option<String>,
    pub hidden: bool,
    pub el: Option<IUIAutomationElement>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// RuntimeId -> ref number per window, never reused. Kept in the daemon, outside any worker, so a fresh
/// worker after a timeout keeps numbering where the old one stopped.
#[derive(Default)]
struct Refs {
    ids: HashMap<Vec<i32>, u32>,
    rids: HashMap<u32, Vec<i32>>,
    next: u32,
}
static REFS: LazyLock<Mutex<HashMap<isize, Refs>>> = LazyLock::new(Default::default);

fn ref_of(h: isize, rid: &[i32]) -> u32 {
    let mut m = lock(&REFS);
    if !m.contains_key(&h) && m.len() >= 64 {
        m.retain(|&k, _| win::is_window(hwnd(k))); // bounded: forget windows that are gone
    }
    let r = m.entry(h).or_default();
    if let Some(&n) = r.ids.get(rid) {
        return n;
    }
    r.next += 1;
    let n = r.next;
    r.ids.insert(rid.to_vec(), n);
    r.rids.insert(n, rid.to_vec());
    n
}
fn rid_of(h: isize, n: u32) -> Option<Vec<i32>> {
    lock(&REFS).get(&h).and_then(|r| r.rids.get(&n).cloned())
}

/// Windows ghost launched or adopted, kept in %LOCALAPPDATA%\ghost\launched.txt. A process is identified by pid
/// AND creation time, so a reused pid never matches; `at` is when the launch happened (FILETIME).
#[derive(Clone, Copy)]
struct Own {
    pid: u32,
    start: u64,
    at: u64,
}
static LAUNCHED: LazyLock<Mutex<HashMap<isize, Own>>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    if let Ok(s) = std::fs::read_to_string(win::state_dir().join("launched.txt")) {
        for l in s.lines() {
            let p: Vec<u64> = l.split(' ').filter_map(|x| x.trim().parse().ok()).collect();
            if p.len() == 4 {
                m.insert(p[0] as isize, Own { pid: p[1] as u32, start: p[2], at: p[3] });
            }
        }
    }
    m.retain(|&h, e| alive(hwnd(h), e)); // daemon start: drop windows and processes that are gone
    Mutex::new(m)
});

/// Creation time of a process (FILETIME), the part of its identity a reused pid can't fake.
pub fn proc_start(pid: u32) -> Option<u64> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let (mut c, mut x, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let ok = GetProcessTimes(h, &mut c, &mut x, &mut k, &mut u).is_ok();
        let _ = CloseHandle(h);
        ok.then(|| ((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64)
    }
}
fn alive(h: HWND, e: &Own) -> bool {
    win::is_window(h) && win::app_pid(h) == e.pid && proc_start(e.pid) == Some(e.start)
}
fn save_launched(l: &HashMap<isize, Own>) {
    let s: String = l.iter().map(|(k, e)| format!("{k} {} {} {}\n", e.pid, e.start, e.at)).collect();
    let _ = std::fs::write(win::state_dir().join("launched.txt"), s);
}
fn forget(h: HWND) {
    let mut l = lock(&LAUNCHED);
    if l.remove(&hv(h)).is_some() {
        save_launched(&l);
    }
}

#[derive(Default)]
pub struct WinState {
    pub els: HashMap<u32, IUIAutomationElement>,
    pub last: Option<Rc<Vec<Node>>>, // what the caller last saw, for diffs
    pub ocr: Vec<(i32, i32)>,        // oN -> point in window coords
    primed: bool,
    chromium: bool,
    pub browser: bool, // a Chromium browser (not an Electron app): page-scoped views apply
    cache: Option<Rc<Vec<Node>>>, // reused until the app fires a UI event
    cache_at: u64,
    cache_end: u64,
    cache_all: bool,
}

pub struct Engine {
    pub uia: Option<IUIAutomation>,
    tree_cr: Option<IUIAutomationCacheRequest>,
    level_cr: Option<IUIAutomationCacheRequest>,
    pub states: HashMap<isize, WinState>,
    pub ocr_engine: Option<windows::Media::Ocr::OcrEngine>,
    deadline: Instant, // the caller's; past it nothing more is sent to the app
}

pub struct Args {
    pub win: HWND,
    pub explicit: bool,
    pub pos: Vec<String>,
    pub opt: HashMap<String, String>,
    pub flags: HashSet<String>,
}
const VALUE_OPTS: [&str; 5] = ["--depth", "--find", "--timeout", "--scale", "--max"];
const STEP_VERBS: [&str; 14] = ["tree", "click", "type", "key", "read", "wait", "go", "shot", "ocr", "park", "show", "close", "use", "launch"];
/// `max_n` for click, read and wait: every word after the window is one name.
const JOIN: usize = usize::MAX;

/// The real forms, with the window only outside `run` steps, and how to write a name with spaces.
fn usage_line(cmd: &str, step: bool) -> String {
    let w = if step { "" } else { "[win] " };
    let f = match cmd {
        "click" => format!("click {w}<name|eN|oN>"),
        "read" => format!("read {w}[name|eN] (bare: the whole window or page), in a browser also: read title | read url"),
        "wait" => format!("wait {w}<text> [--gone] [--timeout ms]"),
        "type" => format!("type {w}<name|eN> <text> [--append]"),
        "key" => format!("key {w}[name|eN] <key>"),
        "go" => format!("go {w}<url>"),
        "shot" => format!("shot {w}<out.png> [--scale F] [--max px] [--gray]"),
        "ocr" => format!("ocr {w}[name|eN]"),
        c => format!("{c} {w}"),
    };
    format!("usage: {f}; names with spaces: click Learn more, type \"Search box\" hi")
}

pub const HELP: &str = r#"ghost: drive Windows apps in the background (no cursor moves, no focus steals)
windows | launch <exe|uri|shell:AppsFolder\AUMID> [args] | use <win> | park|show|close [win]
tree [win] [--find t] [--all]      e<N> role name ="value" [flags]
click [win] <eN|oN|name>           type [win] <target> <text> [--append]
read [win] [eN|name]               bare: the window's text, values and item names
key [win] [target] <key>           wait [win] <text> [--gone] [--timeout ms]
shot [win] <out.png> [--scale F]   ocr [win] [target]
run [win] "<step>" ...             one call; steps checked first; stops at the first failure, but a
                                   last "close" still runs; lines start with the step number
  ghost run "launch C:\path\app.exe" "wait Greet" "type Name bob" "click Greet"
web: launch msedge <url> | click Learn more | read | read title|url | go <url>
[win] = hwnd or title substring (click/read/wait: hwnd only, the rest is the name);
acting on a window ghost did not launch needs [win] named. exit 0 ok, 1 error, 2 usage
details: "#;

fn help() -> String {
    let readme = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("README.md").display().to_string())).unwrap_or_else(|| "README.md".into());
    format!("{HELP}{readme}\n")
}

fn is_ref(s: &str, p: char) -> bool {
    s.len() > 1 && s.starts_with(p) && s[1..].bytes().all(|b| b.is_ascii_digit())
}

pub fn clip(s: &str, n: usize) -> String {
    let s = s.replace("\r\n", "\\n").replace('\n', "\\n").replace('\r', "\\n");
    s.chars().take(n).collect()
}

impl Engine {
    pub fn new() -> Engine {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let uia: Option<IUIAutomation> = unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER).ok() };
        let mut e = Engine { uia, tree_cr: None, level_cr: None, states: HashMap::new(), ocr_engine: None, deadline: Instant::now() };
        e.build_cache_requests();
        e
    }

    fn build_cache_requests(&mut self) {
        let Some(uia) = &self.uia else { return };
        unsafe {
            let Ok(cr) = uia.CreateCacheRequest() else { return };
            for p in [
                UIA_NamePropertyId,
                UIA_ControlTypePropertyId,
                UIA_RuntimeIdPropertyId,
                UIA_ValueValuePropertyId,
                UIA_IsEnabledPropertyId,
                UIA_HasKeyboardFocusPropertyId,
                UIA_IsOffscreenPropertyId,
                UIA_BoundingRectanglePropertyId,
                UIA_ToggleToggleStatePropertyId,
                UIA_ExpandCollapseExpandCollapseStatePropertyId,
                UIA_IsTogglePatternAvailablePropertyId,
                UIA_IsExpandCollapsePatternAvailablePropertyId,
            ] {
                let _ = cr.AddProperty(p);
            }
            let _ = cr.SetTreeScope(TreeScope_Subtree);
            if let Ok(c) = uia.ControlViewCondition() {
                let _ = cr.SetTreeFilter(&c);
            }
            // Full mode: elements stay live, so click/type reuse them without a search.
            let _ = cr.SetAutomationElementMode(AutomationElementMode_Full);
            if let Ok(l) = cr.Clone() {
                let _ = l.SetTreeScope(TreeScope(TreeScope_Element.0 | TreeScope_Children.0));
                self.level_cr = Some(l);
            }
            self.tree_cr = Some(cr);
        }
    }

    fn uia(&self) -> R<&IUIAutomation> {
        self.uia.as_ref().map_or_else(|| ghost("UI Automation is unavailable"), Ok)
    }

    pub fn exec(&mut self, args: Vec<String>, cwd: String, cur: isize, deadline: Instant) -> Reply {
        self.deadline = deadline;
        let mut cx = Ctx { out: String::new(), err: String::new(), cur, cwd, implicit_run: false, step: None, launched: false };
        let cur0 = cur;
        let code = match self.run(&mut cx, &args) {
            Ok(()) => 0,
            Err(Fail::Usage(m)) => {
                cx.err.push_str(&m);
                cx.err.push_str("\nsee: ghost help\n");
                2
            }
            Err(Fail::Ghost(m)) => {
                cx.err.push_str(&m);
                cx.err.push('\n');
                1
            }
        };
        // A failed command leaves the current window as it was, unless it launched one (that must stay reachable).
        let cur = if code != 0 && !cx.launched { cur0 } else { cx.cur };
        Reply { code, out: cx.out, err: cx.err, cur }
    }

    // ---------- arguments ----------
    /// `<win>` is optional: present when the first positional is a live decimal hwnd and the command still has
    /// its required ones after it, or (fixed-arity commands only) when there are more positionals than it takes.
    /// With `max_n == JOIN` the words left after the window become one name.
    fn parse(&mut self, cx: &mut Ctx, a: &[String], min_n: usize, max_n: usize) -> R<Args> {
        let mut g = split_args(a)?;
        // Inside `run` the step's window is always the hwnd run_steps put first.
        if (cx.step.is_some() && !g.pos.is_empty()) || names_win(&g.pos, min_n, max_n) {
            g.win = find_win(&g.pos[0])?;
            g.pos.remove(0);
            cx.cur = hv(g.win);
            g.explicit = !cx.implicit_run;
        } else {
            if cx.cur == 0 || !win::is_window(hwnd(cx.cur)) {
                return ghost("no current window: ghost use <win>, or launch one");
            }
            g.win = hwnd(cx.cur);
        }
        if g.pos.len() < min_n || (max_n != JOIN && g.pos.len() > max_n) {
            return usage(usage_line(&a[0].to_lowercase(), cx.step.is_some()));
        }
        if max_n == JOIN && g.pos.len() > 1 {
            g.pos = vec![g.pos.join(" ")];
        }
        Ok(g)
    }

    // ---------- ownership (safety rule) ----------
    /// Acting on a window ghost did not launch needs that window named in the command itself. Through the
    /// implicit current window only reads are allowed, so an agent can't hit someone else's app by accident.
    fn add_launched(&mut self, h: HWND) {
        adopt(h, filetime_now());
    }

    fn owned(&mut self, g: Args) -> R<Args> {
        let p = win::pid(g.win);
        // pid check: a reused HWND value must not inherit ownership.
        let mine = lock(&LAUNCHED).get(&hv(g.win)).map(|e| alive(g.win, e)).unwrap_or(false);
        if !g.explicit && !mine {
            return ghost("not launched by ghost; name the window explicitly");
        }
        // A hung app gets nothing: refused now, so the action can never land late.
        if !win::responsive(g.win) {
            return ghost(format!("busy: {} not responding, nothing sent", win::proc_name(p)));
        }
        Ok(g)
    }

    /// Last check before anything is sent: once the caller has been told "busy", send nothing more.
    fn sendable(&self, h: HWND) -> R<()> {
        if Instant::now() >= self.deadline {
            return ghost(format!("busy: {} not responding, nothing sent", win::proc_name(win::pid(h))));
        }
        Ok(())
    }
}

/// Records a window as ghost's own: a launched one (`at` = now), or one a launched app opened by itself (`at`
/// inherited from the launch). Entries whose window or process is gone are pruned on the way.
pub fn adopt(h: HWND, at: u64) {
    let pid = win::app_pid(h);
    let Some(start) = proc_start(pid) else { return };
    let mut l = lock(&LAUNCHED);
    l.insert(hv(h), Own { pid, start, at });
    l.retain(|&k, e| alive(hwnd(k), e));
    save_launched(&l);
}

/// Should a newly shown top-level window be parked and adopted? Only if (a) its owner chain reaches a window
/// ghost launched or adopted, or (b) its process is a launched one (same pid AND creation time) and the launch
/// was under 30 s ago (splash -> main window). Returns the launch time to inherit. A window a long-running
/// launched app (a browser, Office) opens later for the user is left alone.
pub fn claim_new(h: HWND) -> Option<u64> {
    let l = lock(&LAUNCHED);
    if l.is_empty() {
        return None;
    }
    let mut o = unsafe { GetWindow(h, GW_OWNER).unwrap_or_default() };
    for _ in 0..16 {
        if o.0.is_null() {
            break;
        }
        if let Some(e) = l.get(&hv(o)) {
            if alive(o, e) {
                return Some(e.at);
            }
        }
        o = unsafe { GetWindow(o, GW_OWNER).unwrap_or_default() };
    }
    let pid = win::app_pid(h);
    if !l.values().any(|e| e.pid == pid) {
        return None; // cheap reject for the system-wide event stream
    }
    let start = proc_start(pid)?;
    let now = filetime_now();
    l.values().find(|e| e.pid == pid && e.start == start && now.saturating_sub(e.at) < 30 * 10_000_000).map(|e| e.at)
}

fn split_args(a: &[String]) -> R<Args> {
    let mut g = Args { win: HWND::default(), explicit: false, pos: vec![], opt: HashMap::new(), flags: HashSet::new() };
    let mut i = 1;
    while i < a.len() {
        if VALUE_OPTS.contains(&a[i].as_str()) {
            if i + 1 >= a.len() {
                return usage(format!("{} needs a value", a[i]));
            }
            g.opt.insert(a[i].clone(), a[i + 1].clone());
            i += 1;
        } else if a[i].starts_with("--") && a[i].len() > 2 {
            g.flags.insert(a[i].clone());
        } else {
            g.pos.push(a[i].clone());
        }
        i += 1;
    }
    Ok(g)
}

/// A joined name can't tell a window word from a name word (`read Next page`), so there only a live hwnd is a window.
fn names_win(pos: &[String], min_n: usize, max_n: usize) -> bool {
    (max_n != JOIN && pos.len() > max_n) || (pos.len() > min_n && pos[0].parse::<i64>().map(|n| win::is_window(hwnd(n as isize))).unwrap_or(false))
}

/// The window a request will act on, so the daemon serializes per window and a hung app stalls only its own
/// requests. -1: touches no app window (windows, use, help, errors), always answered at once. 0: launch.
pub fn route(a: &[String], cur: isize) -> isize {
    let c = a.first().map(|s| s.to_lowercase()).unwrap_or_default();
    let (min_n, max_n) = match c.as_str() {
        "launch" => return 0,
        "park" | "show" | "close" | "tree" => (0, 0),
        "read" => (0, JOIN),
        "click" | "wait" => (1, JOIN),
        "shot" | "go" => (1, 1),
        "type" | "key" => (1, 2),
        "ocr" => (0, 1),
        "run" => (0, 0),
        _ => return -1,
    };
    let k = if c == "run" {
        match a.get(1) {
            Some(s) if !is_step(s) => find_win(s).map(hv).unwrap_or(-1),
            _ => cur,
        }
    } else {
        match split_args(a) {
            Ok(g) if names_win(&g.pos, min_n, max_n) => find_win(&g.pos[0]).map(hv).unwrap_or(-1),
            Ok(_) => cur,
            Err(_) => -1,
        }
    };
    if k > 0 && win::is_window(hwnd(k)) { k } else { -1 }
}

/// "1s" or "500ms" as milliseconds, capped at 30 s; anything else is text to wait for.
pub fn delay_ms(s: &str) -> Option<u64> {
    let (n, mul) = if let Some(n) = s.strip_suffix("ms") { (n, 1) } else { (s.strip_suffix('s')?, 1000) };
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(n.parse::<u64>().unwrap_or(u64::MAX).saturating_mul(mul).min(30_000))
}

fn is_step(s: &str) -> bool {
    split_step(s).first().map(|w| STEP_VERBS.contains(&w.to_lowercase().as_str())).unwrap_or(false)
}

pub fn find_win(s: &str) -> R<HWND> {
    if let Ok(n) = s.parse::<i64>() {
        let h = hwnd(n as isize);
        if !win::is_window(h) {
            return ghost(format!("no window with hwnd {s}"));
        }
        return Ok(h);
    }
    let ls = s.to_lowercase();
    let all: Vec<HWND> = win::top_windows().into_iter().filter(|&h| win::listable(h)).collect();
    let hits: Vec<HWND> = all
        .iter()
        .copied()
        .filter(|&h| win::title(h).to_lowercase().contains(&ls) || win::proc_name(win::pid(h)).to_lowercase().contains(&ls))
        .collect();
    if hits.len() == 1 {
        return Ok(hits[0]);
    }
    let mut m = if hits.is_empty() { format!("no window matches \"{s}\"") } else { format!("{} windows match \"{s}\":", hits.len()) };
    for &h in &hits {
        m.push('\n');
        m.push_str(&win::line(h));
    }
    if hits.is_empty() {
        m.push_str("; windows:");
        for &h in &all {
            m.push('\n');
            m.push_str(&win::line(h));
        }
    }
    ghost(m)
}

impl Engine {
    // ---------- dispatch ----------
    pub fn run(&mut self, cx: &mut Ctx, a: &[String]) -> R<()> {
        if a.is_empty() {
            cx.out.push_str(&help()); // bare `ghost`: the short help
            return Ok(());
        }
        let cmd = a[0].to_lowercase();
        let need = |n: usize| if a.len() < n { usage(format!("usage: missing arguments for {}", a[0])) } else { Ok(()) };
        match cmd.as_str() {
            "help" | "-h" | "--help" => cx.out.push_str(&help()),
            "daemon-stop" => {
                daemon::request_stop();
                cx.line("ok");
            }
            "windows" => {
                for h in win::top_windows() {
                    if win::listable(h) {
                        cx.line(&win::line(h));
                    }
                }
            }
            "launch" => {
                need(2)?;
                self.launch(cx, a)?
            }
            "use" => {
                need(2)?;
                cx.cur = hv(find_win(&a[1])?);
            }
            "park" | "show" | "close" => {
                let g = self.parse(cx, a, 0, 0)?;
                let g = self.owned(g)?;
                daemon::wait_rest();
                daemon::mark_act(win::pid(g.win));
                match cmd.as_str() {
                    "park" => {
                        win::park(g.win).map_err(Fail::Ghost)?;
                    }
                    "show" => win::show(g.win),
                    _ => {
                        refuse_disabled(g.win, None)?;
                        let uwp_app = (win::class_of(g.win) == "ApplicationFrameWindow").then(|| win::app_pid(g.win)).filter(|&p| p != win::pid(g.win));
                        // Ghost's own browser closes over DevTools: WM_CLOSE waits on its UI thread, which under
                        // load ran past the 10 s request deadline (web-features run 5, 2026-10-17).
                        let dir = lock(&BROWSER).get(&hv(g.win)).map(|b| b.dir.clone());
                        let by_cdp = match dir.map(|d| crate::cdp::close(&d)) {
                            Some(Ok(())) => true,
                            Some(Err(e)) => {
                                dbg(&format!("close {}: path wm_close, cdp {e}", hv(g.win)));
                                false
                            }
                            None => false,
                        };
                        if by_cdp {
                            dbg(&format!("close {}: path cdp Browser.close", hv(g.win)));
                            let _ = std::fs::remove_file(win::park_file(g.win));
                        } else {
                            win::close(g.win);
                        }
                        let t = now_ms();
                        while win::is_window(g.win) && now_ms() - t < 5000 {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        if win::is_window(g.win) {
                            return ghost(self.still_open(g.win));
                        }
                        // A UWP app outlives its frame for a moment; launched again within ~1.5 s, its new window
                        // stays outside the new frame and launch times out (measured 2026-10-18: back to back, every
                        // second `launch calc` failed; 1.5 s apart, 4 of 4 ran). See UWP_CLOSED.
                        if let Some(p) = uwp_app {
                            *lock(&UWP_CLOSED) = Some(now_ms());
                            if let Ok(ph) = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, p) } {
                                let r = unsafe { WaitForSingleObject(ph, 5000) };
                                let _ = unsafe { CloseHandle(ph) };
                                dbg(&format!("close {}: uwp app pid {p} {}", hv(g.win), if r == WAIT_OBJECT_0 { "exited" } else { "still running after 5 s" }));
                            }
                            *lock(&UWP_CLOSED) = Some(now_ms());
                        }
                        forget(g.win);
                        self.states.remove(&hv(g.win));
                        if cx.cur == hv(g.win) {
                            cx.cur = 0;
                        }
                        // Said out loud, so an agent doesn't spend a turn checking that it closed.
                        let k = cx.step.map(|k| format!("{k} ")).unwrap_or_default();
                        cx.line(&format!("{k}closed"));
                    }
                }
            }
            "tree" => {
                let g = self.parse(cx, a, 0, 0)?;
                self.tree(cx, g)?
            }
            "read" => {
                let g = self.parse(cx, a, 0, JOIN)?;
                let h = g.win;
                self.prime(h);
                let browser = self.st(h).browser;
                match g.pos.first().map(|s| s.as_str()) {
                    None if browser => self.read_page(cx, h)?,
                    None => self.read_window(cx, h)?,
                    Some(t) if browser && (t.eq_ignore_ascii_case("title") || t.eq_ignore_ascii_case("url")) => {
                        let nodes = self.snapshot(h, false)?;
                        let (title, url) = page_id(&nodes, &win::title(h)).unwrap_or_else(|| (win::title(h), String::new()));
                        cx.line(if t.eq_ignore_ascii_case("title") { &title } else { &url });
                    }
                    Some(t) => {
                        let el = self.resolve(h, t)?;
                        self.read(cx, &el)?
                    }
                }
            }
            "click" | "type" | "key" => {
                let max = if cmd == "click" { JOIN } else { 2 };
                let g = self.parse(cx, a, 1, max)?;
                let g = self.owned(g)?;
                self.act(cx, &cmd, g)?
            }
            "debug-fg0" => {
                let g = self.parse(cx, a, 0, 0)?;
                let g = self.owned(g)?;
                cx.line(&daemon::debug_fg0(g.win).map_err(Fail::Ghost)?);
            }
            "go" => {
                let g = self.parse(cx, a, 1, 1)?;
                let g = self.owned(g)?;
                self.go(cx, g)?
            }
            "shot" => {
                let g = self.parse(cx, a, 1, 1)?;
                self.shot(cx, g)?
            }
            "ocr" => {
                let g = self.parse(cx, a, 0, 1)?;
                self.ocr(cx, g)?
            }
            "wait" => {
                // `wait 1s` / `wait 500ms` is a plain delay (agents write it), capped at 30 s; it needs no window.
                let mut pos = split_args(a)?.pos;
                if names_win(&pos, 1, JOIN) {
                    pos.remove(0);
                }
                if let Some(ms) = (pos.len() == 1).then(|| delay_ms(&pos[0])).flatten() {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                    cx.line(&format!("ok {ms}ms"));
                    return Ok(());
                }
                let g = self.parse(cx, a, 1, JOIN)?;
                self.wait(cx, g)?
            }
            "run" => {
                need(2)?;
                self.run_steps(cx, a)?
            }
            _ => return usage(format!("unknown command: {}; commands: windows launch use park show close tree click type key read wait go shot ocr run help", a[0])),
        }
        Ok(())
    }

    fn run_steps(&mut self, cx: &mut Ctx, a: &[String]) -> R<()> {
        // run [<win>] "<step>" ...: the window is optional when the first argument is already a step.
        let head = split_step(&a[1]);
        let verb = head.first().map(|w| w.to_lowercase()).unwrap_or_default();
        let (mut win, first) = if STEP_VERBS.contains(&verb.as_str()) {
            // A run that starts with `launch` needs no window yet; the launch sets it.
            (if verb == "launch" { None } else { Some(self.parse(cx, &["run".to_string()], 0, 0)?.win) }, 1)
        } else {
            // Not a step: a window, or (a typo'd verb, say) worth naming the step verbs for.
            let h = find_win(&a[1]).map_err(|e| match e {
                Fail::Ghost(_) if a.len() == 2 || a[1].contains(' ') => {
                    let verbs: Vec<&str> = STEP_VERBS.iter().copied().filter(|v| *v != "use").collect();
                    Fail::Usage(format!("step 1: not a step command: {}; steps start with one of: {}", head.first().cloned().unwrap_or_default(), verbs.join(" ")))
                }
                e => e,
            })?;
            cx.cur = hv(h);
            (Some(h), 2)
        };
        cx.implicit_run = first == 1; // steps inherit how run itself got its window
        // Every step is checked before any runs: a usage error launches nothing and leaves nothing open.
        let mut steps = vec![];
        for (i, step) in a.iter().enumerate().skip(first) {
            let parts = split_step(step);
            if parts.is_empty() {
                continue;
            }
            let k = i - first + 1;
            check_step(&parts).map_err(|m| Fail::Usage(format!("step {k}: {m}")))?;
            steps.push((k, parts));
        }
        // A last bare `close` runs even after a failed step, like a finally, so nothing is left open.
        let close_last = steps.last().map(|(_, p)| p.len() == 1 && p[0].eq_ignore_ascii_case("close")).unwrap_or(false);
        for (idx, (k, parts)) in steps.iter().enumerate() {
            let (k, mut parts) = (*k, parts.clone());
            let c = parts[0].to_lowercase();
            let launch = c == "launch";
            let go = c == "go";
            if !launch {
                let Some(w) = win else { return ghost(format!("step {k}: no window yet; start with launch or name one")) };
                parts.insert(1, hv(w).to_string());
            }
            cx.step = Some(k);
            let cur_before = cx.cur;
            let r = self.run(cx, &parts);
            cx.step = None;
            // A launch that recorded its window in launched.txt and then failed still made it the run's window, so a
            // trailing close shuts it (verifier, round 15: two Calculators were left open).
            let recorded = launch && cx.cur != 0 && cx.cur != cur_before && lock(&LAUNCHED).contains_key(&cx.cur);
            if ((launch || go) && r.is_ok()) || recorded {
                win = Some(hwnd(cx.cur)); // the launched (or relaunched) window is the target of the steps after it
            }
            let r = match r {
                Ok(()) => continue,
                Err(Fail::Usage(m)) => Fail::Usage(format!("step {k}: {m}")),
                Err(Fail::Ghost(m)) => Fail::Ghost(format!("step {k}: {m}")),
            };
            if close_last && idx + 1 < steps.len() {
                let kc = steps[steps.len() - 1].0;
                dbg(&format!("run: step {k} failed; finally close of {:?} (window {})", win.map(hv), win.map(win::is_window).unwrap_or(false)));
                match win {
                    Some(w) if win::is_window(w) => {
                        cx.step = Some(kc);
                        if let Err(Fail::Usage(m) | Fail::Ghost(m)) = self.run(cx, &["close".to_string(), hv(w).to_string()]) {
                            cx.line(&format!("{kc} close failed: {m}"));
                        }
                        cx.step = None;
                    }
                    // Already gone by itself (a folder window's own explorer.exe exited, 2 of 10 N1 runs on
                    // 2026-10-18): the finally's goal holds, and it says so rather than staying silent.
                    Some(w) => {
                        dbg(&format!("run: {} was already closed before the finally close", hv(w)));
                        forget(w);
                        cx.line(&format!("{kc} closed"));
                    }
                    None => {}
                }
            }
            return Err(r);
        }
        Ok(())
    }
}

/// A step's words, checked before any step runs: a step verb and the argument count it takes (a step never names
/// its window). Err: what to write instead.
fn check_step(parts: &[String]) -> Result<(), String> {
    let c = parts[0].to_lowercase();
    if !STEP_VERBS.contains(&c.as_str()) || c == "use" {
        let verbs: Vec<&str> = STEP_VERBS.iter().copied().filter(|v| *v != "use").collect();
        return Err(format!("not a step command: {}; steps start with one of: {}", parts[0], verbs.join(" ")));
    }
    let n = split_args(parts).map_err(|e| match e {
        Fail::Usage(m) | Fail::Ghost(m) => m,
    })?.pos.len();
    let ok = match c.as_str() {
        "launch" | "click" | "wait" => n >= 1,
        "type" | "key" => (1..=2).contains(&n), // the same bounds `parse` applies when the step runs
        "go" | "shot" => n == 1,
        "ocr" => n <= 1,
        "read" => true,
        _ => n == 0, // tree, park, show, close
    };
    if ok { Ok(()) } else { Err(usage_line(&c, true)) }
}

/// One `run` step into words: double quotes group words, \" is a literal quote, "" is an empty argument.
pub fn split_step(s: &str) -> Vec<String> {
    let mut r = vec![];
    let mut cur = String::new();
    let (mut q, mut any) = (false, false);
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' && it.peek() == Some(&'"') {
            it.next();
            cur.push('"');
            any = true;
        } else if c == '"' {
            q = !q;
            any = true;
        } else if c.is_whitespace() && !q {
            if any {
                r.push(std::mem::take(&mut cur));
            }
            any = false;
        } else {
            cur.push(c);
            any = true;
        }
    }
    if any {
        r.push(cur);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::split_step;
    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn steps() {
        assert_eq!(split_step(r#"key "Address and search bar" enter"#), v(&["key", "Address and search bar", "enter"]));
        assert_eq!(split_step(r#"type Body """#), v(&["type", "Body", ""]));
        assert_eq!(split_step(r#"type Body "a \"q\" b""#), v(&["type", "Body", r#"a "q" b"#]));
        assert_eq!(split_step("  read   Body "), v(&["read", "Body"]));
    }
    fn node(n: u32, depth: u32, role: &str, name: &str) -> super::Node {
        super::Node { n, depth, role: role.into(), name: name.into(), value: None, flags: String::new(), more: None, hidden: false, el: None }
    }
    #[test]
    fn window_word() {
        use super::{names_win, JOIN};
        // `read Next page`: no live hwnd first, so every word is the name.
        assert!(!names_win(&v(&["Next", "page"]), 0, JOIN));
        assert!(!names_win(&v(&["Learn", "more"]), 1, JOIN));
        // fixed arity keeps the title-substring window: `type notepad Body hi`
        assert!(names_win(&v(&["notepad", "Body", "hi"]), 1, 2));
        assert!(!names_win(&v(&["Body", "hi"]), 1, 2));
    }
    #[test]
    fn pages() {
        use super::{distance, page_range, page_text};
        let t = vec![
            node(1, 0, "win", "x"),
            node(2, 1, "edit", "Address and search bar"),
            node(3, 1, "doc", "Other"),
            node(4, 2, "text", "frame"),
            node(5, 1, "doc", "Example Domain"),
            node(6, 2, "text", "Example Domain"),
            node(7, 2, "link", "Learn more"),
            node(8, 1, "ti", "Example Domain"),
        ];
        assert_eq!(page_range(&t, "Example Domain - Microsoft Edge"), Some((4, 7)));
        assert_eq!(page_range(&t, "untitled"), Some((2, 4)));
        let p = page_text(&t, "Example Domain - Edge", 1000, None);
        assert!(p.starts_with("\"Example Domain\" \n") && p.contains("e7 link \"Learn more\"") && !p.contains("e8"));
        assert!(page_text(&t, "Example Domain - Edge", 30, None).contains("1 more lines"));
        assert_eq!(distance("lern more", "learn more"), 1);
        assert_eq!(distance("", "abc"), 3);
    }
    #[test]
    fn profile_names() {
        use super::ghost_profile;
        assert!(ghost_profile("ghost-134367055101141360"));
        for n in ["134367055101141360", "ghost-", "ghost-12a", "Default", "ghost-1/..", "my-ghost-1"] {
            assert!(!ghost_profile(n), "{n}");
        }
    }
    #[test]
    fn delays() {
        use super::delay_ms;
        assert_eq!(delay_ms("1s"), Some(1000));
        assert_eq!(delay_ms("250ms"), Some(250));
        assert_eq!(delay_ms("90s"), Some(30_000));
        assert_eq!(delay_ms("s"), None);
        assert_eq!(delay_ms("1.5s"), None);
        assert_eq!(delay_ms("Settings"), None);
        assert_eq!(delay_ms("ms"), None);
    }
    #[test]
    fn hung_owner_window_is_handed_back() {
        // A window whose thread stops answering is hung (IsHungAppWindow after 5 s), but it is still the owner's
        // window: handback must return it, never the sink or the taskbar.
        use crate::win;
        use windows::Win32::UI::WindowsAndMessaging::*;
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || unsafe {
            let h = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, windows::core::w!("STATIC"), windows::core::w!("hung"), WS_POPUP | WS_VISIBLE, -32000, -32000, 1, 1, None, None, None, None).unwrap();
            tx.send(win::hv(h)).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(6500)); // stop pumping: the window goes hung
            let _ = DestroyWindow(h);
        });
        let h = win::hwnd(rx.recv().unwrap());
        std::thread::sleep(std::time::Duration::from_millis(5800));
        assert!(unsafe { IsHungAppWindow(h).as_bool() }, "the test window never went hung");
        assert_eq!(win::owner_window(h), h);
        assert_eq!(win::handback(h, Default::default()), h, "a hung owner window was swapped for the sink or the taskbar");
        t.join().unwrap();
    }
    #[test]
    fn uwp_classifier() {
        use super::uwp_kind;
        use std::path::{Path, PathBuf};
        let wa = Path::new(r"C:\U\WindowsApps");
        let links = |set: &'static [&'static str]| move |p: &Path| set.iter().any(|s| p == Path::new(s));
        let none = links(&[]);
        let notepad_app = links(&[r"C:\U\WindowsApps\notepad.exe"]);
        assert!(uwp_kind("calc", &none, Some(wa), None));
        assert!(uwp_kind(r"C:\Windows\System32\calc.exe", &none, Some(wa), None));
        assert!(uwp_kind("ms-settings:display", &none, Some(wa), None));
        assert!(uwp_kind(r"shell:AppsFolder\Microsoft.WindowsCalculator_8wekyb3d8bbwe!App", &none, Some(wa), None));
        assert!(!uwp_kind("notepad", &none, Some(wa), None), "classic Notepad only: not UWP");
        assert!(uwp_kind("notepad.exe", &notepad_app, Some(wa), None), "packaged Notepad's alias installed");
        assert!(uwp_kind("wt", &links(&[r"C:\U\WindowsApps\wt.exe"]), Some(wa), Some(PathBuf::from(r"C:\U\WindowsApps\wt.exe"))));
        assert!(!uwp_kind("explorer.exe", &none, Some(wa), Some(PathBuf::from(r"C:\Windows\explorer.exe"))));
        assert!(!uwp_kind("mailto:a@b.c", &none, Some(wa), None));
        assert!(!uwp_kind("mspaint", &none, None, None));
    }
    #[test]
    fn handback_prior_rules() {
        // Round 11: the prior foreground goes back as long as it exists, hidden or transient; gone, the owner's
        // last visible window; the sink (the taskbar here: no sink in tests) only for prior 0 or none left.
        use crate::win;
        use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::*;
        unsafe extern "system" fn proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            unsafe { DefWindowProcW(h, m, w, l) }
        }
        unsafe {
            let wc = WNDCLASSW { lpfnWndProc: Some(proc), lpszClassName: windows::core::w!("ForegroundStaging"), ..Default::default() };
            RegisterClassW(&wc);
            let mk = |class: windows::core::PCWSTR, style: WINDOW_STYLE| CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, class, windows::core::w!("t"), WS_POPUP | style, -32000, -32000, 1, 1, None, None, None, None).unwrap();
            let last = mk(windows::core::w!("STATIC"), WS_VISIBLE);
            let transient = mk(windows::core::w!("ForegroundStaging"), WS_VISIBLE);
            let hidden = mk(windows::core::w!("STATIC"), WINDOW_STYLE(0));
            let gone = mk(windows::core::w!("STATIC"), WS_VISIBLE);
            DestroyWindow(gone).unwrap();
            assert!(!win::user_window(transient) && !win::user_window(hidden) && win::user_window(last));
            assert_eq!(win::handback(transient, last), transient, "transient prior");
            assert_eq!(win::handback(hidden, last), hidden, "hidden prior");
            assert_eq!(win::handback(gone, last), last, "prior gone");
            assert_eq!(win::handback(gone, gone), win::taskbar(), "prior and last user window both gone");
            assert_eq!(win::handback(HWND::default(), last), win::taskbar(), "prior 0");
            assert_eq!(win::handback(win::taskbar(), last), win::taskbar(), "taskbar prior counts as 0");
            for h in [last, transient, hidden] {
                let _ = DestroyWindow(h);
            }
        }
    }
    #[test]
    fn stale_park_files() {
        // A park file left for a window ghost doesn't own (the taskbar here) must never make it ghost's.
        use crate::win;
        let t = win::taskbar();
        assert!(!t.0.is_null());
        let f = win::park_file(t);
        let p = win::pid(t);
        let start = super::proc_start(p).unwrap();
        for stale in ["0 0 10 10".to_string(), format!("0 0 10 10 {p} {}", start + 1), format!("0 0 10 10 {} {start}", p + 4)] {
            std::fs::write(&f, &stale).unwrap();
            assert!(!super::ghost_window(t), "{stale}");
        }
        win::prune_parks();
        assert!(!f.exists(), "stale park file not pruned");
        // Positive control: the right pid and creation time do claim it (removed at once).
        std::fs::write(&f, format!("0 0 10 10 {p} {start}")).unwrap();
        let owned = super::ghost_window(t);
        std::fs::remove_file(&f).unwrap();
        assert!(owned);
    }
}

// ---------- VARIANT helpers (VARIANT has no Drop: every one is cleared here) ----------
struct Var(VARIANT);
impl Drop for Var {
    fn drop(&mut self) {
        unsafe {
            let _ = VariantClear(&mut self.0);
        }
    }
}
impl Var {
    fn vt(&self) -> u16 {
        unsafe { self.0.Anonymous.Anonymous.vt.0 }
    }
    fn bstr(&self) -> Option<String> {
        (self.vt() == VT_BSTR.0).then(|| unsafe { self.0.Anonymous.Anonymous.Anonymous.bstrVal.to_string() })
    }
    fn i32(&self) -> Option<i32> {
        (self.vt() == VT_I4.0).then(|| unsafe { self.0.Anonymous.Anonymous.Anonymous.lVal })
    }
    fn bool(&self) -> Option<bool> {
        (self.vt() == VT_BOOL.0).then(|| unsafe { self.0.Anonymous.Anonymous.Anonymous.boolVal.0 != 0 })
    }
    fn i32s(&self) -> Option<Vec<i32>> {
        if self.vt() != (VT_ARRAY.0 | VT_I4.0) {
            return None;
        }
        unsafe { safearray_i32(self.0.Anonymous.Anonymous.Anonymous.parray) }
    }
}
unsafe fn safearray_i32(p: *const SAFEARRAY) -> Option<Vec<i32>> {
    if p.is_null() || (*p).cDims != 1 {
        return None;
    }
    let n = (*p).rgsabound[0].cElements as usize;
    Some(std::slice::from_raw_parts((*p).pvData as *const i32, n).to_vec())
}
fn cached(el: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Option<Var> {
    unsafe { el.GetCachedPropertyValue(id).ok().map(Var) }
}

// ---------- roles ----------
#[allow(non_upper_case_globals)]
fn role_of(ct: i32) -> String {
    let r = match UIA_CONTROLTYPE_ID(ct) {
        UIA_ButtonControlTypeId => "btn",
        UIA_EditControlTypeId => "edit",
        UIA_DocumentControlTypeId => "doc",
        UIA_TextControlTypeId => "text",
        UIA_HyperlinkControlTypeId => "link",
        UIA_CheckBoxControlTypeId => "chk",
        UIA_RadioButtonControlTypeId => "radio",
        UIA_ComboBoxControlTypeId => "combo",
        UIA_ListControlTypeId => "list",
        UIA_ListItemControlTypeId => "li",
        UIA_MenuControlTypeId => "menu",
        UIA_MenuItemControlTypeId => "mi",
        UIA_MenuBarControlTypeId => "menubar",
        UIA_TabControlTypeId => "tab",
        UIA_TabItemControlTypeId => "ti",
        UIA_TreeControlTypeId => "tree",
        UIA_TreeItemControlTypeId => "tvi",
        UIA_TableControlTypeId | UIA_DataGridControlTypeId => "tbl",
        UIA_DataItemControlTypeId => "row",
        UIA_HeaderControlTypeId | UIA_HeaderItemControlTypeId => "hdr",
        UIA_SliderControlTypeId => "slider",
        UIA_ProgressBarControlTypeId => "prog",
        UIA_ScrollBarControlTypeId => "sb",
        UIA_ToolBarControlTypeId => "tbar",
        UIA_StatusBarControlTypeId => "sbar",
        UIA_TitleBarControlTypeId => "title",
        UIA_WindowControlTypeId => "win",
        UIA_PaneControlTypeId => "pane",
        UIA_GroupControlTypeId => "group",
        UIA_ImageControlTypeId => "img",
        UIA_SplitButtonControlTypeId => "split",
        UIA_SpinnerControlTypeId => "spin",
        UIA_ToolTipControlTypeId => "tip",
        UIA_ThumbControlTypeId => "thumb",
        UIA_SeparatorControlTypeId => "sep",
        UIA_CalendarControlTypeId => "cal",
        UIA_CustomControlTypeId => "custom",
        UIA_SemanticZoomControlTypeId => "semanticzoom",
        UIA_AppBarControlTypeId => "appbar",
        _ => "custom",
    };
    r.to_string()
}
const NOISE_ROLES: [&str; 4] = ["title", "sb", "thumb", "sep"];
const INPUT_ROLES: [&str; 7] = ["edit", "doc", "combo", "chk", "radio", "slider", "btn"];

pub fn fmt(n: &Node) -> String {
    if let Some(m) = &n.more {
        return m.clone();
    }
    let mut s = format!("e{} {}", n.n, n.role);
    if !n.name.is_empty() {
        let name = clip(&n.name, 60);
        // Quotes only where the name could be misread: spaces, quotes, or a start like a value or flag.
        if name.contains([' ', '"']) || name.starts_with(['=', '[', '+', '-', '~']) {
            s.push_str(&format!(" \"{name}\""));
        } else {
            s.push(' ');
            s.push_str(&name);
        }
    }
    // A value that only repeats the name (Explorer's `li 4K ="4K"`) says nothing new.
    if let Some(v) = n.value.as_deref().filter(|v| !v.is_empty() && *v != n.name) {
        s.push_str(&format!(" =\"{}\"", clip(v, 80)));
    }
    if !n.flags.is_empty() {
        s.push(' ');
        s.push_str(&n.flags);
    }
    s
}

struct Walk {
    h: isize,
    nodes: Vec<Node>,
    all: bool,
    wr: RECT,
    path: HashSet<Vec<i32>>,
    budget: i32,
    level: Option<IUIAutomationCacheRequest>, // per-level fallback
    browser: bool,
}

impl Walk {
    fn walk(&mut self, e: &IUIAutomationElement, depth: u32, parent_name: Option<&str>, in_page: bool) {
        if depth > 25 {
            return;
        }
        self.budget -= 1;
        if self.budget < 0 {
            return;
        }
        let (name, ct) = unsafe {
            let Ok(nm) = e.CachedName() else { return }; // element vanished mid-walk: skip it, never crash
            (nm.to_string().trim().to_string(), e.CachedControlType().map(|c| c.0).unwrap_or(0))
        };
        let mut role = role_of(ct);
        if role == "win" && depth > 0 {
            role = "dlg".into();
        }
        if !self.all && NOISE_ROLES.contains(&role.as_str()) {
            return; // window chrome: whole subtree
        }
        // A web page below the fold is still the page: scrolled-away content is only skipped outside documents.
        if !self.all && !in_page && unsafe { e.CachedIsOffscreen().map(|b| b.as_bool()).unwrap_or(false) } {
            // Parked windows are off every monitor, so only skip what lies outside the window itself (scrolled away).
            let b = unsafe { e.CachedBoundingRectangle().unwrap_or_default() };
            let wr = &self.wr;
            if (b.right - b.left <= 0 && b.bottom - b.top <= 0) || b.right <= wr.left || b.left >= wr.right || b.bottom <= wr.top || b.top >= wr.bottom {
                return;
            }
        }
        let value = cached(e, UIA_ValueValuePropertyId).and_then(|v| v.bstr());
        let mut flags = String::new();
        if cached(e, UIA_IsTogglePatternAvailablePropertyId).and_then(|v| v.bool()) == Some(true) {
            if let Some(t) = cached(e, UIA_ToggleToggleStatePropertyId).and_then(|v| v.i32()) {
                flags.push_str(if t == 1 { "[on]" } else { "[off]" });
            }
        }
        if unsafe { e.CachedIsEnabled().map(|b| !b.as_bool()).unwrap_or(false) } {
            flags.push_str("[x]");
        }
        if unsafe { e.CachedHasKeyboardFocus().map(|b| b.as_bool()).unwrap_or(false) } {
            flags.push_str("[*]");
        }
        if cached(e, UIA_IsExpandCollapsePatternAvailablePropertyId).and_then(|v| v.bool()) == Some(true) {
            match cached(e, UIA_ExpandCollapseExpandCollapseStatePropertyId).and_then(|v| v.i32()) {
                Some(1) | Some(2) => flags.push_str("[+]"),
                Some(0) => flags.push_str("[-]"),
                _ => {}
            }
        }
        let rid = cached(e, UIA_RuntimeIdPropertyId).and_then(|v| v.i32s());
        // A RuntimeId seen again on the same branch is a provider cycle (real apps loop forever): stop there.
        if let Some(r) = &rid {
            if !self.path.insert(r.clone()) {
                return;
            }
        }
        let mut keep = self.all || !name.is_empty() || value.as_deref().map(|v| !v.is_empty()).unwrap_or(false) || INPUT_ROLES.contains(&role.as_str()) || depth == 0;
        // Fold a text child that only repeats its parent's label.
        if !self.all && role == "text" && parent_name.map(|p| p.eq_ignore_ascii_case(&name)).unwrap_or(false) {
            keep = false;
        }
        let mut child_depth = depth;
        let mut child_parent: Option<String> = parent_name.map(|s| s.to_string());
        let child_in_page = in_page || (self.browser && role == "doc");
        if keep {
            if let Some(r) = &rid {
                let n = ref_of(self.h, r);
                // A page's heading often repeats its title (the document's name); it is content, never folded.
                let doc = role == "doc";
                self.nodes.push(Node { n, depth, role, name: name.clone(), value, flags, more: None, hidden: false, el: Some(e.clone()) });
                child_depth = depth + 1;
                child_parent = if doc { None } else { Some(name) };
            }
        }
        unsafe {
            if let Ok(kids) = e.GetCachedChildren() {
                let n = kids.Length().unwrap_or(0);
                for i in 0..n {
                    let Ok(c) = kids.GetElement(i) else { continue };
                    let c = match &self.level {
                        Some(l) => match c.BuildUpdatedCache(l) {
                            Ok(c) => c,
                            Err(e2) => {
                                dbg(&format!("level cache failed at depth {depth}: {}", e2.message()));
                                continue;
                            }
                        },
                        None => c,
                    };
                    self.walk(&c, child_depth, child_parent.as_deref(), child_in_page);
                }
            }
        }
        if let Some(r) = &rid {
            self.path.remove(r);
        }
    }
}

/// Runs of more than 12 same-role siblings: keep the first 10, summarize the rest.
fn collapse(nodes: &mut Vec<Node>) {
    let mut done = vec![false; nodes.len()];
    let mut inserts: Vec<(usize, Node)> = vec![];
    for i in 0..nodes.len() {
        if nodes[i].hidden || done[i] {
            continue;
        }
        let (d, role) = (nodes[i].depth, nodes[i].role.clone());
        let mut run = vec![i];
        let mut j = i + 1;
        loop {
            while j < nodes.len() && nodes[j].depth > d {
                j += 1;
            }
            if j < nodes.len() && nodes[j].depth == d && nodes[j].role == role {
                run.push(j);
                j += 1;
            } else {
                break;
            }
        }
        for &k in &run {
            done[k] = true;
        }
        if run.len() <= 12 {
            continue;
        }
        for k in run[10]..j {
            nodes[k].hidden = true;
        }
        let more = Node { n: 0, depth: d, role: String::new(), name: String::new(), value: None, flags: String::new(), more: Some(format!("\u{2026} {} more {}", run.len() - 10, role)), hidden: false, el: None };
        inserts.push((j, more));
    }
    for (at, n) in inserts.into_iter().rev() {
        nodes.insert(at, n);
    }
}

fn empty_doc(nodes: &[Node]) -> bool {
    for (i, n) in nodes.iter().enumerate() {
        if n.role == "doc" {
            return i + 1 >= nodes.len() || nodes[i + 1].depth <= n.depth;
        }
    }
    false
}

const BROWSERS: [&str; 6] = ["chrome", "msedge", "opera", "brave", "vivaldi", "electron"];
/// Chromium browsers that get a throwaway profile at launch, so they never hand off to the owner's instance.
const PROFILED: [&str; 4] = ["msedge", "chrome", "brave", "opera"];
/// Chars of page elements shown after `launch <browser> <url>`: about 400 tokens, so a whole web task (commands
/// plus everything ghost prints) stays under 4000 chars, and enough for a simple page (example.com is ~150).
const PAGE_CAP: usize = 1600;
/// After a click or key navigates: the new page's title and URL are the news, its elements a short preview.
const NAV_CAP: usize = 700;
const READ_CAP: usize = 1500; // text printed by `read`

/// The page document's subtree in a browser window, as an index range of the snapshot: the outermost document
/// whose name starts the window title (the active tab), else the first one. Frames are documents inside it.
fn page_range(nodes: &[Node], title: &str) -> Option<(usize, usize)> {
    let mut first = None;
    let mut i = 0;
    while i < nodes.len() {
        if nodes[i].role != "doc" || nodes[i].more.is_some() {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < nodes.len() && nodes[j].depth > nodes[i].depth {
            j += 1;
        }
        if !nodes[i].name.is_empty() && title.starts_with(&nodes[i].name) {
            return Some((i, j));
        }
        first = first.or(Some((i, j)));
        i = j;
    }
    first
}

/// The browser's address bar: the first named text box before the page.
fn addr_bar(nodes: &[Node], before: usize) -> Option<usize> {
    nodes[..before].iter().position(|n| n.role == "edit" && !n.name.is_empty())
}

/// (title, url) of the page.
fn page_id(nodes: &[Node], title: &str) -> Option<(String, String)> {
    page_range(nodes, title).map(|(s, _)| (nodes[s].name.clone(), nodes[s].value.clone().unwrap_or_default()))
}

/// Which snapshot lines a browser window shows by default: the address bar and the page. None: everything.
fn view_mask(nodes: &[Node], title: &str) -> Option<Vec<bool>> {
    let (s, e) = page_range(nodes, title)?;
    let mut m = vec![false; nodes.len()];
    if let Some(a) = addr_bar(nodes, s) {
        m[a] = true;
    }
    m[s..e].iter_mut().for_each(|x| *x = true);
    Some(m)
}

/// `"title" url` (the url left out when it is the one the agent asked for), then the page's elements indented
/// under the document: link URLs dropped, a repeated role+name shown once, cut near `cap` chars.
fn page_text(nodes: &[Node], title: &str, cap: usize, asked: Option<&str>) -> String {
    let Some((s, e)) = page_range(nodes, title) else { return "\"\" (no page document yet)\n".into() };
    let d = &nodes[s];
    let url = d.value.as_deref().unwrap_or("");
    let same = |a: &str, b: &str| a.trim_end_matches('/').eq_ignore_ascii_case(b.trim_end_matches('/'));
    let mut out = if asked.map(|a| same(a, url)).unwrap_or(false) { format!("\"{}\"\n", clip(&d.name, 80)) } else { format!("\"{}\" {url}\n", clip(&d.name, 80)) };
    let (mut used, mut left) = (0, 0);
    let mut seen = HashSet::new();
    let mut up: Vec<(u32, &str)> = vec![]; // ancestors' names: `li Domains` > `link Domains` is one line
    for n in nodes[s + 1..e].iter().filter(|n| !n.hidden) {
        while up.last().map(|&(d, _)| d >= n.depth).unwrap_or(false) {
            up.pop();
        }
        let repeats = up.last().map(|&(_, p)| !n.name.is_empty() && p.eq_ignore_ascii_case(&n.name)).unwrap_or(false);
        up.push((n.depth, &n.name));
        if repeats || (n.more.is_none() && !n.name.is_empty() && !seen.insert((n.role.clone(), n.name.clone()))) {
            continue;
        }
        if left > 0 {
            left += 1;
            continue;
        }
        let mut c = n.clone();
        if c.role == "link" {
            c.value = None; // `ghost read <link>` gives the URL when it matters
        }
        let l = format!("{}{}\n", " ".repeat(n.depth.saturating_sub(d.depth + 1) as usize), fmt(&c));
        if used + l.len() > cap {
            left = 1;
            continue;
        }
        used += l.len();
        out.push_str(&l);
    }
    if left > 0 {
        out.push_str(&format!("\u{2026} {left} more lines (cap {cap} chars): ghost tree --find <text>\n"));
    }
    if s + 1 == e {
        out.push_str("\u{2026}page loading\n");
    }
    out
}

/// `read` output: at most READ_CAP chars, saying so when cut.
fn cap_text(t: &str) -> String {
    if t.chars().count() <= READ_CAP {
        return t.to_string();
    }
    format!("{}\n\u{2026} cut at {READ_CAP} chars", t.chars().take(READ_CAP).collect::<String>())
}

/// Edit distance over chars, for "nearest names".
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().take(60).collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().take(60).enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for j in 0..b.len() {
            let cur = row[j + 1];
            row[j + 1] = (prev + (ca != b[j]) as usize).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// Exact (case-insensitive) name first, then a unique substring. Ok(None): nothing; Err: ambiguous.
fn pick(nodes: &[Node], lt: &str, t: &str) -> R<Option<u32>> {
    let named = |n: &&Node| n.more.is_none() && n.depth > 0;
    let exact: Vec<&Node> = nodes.iter().filter(named).filter(|n| n.name.to_lowercase() == lt).collect();
    let part: Vec<&Node> = nodes.iter().filter(named).filter(|n| n.name.to_lowercase() != lt && n.name.to_lowercase().contains(lt)).collect();
    let hits = if exact.is_empty() { part } else { exact };
    match hits.len() {
        0 => Ok(None),
        1 => Ok(Some(hits[0].n)),
        k => {
            let mut m = format!("{k} elements match \"{t}\":");
            for n in hits.iter().take(5) {
                m.push_str(&format!("\ne{} {} {}", n.n, n.role, clip(&n.name, 40)));
            }
            ghost(m)
        }
    }
}

/// Recovery hint for a browser page that UIA doesn't show (empty or missing document).
const NOT_EXPOSED: &str = "page not exposed to UI Automation yet (its document is empty): if it is loading, ghost wait <text> polls until it shows; a browser ghost did not start may never expose it, so open one with: ghost launch msedge <url>";

impl Engine {
    pub fn st(&mut self, h: HWND) -> &mut WinState {
        if !self.states.contains_key(&hv(h)) && self.states.len() >= 64 {
            // bounded: drop state of windows that are gone
            self.states.retain(|&k, _| win::is_window(hwnd(k)));
            if self.states.len() >= 64 {
                self.states.clear();
            }
        }
        self.states.entry(hv(h)).or_default()
    }

    pub fn snapshot(&mut self, h: HWND, all: bool) -> R<Rc<Vec<Node>>> {
        self.prime(h);
        let pid = win::app_pid(h); // UI events of a UWP app come from the app, not its frame host
        daemon::watch_pid(pid);
        let now = now_ms();
        {
            let ws = self.st(h);
            // Event-driven cache: no object event from that process since the snapshot means nothing changed.
            if let Some(c) = &ws.cache {
                // Age counts from when the walk finished (slow walks must not expire at birth); quiet counts from its start.
                if ws.cache_all == all && now - ws.cache_end < 3000 && daemon::quiet_since(pid, ws.cache_at) {
                    return Ok(c.clone());
                }
            }
        }
        let uia = self.uia()?.clone();
        let tree_cr = self.tree_cr.clone().map_or_else(|| ghost("UI Automation is unavailable"), Ok)?;
        let (root, level) = match unsafe { uia.ElementFromHandleBuildCache(h, &tree_cr) } {
            Ok(r) => (r, None),
            Err(e) => {
                // Live pages (seen on Opera) change while one Subtree cache is built and the whole call fails with
                // "Unrecognized error". Fall back to caching one level at a time: slower, but it survives.
                dbg(&format!("subtree cache of {} failed ({}), walking per level", hv(h), e.message()));
                let l = self.level_cr.clone().map_or_else(|| ghost("UI Automation is unavailable"), Ok)?;
                let r = unsafe { uia.ElementFromHandle(h).and_then(|r| r.BuildUpdatedCache(&l)) }
                    .map_err(|e2| Fail::Ghost(format!("UIA could not read window {}: {}", hv(h), e2.message())))?;
                (r, Some(l))
            }
        };
        let wr = win::rect(h);
        let browser = self.st(h).browser;
        let mut w = Walk { h: hv(h), nodes: Vec::with_capacity(64), all, wr, path: HashSet::new(), budget: 5000, level, browser };
        let t1 = now_ms();
        w.walk(&root, 0, None, false);
        dbg(&format!("snapshot {}: build {} ms, walk {} ms, quiet={}", hv(h), t1 - now, now_ms() - t1, daemon::quiet_since(pid, now)));
        let mut nodes = w.nodes;
        if !all {
            collapse(&mut nodes);
        }
        let ws = self.st(h);
        for n in &nodes {
            if let Some(el) = &n.el {
                ws.els.insert(n.n, el.clone());
            }
        }
        let rc = Rc::new(nodes);
        ws.cache = Some(rc.clone());
        ws.cache_at = now;
        ws.cache_end = now_ms();
        ws.cache_all = all;
        Ok(rc)
    }

    /// Chromium/Electron build their accessibility tree only once a client asks for it.
    fn prime(&mut self, h: HWND) {
        if self.st(h).primed {
            return;
        }
        self.st(h).primed = true;
        let hosts: Vec<HWND> = win::child_windows(h).into_iter().filter(|&c| win::class_of(c) == "Chrome_RenderWidgetHostHWND").collect();
        if hosts.is_empty() {
            return;
        }
        self.st(h).chromium = true;
        self.st(h).browser = PROFILED.contains(&win::proc_name(win::pid(h)).to_lowercase().as_str()) || win::proc_name(win::pid(h)).eq_ignore_ascii_case("vivaldi");
        for c in hosts {
            unsafe {
                let mut r = 0usize;
                SendMessageTimeoutW(c, WM_GETOBJECT, WPARAM(0), LPARAM(-4), SMTO_ABORTIFHUNG, 200, Some(&mut r)); // OBJID_CLIENT
                SendMessageTimeoutW(c, WM_GETOBJECT, WPARAM(0), LPARAM(-25), SMTO_ABORTIFHUNG, 200, Some(&mut r)); // UiaRootObjectId
            }
        }
        let pid = win::pid(h);
        daemon::watch_pid(pid);
        // Event-driven: wait while the renderer keeps announcing new nodes (30 ms quiet, 1.5 s cap).
        let start = now_ms();
        std::thread::sleep(std::time::Duration::from_millis(20));
        while now_ms() - start < 1500 {
            if now_ms() - start.max(daemon::last_event(pid)) >= 30 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        dbg(&format!("primed chromium window {} in {} ms", hv(h), now_ms() - start));
    }

    fn tree(&mut self, cx: &mut Ctx, g: Args) -> R<()> {
        let h = g.win;
        let all = g.flags.contains("--all");
        let depth: u32 = match g.opt.get("--depth") {
            Some(v) => v.parse().map_err(|_| Fail::Usage("--depth needs a number".into()))?,
            None => 25,
        };
        let find = g.opt.get("--find").map(|s| s.to_lowercase());
        self.prime(h);
        let mut nodes = self.snapshot(h, all)?;
        // Already-open browser: the first walk only wakes the renderer. Re-poll (the cache invalidates on its
        // a11y events) until the document has children, 2 s at most.
        let browser = self.st(h).browser;
        let empty = |n: &[Node]| if browser { page_range(n, &win::title(h)).map(|(s, e)| e == s + 1).unwrap_or(true) } else { empty_doc(n) };
        let mut loading = self.st(h).chromium && empty(&nodes);
        let t0 = now_ms();
        while loading && now_ms() - t0 < 2000 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            nodes = self.snapshot(h, all)?;
            loading = empty(&nodes);
        }
        // A browser shows its address bar and the page; `--full` (or `--all`) the whole window.
        let full = all || g.flags.contains("--full");
        let mask = if browser && !full { view_mask(&nodes, &win::title(h)) } else { None };
        let base = mask.as_ref().and_then(|_| page_range(&nodes, &win::title(h))).map(|(s, _)| nodes[s].depth);
        let shown: Vec<&Node> = nodes.iter().enumerate().filter(|(i, n)| n.depth <= depth && mask.as_ref().map(|m| m[*i]).unwrap_or(true)).map(|(_, n)| n).collect();
        let mut s = String::new();
        match &find {
            None => {
                for n in shown.iter().filter(|n| !n.hidden) {
                    // The root is the only depth-0 line, so its children need no indent; a page indents under its document.
                    let ind = match base {
                        Some(b) => n.depth.saturating_sub(b),
                        None => n.depth.saturating_sub(1),
                    };
                    s.push_str(&" ".repeat(ind as usize));
                    s.push_str(&fmt(n));
                    s.push('\n');
                }
            }
            Some(f) => {
                // Names first: values (link URLs, field text) only when no name matches.
                let real: Vec<&&Node> = shown.iter().filter(|n| n.more.is_none()).collect();
                let mut hits: Vec<&&Node> = real.iter().copied().filter(|n| n.name.to_lowercase().contains(f)).collect();
                if hits.is_empty() {
                    hits = real.into_iter().filter(|n| n.value.as_deref().map(|v| v.to_lowercase().contains(f)).unwrap_or(false)).collect();
                }
                for n in hits {
                    s.push_str(&fmt(n));
                    s.push('\n');
                }
            }
        }
        if loading {
            s.push_str("\u{2026}page loading\n");
        }
        if !all {
            self.st(h).last = Some(nodes);
        }
        cx.out.push_str(&s);
        Ok(())
    }

    /// A target given as text: exact name (case-insensitive) first, then a unique substring. Not there yet: looked
    /// up again each time the app reports a UI change, for up to 2 s, so an action right after another rarely
    /// needs its own `wait`.
    fn by_name(&mut self, h: HWND, t: &str) -> R<u32> {
        let lt = t.to_lowercase();
        let pid = win::app_pid(h);
        let start = now_ms();
        loop {
            let since = now_ms();
            if let Some(n) = self.find_named(h, &lt, t)? {
                return Ok(n);
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
            while daemon::last_event(pid) < since && now_ms() - start < 2000 {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if now_ms() - start >= 2000 {
                break;
            }
        }
        ghost(self.not_found(h, t)?)
    }

    /// In a browser the page comes first, then its own controls (Back, tabs); everywhere the default view, then
    /// the full tree, where title bar buttons (Minimize, Maximize, Close) are.
    fn find_named(&mut self, h: HWND, lt: &str, t: &str) -> R<Option<u32>> {
        let nodes = self.snapshot(h, false)?;
        if self.st(h).browser {
            if let Some((s, e)) = page_range(&nodes, &win::title(h)) {
                if let Some(n) = pick(&nodes[s + 1..e], lt, t)? {
                    return Ok(Some(n));
                }
            }
        }
        if nodes.iter().any(|n| n.more.is_none() && n.depth > 0 && n.name.to_lowercase().contains(lt)) {
            return pick(&nodes, lt, t);
        }
        let all = self.snapshot(h, true)?;
        pick(&all, lt, t)
    }

    /// `no element named "X"`, the 3 nearest names (in a browser, from the page), and why a page may be empty.
    fn not_found(&mut self, h: HWND, t: &str) -> R<String> {
        let nodes = self.snapshot(h, false)?;
        let page = if self.st(h).browser { Some(page_range(&nodes, &win::title(h))) } else { None };
        let pool: &[Node] = match page {
            Some(Some((s, e))) if e > s + 1 => &nodes[s + 1..e],
            _ => &nodes,
        };
        let lt = t.to_lowercase();
        let mut near: Vec<(usize, &Node)> = pool.iter().filter(|n| n.more.is_none() && n.depth > 0 && !n.name.is_empty()).map(|n| (distance(&lt, &n.name.to_lowercase()), n)).collect();
        near.sort_by_key(|(d, n)| (*d, n.name.len()));
        let mut seen = HashSet::new();
        let near: Vec<String> = near.into_iter().filter(|(_, n)| seen.insert(n.name.to_lowercase())).take(3).map(|(_, n)| format!("e{} {} \"{}\"", n.n, n.role, clip(&n.name, 40))).collect();
        let mut m = format!("no element named \"{t}\"");
        if !near.is_empty() {
            m.push_str(&format!("; nearest: {}", near.join(", ")));
        }
        if matches!(page, Some(None)) || matches!(page, Some(Some((s, e))) if e == s + 1) {
            m.push_str("; ");
            m.push_str(NOT_EXPOSED);
        }
        Ok(m)
    }

    pub fn resolve(&mut self, h: HWND, r: &str) -> R<IUIAutomationElement> {
        let n = if is_ref(r, 'e') { r[1..].parse().unwrap_or(0) } else { self.by_name(h, r)? };
        let alive = |el: &IUIAutomationElement| unsafe { el.CurrentProcessId().is_ok() };
        if let Some(el) = self.st(h).els.get(&n).cloned() {
            if alive(&el) {
                return Ok(el);
            }
            self.st(h).els.remove(&n);
        }
        let Some(rid) = rid_of(hv(h), n) else {
            return ghost(format!("unknown ref {r}; run: ghost tree {}", hv(h)));
        };
        // Refs are keyed by RuntimeId: a fresh full walk puts the element back in the map if it still exists.
        self.st(h).cache = None;
        self.snapshot(h, true)?;
        if let Some(el) = self.st(h).els.get(&n).cloned() {
            if alive(&el) {
                return Ok(el);
            }
        }
        // HWND-backed elements ([42, hwnd]) can drop out of the tree while hidden; reach them directly.
        if rid.len() == 2 && rid[0] == 42 && unsafe { IsChild(h, hwnd(rid[1] as isize)).as_bool() } {
            let el = unsafe { self.uia()?.ElementFromHandle(hwnd(rid[1] as isize))? };
            self.st(h).els.insert(n, el.clone());
            return Ok(el);
        }
        ghost(format!("ref {r} is gone (UI changed); run: ghost tree {}", hv(h)))
    }

    /// Bare `read` in a browser: `page "title" url`, then the page's text (its TextPattern), capped.
    fn read_page(&mut self, cx: &mut Ctx, h: HWND) -> R<()> {
        let nodes = self.snapshot(h, false)?;
        let title = win::title(h);
        let Some((s, e)) = page_range(&nodes, &title) else { return ghost(NOT_EXPOSED) };
        let d = &nodes[s];
        cx.line(&format!("page \"{}\" {}", clip(&d.name, 80), d.value.as_deref().unwrap_or("")));
        let text = d.el.as_ref().and_then(|el| unsafe {
            let tp = el.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId).ok()?;
            tp.DocumentRange().ok()?.GetText((READ_CAP + 1) as i32).ok().map(|b| b.to_string())
        });
        // No text pattern: the page's own names, in order.
        let text = text.unwrap_or_else(|| nodes[s + 1..e].iter().filter(|n| n.more.is_none() && !n.name.is_empty()).map(|n| n.name.as_str()).collect::<Vec<_>>().join("\n"));
        let text = text.trim();
        if !text.is_empty() {
            cx.line(&cap_text(text));
        } else if e == s + 1 {
            return ghost(NOT_EXPOSED);
        }
        Ok(())
    }

    /// Bare `read` outside a browser: the window's content in a few lines. Text, the values of edits, the names
    /// of list and grid items (not their columns); no title bar, menus, toolbars or ribbon, navigation tree,
    /// header, status or scroll bars. Each line once, at most READ_CAP chars.
    fn read_window(&mut self, cx: &mut Ctx, h: HWND) -> R<()> {
        let nodes = self.snapshot(h, false)?;
        let mut out: Vec<String> = vec![];
        let mut seen = HashSet::new();
        let mut skip: Option<u32> = None; // depth of the subtree being left out
        for n in nodes.iter() {
            if let Some(d) = skip {
                if n.depth > d {
                    continue;
                }
                skip = None;
            }
            if n.hidden {
                continue;
            }
            if let Some(m) = &n.more {
                out.push(m.clone());
                continue;
            }
            let line = match n.role.as_str() {
                "title" | "menubar" | "menu" | "tbar" | "appbar" | "sbar" | "sb" | "tree" | "hdr" | "tip" => {
                    skip = Some(n.depth);
                    continue;
                }
                "li" | "row" | "tvi" => {
                    skip = Some(n.depth);
                    n.name.clone()
                }
                "text" => n.name.clone(),
                "edit" | "doc" | "combo" => match n.value.as_deref().map(str::trim) {
                    Some(v) if !v.is_empty() && n.name.is_empty() => v.to_string(),
                    Some(v) if !v.is_empty() => format!("{}: {v}", n.name),
                    _ => continue,
                },
                _ => continue,
            };
            let line = line.trim().to_string();
            if !line.is_empty() && seen.insert(line.clone()) {
                out.push(line);
            }
        }
        if out.is_empty() {
            return ghost("nothing to read: no text, values or items here; ghost tree lists the controls");
        }
        cx.line(&cap_text(&out.join("
")));
        Ok(())
    }

    fn read(&mut self, cx: &mut Ctx, el: &IUIAutomationElement) -> R<()> {
        let t = unsafe {
            if let Ok(tp) = el.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) {
                tp.DocumentRange()?.GetText((READ_CAP + 1) as i32)?.to_string()
            } else if let Ok(vp) = el.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
                vp.CurrentValue()?.to_string()
            } else {
                el.CurrentName()?.to_string()
            }
        };
        cx.line(&cap_text(&t));
        Ok(())
    }

    // ---------- actions ----------
    fn act(&mut self, cx: &mut Ctx, cmd: &str, g: Args) -> R<()> {
        daemon::wait_rest();
        let h = g.win;
        let quiet = g.flags.contains("--quiet");
        let mut rest = g.pos.clone();
        if g.flags.contains("--append") {
            rest.push("--append".into());
        }
        let pid = win::app_pid(h);
        let frame_pid = win::pid(h); // differs for a UWP frame; the foreground guard sees the frame's process
        daemon::watch_pid(pid);
        if frame_pid != pid {
            daemon::mark_act(frame_pid);
        }
        if !quiet && self.st(h).last.is_none() {
            let base = self.snapshot(h, false)?; // baseline for the diff
            self.st(h).last = Some(base);
        }
        // Browser: what page this is before the action, to see a navigation after it.
        let browser = self.st(h).browser;
        let before = if browser { Some(self.snapshot(h, false)?) } else { None };
        let before_id = before.as_ref().and_then(|b| page_id(b, &win::title(h)));
        let mut nav_ms = 0u64; // how long a navigation may take to start after this action
        let mut is_btn = false;
        if browser && ((cmd == "type" && !rest.is_empty()) || (cmd == "key" && rest.len() == 2)) {
            // The browser's own text boxes (address bar, find bar) take the foreground when given text or keys.
            let el = self.resolve(h, &rest[0])?;
            if chrome_edit(self.uia()?, &el) {
                return ghost("background unavailable: the browser's own address bar takes the foreground when typed into; use: ghost go <url>");
            }
        }
        daemon::mark_act(pid);
        let (m, check) = match cmd {
            "click" => {
                if rest.len() != 1 {
                    return usage(usage_line("click", cx.step.is_some()));
                }
                if is_ref(&rest[0], 'o') {
                    self.ocr_click(h, &rest[0])?
                } else {
                    let el = self.resolve(h, &rest[0])?;
                    refuse_disabled(h, Some(&el))?;
                    if browser {
                        (nav_ms, is_btn) = nav_expect(&el, before_id.as_ref().map(|(_, u)| u.as_str()).unwrap_or(""));
                    }
                    self.sendable(h)?;
                    let check = check_of(&el).filter(|_| unsafe { [UIA_CheckBoxControlTypeId, UIA_RadioButtonControlTypeId].contains(&el.CurrentControlType().unwrap_or_default()) });
                    match self.cdp_on(h, &el, "click", |s, n| {
                        s.call("DOM.scrollIntoViewIfNeeded", &format!(r#"{{"backendNodeId":{n}}}"#)).map_err(|e| crate::cdp::CallErr::Unsent(e.to_string()))?;
                        crate::cdp::on_node(s, n, "function(){ this.click(); }")
                    }) {
                        Some(r) => r.map(|_| ("cdp_click".to_string(), check))?,
                        None => click_el(self.uia()?, h, &el)?,
                    }
                }
            }
            "type" => self.type_text(h, &rest)?,
            _ => {
                if rest.is_empty() || rest.len() > 2 {
                    return usage(usage_line("key", cx.step.is_some()));
                }
                if browser && rest[rest.len() - 1].eq_ignore_ascii_case("enter") {
                    nav_ms = 3000; // Enter in a page form usually submits it
                }
                let target = if rest.len() == 2 {
                    let el = self.resolve(h, &rest[0])?;
                    refuse_disabled(h, Some(&el))?;
                    key_target_of(self.uia()?, h, &el)
                } else {
                    refuse_disabled(h, None)?;
                    key_target(h)
                };
                if !unsafe { IsWindowEnabled(target).as_bool() } {
                    return ghost(format!("disabled: {}", win::title(target)));
                }
                self.sendable(h)?;
                crate::keys::key(target, &rest[rest.len() - 1])?;
                ("posted".to_string(), None)
            }
        };
        dbg(&format!("{cmd} {}: {m}", hv(h)));
        if std::env::var_os("GHOST_DEBUG").is_some() {
            cx.line(&format!("ok {m}"));
        }
        daemon::settle(pid);
        daemon::mark_act(pid);
        // Success has to be true: a toggle, select or expand that left the state as it was is an error.
        if let Some(c) = &check {
            let t = now_ms();
            while !c.done() {
                if now_ms() - t > c.wait_ms() {
                    return match c {
                        Check::Gone(w) => ghost(self.still_open(*w)),
                        Check::Text(..) => ghost("no effect: the text did not arrive"),
                        _ => ghost("no effect: the control's state did not change"),
                    };
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        // Browser: a new document or URL is a navigation. Wait for the new page to settle and show it instead
        // of a diff against the old one.
        if let (Some(b), Some(id)) = (&before, &before_id) {
            if let Some(nodes) = self.await_nav(h, b, id, nav_ms, is_btn)? {
                let k = cx.step.map(|k| format!("{k} ")).unwrap_or_default();
                if !quiet {
                    cx.out.push_str(&format!("{k}nav {}", page_text(&nodes, &win::title(h), NAV_CAP, None)));
                }
                self.st(h).last = Some(nodes);
                return daemon::fg_check(frame_pid).map_err(Fail::Ghost);
            }
        }
        // A success always says something: what changed, or "no change" when the effect can't be seen.
        if !quiet {
            let k = cx.step.map(|k| format!("{k} ")).unwrap_or_default();
            if !win::is_window(h) {
                cx.line(&format!("{k}window closed"));
            } else if self.diff(cx, h)? == 0 {
                cx.line(&format!("{k}no change"));
            }
        }
        daemon::fg_check(frame_pid).map_err(Fail::Ghost)
    }

    /// After a browser action: did the page change (title or URL)? Polls up to `wait_ms` for a navigation to
    /// start (a button that changed the page in place ends that early), then waits until the new page has
    /// content and UIA has been quiet for 500 ms, 10 s at most. None: no navigation.
    fn await_nav(&mut self, h: HWND, before: &[Node], id: &(String, String), wait_ms: u64, is_btn: bool) -> R<Option<Rc<Vec<Node>>>> {
        let pid = win::app_pid(h);
        let t0 = now_ms();
        let content = |n: &[Node]| -> Vec<(u32, String, String)> {
            match page_range(n, &win::title(h)) {
                Some((s, e)) => n[s + 1..e].iter().map(|x| (x.n, x.name.clone(), x.value.clone().unwrap_or_default())).collect(),
                None => vec![],
            }
        };
        let was = content(before);
        loop {
            let nodes = self.snapshot(h, false)?;
            if page_id(&nodes, &win::title(h)).as_ref().map(|x| x != id).unwrap_or(false) {
                break;
            }
            if now_ms() - t0 >= wait_ms || (is_btn && content(&nodes) != was) {
                return Ok(None);
            }
            daemon::mark_act(pid); // the page may still grab focus: keep the guard and foreground lock on
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Ok(Some(self.settle_page(h, pid, t0)?))
    }

    /// Until the page document has content and the app has sent no UI event for 500 ms; 10 s after `t0` at most.
    fn settle_page(&mut self, h: HWND, pid: u32, t0: u64) -> R<Rc<Vec<Node>>> {
        loop {
            let nodes = self.snapshot(h, false)?;
            let page = page_range(&nodes, &win::title(h));
            let now = now_ms();
            // Quiet counts from t0 too: events from before the hook was set were never seen.
            let quiet = now.saturating_sub(daemon::last_event(pid).max(t0));
            // Content and 500 ms quiet; a page that stays empty (about:blank) after 2 s quiet is what it is.
            let ready = page.map(|(s, e)| (e > s + 1 && quiet >= 500) || quiet >= 2000).unwrap_or(false);
            if ready || now - t0 >= 10_000 {
                dbg(&format!("page {} settled in {} ms (ready={ready})", hv(h), now - t0));
                return Ok(nodes);
            }
            daemon::mark_act(win::pid(h));
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    /// Prints the changes since the caller's last look and returns how many there were. A browser's diff covers
    /// what its tree shows: the address bar and the page.
    fn diff(&mut self, cx: &mut Ctx, h: HWND) -> R<usize> {
        let now = self.snapshot(h, false)?;
        let last = self.st(h).last.clone().unwrap_or_default();
        let browser = self.st(h).browser;
        let title = win::title(h);
        // In a browser the document line only repeats title and URL (a navigation reports those itself).
        let shown = |nodes: &[Node]| -> Vec<bool> {
            let m = if browser { view_mask(nodes, &title) } else { None };
            let mut m = m.unwrap_or_else(|| vec![true; nodes.len()]);
            if browser {
                m.iter_mut().zip(nodes).filter(|(_, n)| n.role == "doc").for_each(|(k, _)| *k = false);
            }
            m
        };
        let (mn, ml) = (shown(&now), shown(&last));
        let old: HashMap<u32, &Node> = last.iter().zip(&ml).filter(|(n, &k)| k && n.more.is_none()).map(|(n, _)| (n.n, n)).collect();
        let mut lines = vec![];
        let mut seen = HashSet::new();
        for n in now.iter().zip(&mn).filter(|(n, &k)| k && n.more.is_none()).map(|(n, _)| n) {
            seen.insert(n.n);
            match old.get(&n.n) {
                None => lines.push(format!("+ {}", fmt(n))),
                Some(o) => {
                    // A page moves focus on every click and type: in a browser that alone isn't news.
                    let flags = if browser { o.flags.replace("[*]", "") != n.flags.replace("[*]", "") } else { o.flags != n.flags };
                    if o.name != n.name || o.value.as_deref().unwrap_or("") != n.value.as_deref().unwrap_or("") || flags {
                        lines.push(format!("~ {}", fmt(n)));
                    }
                }
            }
        }
        for o in last.iter().zip(&ml).filter(|(n, &k)| k && n.more.is_none() && !seen.contains(&n.n)).map(|(n, _)| n) {
            lines.push(format!("- {}", fmt(o)));
        }
        self.st(h).last = Some(now);
        let k = cx.step.map(|k| k.to_string()).unwrap_or_default(); // in `run`: "2~ e4 ..." names the step
        for l in lines.iter().take(30) {
            cx.line(&format!("{k}{l}"));
        }
        if lines.len() > 30 {
            cx.line(&format!("{k}\u{2026} {} more changes", lines.len() - 30));
        }
        Ok(lines.len())
    }

    /// A window that should have closed: what it shows now, and any dialog it opened (a "save changes?" prompt).
    fn still_open(&mut self, h: HWND) -> String {
        let mut t = Ctx { out: String::new(), err: String::new(), cur: 0, cwd: String::new(), implicit_run: false, step: None, launched: false };
        if self.st(h).last.is_some() {
            let _ = self.diff(&mut t, h);
        }
        let mut m = String::from("still open");
        for l in t.out.lines() {
            m.push('\n');
            m.push_str(l);
        }
        for w in win::top_windows() {
            if win::visible(w) && unsafe { GetWindow(w, GW_OWNER) }.map(|o| o == h).unwrap_or(false) {
                m.push_str(&format!("\n+ dlg \"{}\" (hwnd {})", clip(&win::title(w), 60), hv(w)));
            }
        }
        m
    }

    fn ocr_click(&mut self, h: HWND, r: &str) -> R<(String, Option<Check>)> {
        let n: usize = r[1..].parse().unwrap_or(0);
        let ocr = &self.st(h).ocr;
        if n < 1 || n > ocr.len() {
            return ghost(format!("unknown ref {r}; run: ghost ocr {}", hv(h)));
        }
        let (x, y) = ocr[n - 1];
        let wr = win::rect(h);
        let screen = POINT { x: wr.left + x, y: wr.top + y };
        // Deepest visible child window under the point.
        let mut cur = h;
        for _ in 0..32 {
            let p = win::screen_to_client(cur, screen);
            let c = unsafe { ChildWindowFromPointEx(cur, p, CWP_SKIPINVISIBLE | CWP_SKIPTRANSPARENT) };
            if c.0.is_null() || c == cur {
                break;
            }
            cur = c;
        }
        let el = unsafe { self.uia()?.ElementFromHandle(cur).ok() };
        let ct = el.as_ref().and_then(|e| unsafe { e.CurrentControlType().ok() }).unwrap_or_default();
        refuse_disabled(h, el.as_ref())?;
        if !unsafe { IsWindowEnabled(cur).as_bool() } {
            return ghost(format!("disabled: {}", win::title(cur)));
        }
        self.sendable(h)?;
        if ct == UIA_ButtonControlTypeId && is_push_button(cur) {
            bn_clicked(cur);
            return Ok(("bn_clicked".into(), None));
        }
        let check = el.as_ref().filter(|_| ct == UIA_CheckBoxControlTypeId || ct == UIA_RadioButtonControlTypeId).and_then(check_of);
        post_click(cur, screen);
        Ok(("postmessage".into(), check))
    }

    fn type_text(&mut self, h: HWND, rest: &[String]) -> R<(String, Option<Check>)> {
        if rest.is_empty() {
            return usage("usage: type <win> <eN> <text> [--append]");
        }
        let el = self.resolve(h, &rest[0])?;
        refuse_disabled(h, Some(&el))?;
        unsafe {
            let ct = el.CurrentControlType().unwrap_or_default();
            let writable = el
                .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                .map(|vp| !vp.CurrentIsReadOnly().map(|b| b.as_bool()).unwrap_or(true))
                .unwrap_or(false);
            if !writable && ct != UIA_EditControlTypeId && ct != UIA_DocumentControlTypeId {
                return ghost(format!("not editable: {}", el.CurrentName().map(|b| b.to_string()).unwrap_or_default()));
            }
        }
        self.sendable(h)?;
        // Password fields can't be read back, so their text goes unchecked.
        let secret = unsafe { el.CurrentIsPassword().map(|b| b.as_bool()).unwrap_or(false) };
        let check = |hw: HWND, want: String, suffix: bool| (!secret).then(|| Check::Text(el.clone(), hw, want, suffix));
        // Windows PowerShell 5 drops "" arguments to native commands, so a missing text means "".
        let (text, i0) = if rest.len() > 1 && rest[1] != "--append" { (rest[1].clone(), 2) } else { (String::new(), 1) };
        let mut append = false;
        for r in &rest[i0..] {
            if r == "--append" {
                append = true;
            } else {
                return usage(format!("type: unknown option {r}"));
            }
        }
        // Ghost's own browser: focus, clear and insert over the DevTools protocol (UIA SetValue activated Edge).
        if lock(&BROWSER).contains_key(&hv(h)) {
            let cur = unsafe { el.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId).and_then(|vp| vp.CurrentValue()).map(|b| b.to_string()).unwrap_or_default() };
            let nv = if append { format!("{cur}{text}") } else { text.clone() };
            let prep = if append {
                "function(){ try { const n = this.value.length; this.setSelectionRange(n, n); } catch (e) {} }"
            } else {
                "function(){ if ('value' in this) { this.value = ''; } else { this.textContent = ''; } this.dispatchEvent(new Event('input', { bubbles: true })); }"
            };
            let r = self.cdp_on(h, &el, "type", |s, n| {
                let safe = |e: crate::cdp::CallErr| crate::cdp::CallErr::Unsent(e.to_string());
                s.call("DOM.focus", &format!(r#"{{"backendNodeId":{n}}}"#)).map_err(safe)?;
                crate::cdp::on_node(s, n, prep).map_err(safe)?;
                if text.is_empty() {
                    return Ok(());
                }
                // Replacing: a failed insert is redone whole by UIA below. Appending: it may have landed, so it stops.
                s.call("Input.insertText", &format!(r#"{{"text":{}}}"#, crate::cdp::json_str(&text))).map(|_| ()).map_err(|e| if append { e } else { safe(e) })
            });
            if let Some(r) = r {
                return r.map(|_| ("cdp_insert".to_string(), check(HWND::default(), nv, false)));
            }
        }
        unsafe {
            // A native edit control takes WM_SETTEXT with no focus change; Windows' UIA proxy for it focuses (and
            // can activate) the window on SetValue. So native edits skip ValuePattern.
            let own = el.CurrentNativeWindowHandle().unwrap_or_default();
            let native_edit = !own.0.is_null() && win::class_of(own).to_lowercase().contains("edit");
            if native_edit {
            } else if let Ok(vp) = el.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
                if !vp.CurrentIsReadOnly().map(|b| b.as_bool()).unwrap_or(true) {
                    let nv = if append { format!("{}{}", vp.CurrentValue().map(|b| b.to_string()).unwrap_or_default(), text) } else { text.clone() };
                    let b = BSTR::from(nv.as_str());
                    if shielded(h, || vp.SetValue(&b)).is_ok() {
                        return Ok(("value".into(), check(HWND::default(), nv, false)));
                    }
                }
            }
            // Native edit HWND: the element itself, else the first descendant that has one.
            let mut hw = el.CurrentNativeWindowHandle().unwrap_or_default();
            if hw.0.is_null() {
                if let Ok(c) = self.uia()?.CreateTrueCondition() {
                    if let Ok(arr) = el.FindAll(TreeScope_Descendants, &c) {
                        for i in 0..arr.Length().unwrap_or(0) {
                            if let Ok(d) = arr.GetElement(i) {
                                let n = d.CurrentNativeWindowHandle().unwrap_or_default();
                                if !n.0.is_null() {
                                    hw = n;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            if !hw.0.is_null() {
                let v = if append { format!("{}{}", get_text(hw), text) } else { text.clone() };
                let w = win::wide(&v);
                let mut res = 0usize;
                let ok = SendMessageTimeoutW(hw, WM_SETTEXT, WPARAM(0), LPARAM(w.as_ptr() as isize), SMTO_ABORTIFHUNG, 5000, Some(&mut res));
                if ok.0 == 0 || res == 0 {
                    return ghost(format!("WM_SETTEXT rejected by window {}", hv(hw)));
                }
                // Caret to the end like real typing, so a following `key` appends. EM_SETSEL means nothing to non-edit classes.
                if win::class_of(hw).to_lowercase().contains("edit") {
                    let len = v.encode_utf16().count();
                    SendMessageTimeoutW(hw, 0xB1, WPARAM(len), LPARAM(len as isize), SMTO_ABORTIFHUNG, 5000, Some(&mut res));
                }
                return Ok(("wm_settext".into(), check(hw, v, false)));
            }
            if !append {
                return ghost("background unavailable: no writable ValuePattern and no native edit window; try --append (types at the caret)");
            }
            let up = hwnd_up(self.uia()?, &el, h);
            for c in text.encode_utf16() {
                let _ = PostMessageW(Some(up), WM_CHAR, WPARAM(c as usize), LPARAM(1));
            }
            Ok(("wm_char".into(), check(HWND::default(), text, true)))
        }
    }

    // ---------- wait ----------
    fn wait(&mut self, cx: &mut Ctx, g: Args) -> R<()> {
        let h = g.win;
        let text = g.pos[0].to_lowercase();
        let gone = g.flags.contains("--gone");
        let timeout: u64 = match g.opt.get("--timeout") {
            Some(v) => v.parse().map_err(|_| Fail::Usage("--timeout needs ms".into()))?,
            None => 5000,
        };
        let start = now_ms();
        self.prime(h);
        let browser = self.st(h).browser;
        loop {
            let nodes = self.snapshot(h, false)?;
            // In a browser only the page counts: the tab and window titles show a page's title before its content.
            let range = if browser { page_range(&nodes, &win::title(h)).map(|(s, e)| (s + 1, e)).unwrap_or((0, 0)) } else { (0, nodes.len()) };
            let found = nodes[range.0..range.1].iter().any(|n| n.more.is_none() && (n.name.to_lowercase().contains(&text) || n.value.as_deref().map(|v| v.to_lowercase().contains(&text)).unwrap_or(false)));
            let el = now_ms() - start;
            if found != gone {
                cx.line(&format!("ok {el}ms"));
                return Ok(());
            }
            if el >= timeout {
                let mut m = format!("timeout after {el}ms: \"{}\" {}", g.pos[0], if gone { "still there" } else { "not found" });
                if browser {
                    match page_id(&nodes, &win::title(h)) {
                        Some((t, u)) => m.push_str(&format!("; page \"{}\" {u}", clip(&t, 80))),
                        None => m.push_str(&format!("; {NOT_EXPOSED}")),
                    }
                    if range.1 == range.0 && page_range(&nodes, &win::title(h)).is_some() {
                        m.push_str(&format!("; {NOT_EXPOSED}"));
                    }
                }
                return ghost(m);
            }
            std::thread::sleep(std::time::Duration::from_millis(30.min((timeout - el).max(1))));
        }
    }

    // ---------- launch ----------
    /// A browser given a URL prints `hwnd pid process`, then `page "title" url` and the page once it has loaded.
    fn launch(&mut self, cx: &mut Ctx, a: &[String]) -> R<()> {
        let (found, url) = self.launch_win(cx, a)?;
        if !url {
            cx.line(&win::line(found));
            return Ok(());
        }
        let nodes = self.settle_page(found, win::app_pid(found), now_ms())?;
        cx.line(&format!("{} {} {}", hv(found), win::pid(found), win::proc_name(win::pid(found))));
        cx.out.push_str(&format!("page {}", page_text(&nodes, &win::title(found), PAGE_CAP, a[2..].iter().find(|x| !x.starts_with('-')).map(|s| s.as_str()))));
        Ok(())
    }

    /// `go <url>`: typing into a browser's address bar takes the foreground (UIA SetValue there activates the
    /// window, measured). On ghost's own browsers the tab navigates in place over the DevTools protocol (cdp.rs);
    /// without it (a browser started before ghost opened the port) the URL is handed to the running instance,
    /// which opens a new tab, and the old tab is closed. Both keep the window, its cookies and session.
    fn go(&mut self, cx: &mut Ctx, g: Args) -> R<()> {
        let h = g.win;
        let Some(br) = lock(&BROWSER).get(&hv(h)).cloned() else {
            return ghost("go works in a browser ghost launched with its own profile (no --user-data-dir); start one at the page: ghost launch msedge <url>");
        };
        daemon::wait_rest();
        let pid = win::pid(h);
        let back = daemon::handback_from(unsafe { GetForegroundWindow() });
        daemon::mark_act(pid);
        let t0 = now_ms();
        dbg(&format!("go: {}", daemon::lock_state()));
        // Any activation while navigating is taken straight back.
        let hold = |pid: u32| {
            let cur = unsafe { GetForegroundWindow() };
            if cur != back && !cur.0.is_null() && (win::pid(cur) == pid || ghost_window(cur)) {
                dbg(&format!("go: {} ({}) in front at +{} ms, {}", hv(cur), win::class_of(cur), now_ms() - t0, daemon::lock_state()));
                win::restore_foreground(back, cur);
            }
            daemon::mark_act(pid);
        };
        match crate::cdp::navigate(&br.dir, &win::title(h), &g.pos[0]) {
            Ok(note) => dbg(&format!("go: path cdp, replied +{} ms{}", now_ms() - t0, note.map(|n| format!(", {n}")).unwrap_or_default())),
            Err(why) => {
                dbg(&format!("go: path hand-off, cdp unavailable: {why}"));
                self.go_handoff(cx, h, &br, &g.pos[0], t0, &hold)?;
            }
        }
        // Settle the new page while still reclaiming the foreground each poll: an empty page (about:blank) waits up
        // to 2 s for quiet, and the browser would otherwise hold the foreground for that whole time.
        let apid = win::app_pid(h);
        let nodes = loop {
            hold(pid);
            let nodes = self.snapshot(h, false)?;
            let page = page_range(&nodes, &win::title(h));
            let now = now_ms();
            let quiet = now.saturating_sub(daemon::last_event(apid).max(t0));
            let ready = page.map(|(s, e)| (e > s + 1 && quiet >= 500) || quiet >= 2000).unwrap_or(false);
            if ready || now - t0 >= 10_000 {
                break nodes;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        hold(pid);
        // The handoff's short-lived process and the new tab can activate the browser a tick or two after settle.
        // A brief synchronous poll catches that in a few ms, rather than waiting on the async WinEvent hook (whose
        // out-of-context delivery was measured at 60-110 ms under load, long enough to look like a flash).
        let tail = now_ms();
        while now_ms() - tail < 150 {
            hold(pid);
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        self.st(h).last = Some(nodes.clone());
        let k = cx.step.map(|k| format!("{k} ")).unwrap_or_default();
        cx.line(&format!("{} {} {}", hv(h), pid, win::proc_name(pid)));
        cx.out.push_str(&format!("{k}nav {}", page_text(&nodes, &win::title(h), NAV_CAP, Some(&g.pos[0]))));
        daemon::fg_check(pid).map_err(Fail::Ghost)
    }

    /// Runs `f` on the page element `el` names, over the DevTools protocol, in a browser ghost launched. None: not
    /// available here (not ghost's browser, a role or name CDP can't match to exactly one element, the port gone,
    /// or a step that failed before it changed anything): the caller uses UIA. Some(Err): it failed after acting.
    fn cdp_on(&mut self, h: HWND, el: &IUIAutomationElement, what: &str, f: impl FnOnce(&mut crate::cdp::Session, u64) -> Result<(), crate::cdp::CallErr>) -> Option<R<()>> {
        let dir = lock(&BROWSER).get(&hv(h)).map(|b| b.dir.clone())?;
        let (name, ct) = unsafe { (el.CurrentName().map(|b| b.to_string()).unwrap_or_default(), el.CurrentControlType().unwrap_or_default()) };
        let roles = ax_roles(ct);
        let t0 = now_ms();
        let skip = |why: String| {
            dbg(&format!("{what} {}: path uia, cdp {why}", hv(h)));
            None
        };
        if name.is_empty() || roles.is_empty() {
            return skip(format!("can't match \"{name}\" (control type {})", ct.0));
        }
        let mut s = match crate::cdp::Session::page(&dir, &win::title(h)) {
            Ok(s) => s,
            Err(e) => return skip(format!("unavailable: {e}")),
        };
        let node = match crate::cdp::find(&mut s, &name, roles) {
            Ok(Some(n)) => n,
            Ok(None) => return skip(format!("found no single {roles:?} named \"{name}\"")),
            Err(e) => return skip(format!("query failed: {e}")),
        };
        match f(&mut s, node) {
            Ok(()) => {
                dbg(&format!("{what} {}: path cdp, node {node}, {} ms", hv(h), now_ms() - t0));
                Some(Ok(()))
            }
            Err(crate::cdp::CallErr::Unsent(e)) => skip(format!("step failed before acting: {e}")),
            Err(crate::cdp::CallErr::Sent(e)) => Some(ghost(format!("{what} over DevTools failed after it was sent (it may still apply): {e}"))),
        }
    }

    /// `go`'s fallback: the URL handed to the running instance (a new tab), then the old tab closed in place.
    fn go_handoff(&mut self, cx: &mut Ctx, h: HWND, br: &Br, url: &str, t0: u64, hold: &dyn Fn(u32)) -> R<()> {
        let pid = win::pid(h);
        let tabs = |n: &[Node]| -> Vec<u32> { n.iter().filter(|x| x.role == "ti").map(|x| x.n).collect() };
        let old = tabs(&self.snapshot(h, true)?);
        // The same exe on the same profile: Chromium hands the URL to the running instance, which opens it in a
        // new tab of this window, so cookies and the session stay. (SetValue on the address bar activates the
        // browser: see chrome_edit.)
        let cmd = format!("{} {} {}", quote(&br.exe), quote(&format!("--user-data-dir={}", br.dir)), quote(url));
        spawn(&cmd, &cx.cwd)?;
        dbg(&format!("go: spawned +{} ms", now_ms() - t0));
        loop {
            hold(pid);
            let now = tabs(&self.snapshot(h, true)?);
            if now.iter().any(|n| !old.contains(n)) {
                dbg(&format!("go: new tab +{} ms", now_ms() - t0));
                break;
            }
            if now_ms() - t0 > 10_000 {
                return ghost("the browser did not open the page in a new tab within 10 s");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // The old tab closes in place: its own close button, invoked under the shield.
        let all = self.snapshot(h, true)?;
        for (i, n) in all.iter().enumerate().filter(|(_, n)| n.role == "ti" && old.contains(&n.n)) {
            let close = all[i + 1..].iter().take_while(|c| c.depth > n.depth).find(|c| c.role == "btn").and_then(|c| c.el.clone());
            let Some(el) = close else { return ghost(format!("opened the page in a new tab, but the old tab has no close button: {}", clip(&n.name, 60))) };
            let p = unsafe { el.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) }?;
            shielded(h, || unsafe { p.Invoke() })?;
            dbg(&format!("go: old tab close invoked +{} ms", now_ms() - t0));
            hold(pid);
        }
        loop {
            hold(pid);
            let now = tabs(&self.snapshot(h, true)?);
            if !now.iter().any(|n| old.contains(n)) {
                dbg(&format!("go: old tab closed +{} ms", now_ms() - t0));
                break;
            }
            if now_ms() - t0 > 12_000 {
                return ghost("opened the page in a new tab, but the old tab is still open");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Ok(())
    }

    /// Starts the program parked and returns its window, and whether it is a browser given a URL.
    fn launch_win(&mut self, cx: &mut Ctx, a: &[String]) -> R<(HWND, bool)> {
        let uri = is_uri(&a[1]);
        // `launch msedge`: a bare name that isn't on PATH is looked up in App Paths, as the Run box does.
        let exe = if uri { None } else { app_path(&a[1]) }.unwrap_or_else(|| a[1].clone());
        let base = std::path::Path::new(&exe).file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
        // Only a target that opened a UWP frame before (calc, an AUMID, ms-settings:) waits after a UWP close.
        let target = a[1].to_lowercase();
        if let Some(t) = (*lock(&UWP_CLOSED)).filter(|_| lock(&UWP_TARGETS).contains(&target)) {
            let left = (t + UWP_COOLDOWN_MS).saturating_sub(now_ms());
            if left > 0 {
                dbg(&format!("launch: waiting {left} ms after a UWP close"));
                std::thread::sleep(std::time::Duration::from_millis(left));
            }
        }
        daemon::wait_rest();
        // A UWP app is activated by the system at launch. With the owner active ghost can't hold the foreground
        // lock, and then nothing it does (SetForegroundWindow, hiding, minimizing) takes the foreground back from the
        // new frame (budget N2, 2026-10-18: lock refused with error 5 in every failing run, taken in every passing
        // one). Windows refuses the lock while the owner types, so such a launch waits for a second without input
        // and tries again, 20 s at most, and is refused rather than steal the foreground.
        if lock(&UWP_TARGETS).contains(&target) || uwp_target(&a[1]) {
            // Windows grants the lock only after ForegroundLockTimeout without input (200 s by default, read here):
            // a wait that can't reach it in the 20 s stops at once instead of idling for nothing.
            let mut fglt = 0u32;
            let _ = unsafe { SystemParametersInfoW(SPI_GETFOREGROUNDLOCKTIMEOUT, 0, Some(&mut fglt as *mut u32 as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)) };
            let t = now_ms();
            daemon::mark_act(0);
            let mut why = String::new();
            while !daemon::lock_held() {
                let (left, idle) = (20_000u64.saturating_sub(now_ms() - t), idle_ms());
                if left == 0 || (fglt as u64).saturating_sub(idle) > left {
                    why = format!("idle {idle} ms, the lock needs {fglt} ms without input");
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                if idle_ms() >= 1000 {
                    daemon::mark_act(0);
                }
            }
            let waited = now_ms() - t;
            if !daemon::lock_held() {
                dbg(&format!("launch {target}: uwp, refused after {waited} ms ({why}), foreground lock not available ({})", daemon::lock_state()));
                return ghost("background unavailable: you are using the PC, and this app takes the foreground when it starts; nothing was launched. Retry when the PC is idle");
            }
            dbg(&format!("launch {target}: uwp, lock held after {waited} ms"));
        }
        let before: HashSet<isize> = win::top_windows().into_iter().map(hv).collect();
        let fg = unsafe { GetForegroundWindow() };
        let browser = BROWSERS.contains(&base.as_str());
        // Chromium keeps only the last --disable-features, so the caller's list is merged into ghost's one.
        let mut off = vec!["CalculateNativeWinOcclusion".to_string()];
        let mut cmd = quote(&exe);
        for x in &a[2..] {
            if let Some(v) = x.strip_prefix("--disable-features=").filter(|_| browser) {
                off.extend(v.split(',').filter(|f| !f.is_empty()).map(String::from));
                continue;
            }
            cmd.push(' ');
            cmd.push_str(&quote(x));
        }
        // Browsers ghost starts: build the page a11y tree eagerly and keep rendering while parked/occluded.
        if browser {
            if base == "msedge" {
                off.push("msImplicitSignin".into()); // Edge signs a new profile into the Windows account by itself
            }
            cmd.push_str(" --force-renderer-accessibility --disable-backgrounding-occluded-windows --disable-renderer-backgrounding ");
            cmd.push_str(&quote(&format!("--disable-features={}", off.join(","))));
        }
        // Its own throwaway profile, so it is always a new instance and never hands the URL to the owner's browser.
        let profiled = PROFILED.contains(&base.as_str()) && !a[2..].iter().any(|x| x.to_lowercase().starts_with("--user-data-dir"));
        let mut pdir = String::new();
        if profiled {
            // Never a relative fallback: without LOCALAPPDATA the profile would land in the caller's directory.
            let Some(base) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) else {
                return ghost("LOCALAPPDATA is not set, so ghost has nowhere to put the browser's throwaway profile; pass --user-data-dir=<dir>");
            };
            let dir = std::path::PathBuf::from(base).join("ghost").join("profiles").join(format!("{PROFILE_PREFIX}{}", filetime_now()));
            let _ = std::fs::create_dir_all(&dir);
            prune_profiles(dir.clone());
            pdir = dir.display().to_string();
            cmd.push(' ');
            cmd.push_str(&quote(&format!("--user-data-dir={}", dir.display())));
            // DevTools on 127.0.0.1, random port, for `go` (see cdp.rs); only on these throwaway profiles.
            cmd.push_str(" --no-first-run --no-default-browser-check --remote-debugging-port=0");
        }
        let url = BROWSERS.contains(&base.as_str()) && a[2..].iter().any(|x| !x.starts_with('-'));
        let t0 = filetime_now();
        let root = if uri {
            shell_launch(&a[1], &a[2..])? // ms-settings:..., other protocols, shell:AppsFolder\<AUMID>
        } else {
            spawn(&cmd, &cx.cwd)?
        };
        daemon::mark_act(root);
        *lock(&PENDING) = Some((root, base.clone(), t0));
        let _pending = Pending; // cleared however this function returns
        let sw = now_ms();
        let mut verdict: HashMap<u32, bool> = HashMap::new();
        let mut found = HWND::default();
        let mut seen: HashSet<isize> = HashSet::new();
        while found.0.is_null() && now_ms() - sw < 15000 {
            for h in win::top_windows() {
                if before.contains(&hv(h)) || !win::visible(h) {
                    continue;
                }
                unsafe {
                    if GetWindowLongW(h, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as i32 != 0 || GetWindow(h, GW_OWNER).map(|o| !o.0.is_null()).unwrap_or(false) {
                        continue; // tool / owned windows
                    }
                }
                let p = win::pid(h);
                if seen.insert(hv(h)) {
                    let ap = win::app_pid(h);
                    dbg(&format!("launch: new window {} class {} pid {} app pid {} ({}, started {:?}, launch t0 {t0})", hv(h), win::class_of(h), p, ap, win::proc_name(ap), proc_start(ap)));
                }
                if win::class_of(h) == "ApplicationFrameWindow" {
                    // UWP: ApplicationFrameHost owns the frame; the app is the process of the CoreWindow it hosts,
                    // which arrives a moment later. Ours if that is the activated pid or started after launch.
                    let ap = win::app_pid(h);
                    if ap != p && (ap == root || proc_start(ap).map(|s| s + 500_000 >= t0).unwrap_or(false)) {
                        found = h;
                        break;
                    }
                    continue;
                }
                let ok = *verdict.entry(p).or_insert_with(|| from_launch(p, root, &base, t0));
                if ok {
                    found = h;
                    break;
                }
            }
            if found.0.is_null() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        if found.0.is_null() {
            let why = if uri { "; an app that was already open may have taken the request in its existing window, which ghost does not adopt" } else { "" };
            return ghost(format!("no new window from the launched process appeared within 15 s (pid {root}){why}"));
        }
        let fpid = win::pid(found);
        if win::class_of(found) == "ApplicationFrameWindow" {
            let mut u = lock(&UWP_TARGETS);
            if u.insert(target.clone()) {
                let _ = std::fs::write(win::state_dir().join("uwp-targets.txt"), u.iter().map(|t| format!("{t}\n")).collect::<String>());
            }
        }
        daemon::mark_act(fpid);
        win::park(found).map_err(Fail::Ghost)?;
        // The app may activate itself or restore its saved position a moment after showing. With no window in
        // front before (foreground 0) it goes to win::handback's choice: "no window" can't be handed back to.
        let back = daemon::handback_from(fg);
        // From here the window is ghost's, even if this launch fails: the caller must be able to close it.
        cx.cur = hv(found);
        cx.launched = true;
        self.add_launched(found);
        // The launched app in front: its window, any window of its app process (app_pid: for UWP, pid is the shared
        // ApplicationFrameHost, which also hosts the owner's own UWP windows), or the launch root.
        let app = Some(win::app_pid(found)).filter(|&a| a != win::pid(found) || win::class_of(found) != "ApplicationFrameWindow"); // a UWP frame without its app yet: by hwnd only
        let in_front = |w: HWND| !w.0.is_null() && w != back && (w == found || app.is_some_and(|a| win::app_pid(w) == a) || win::pid(w) == root || ghost_window(w));
        let guard = now_ms();
        while now_ms() - guard < 1000 || (win::title(found).is_empty() && now_ms() - sw < 15000) {
            if !win::is_window(found) {
                return ghost("launched window closed itself");
            }
            let cur = unsafe { GetForegroundWindow() };
            if in_front(cur) {
                win::restore_foreground(back, cur);
            }
            let _ = win::park(found);
            daemon::mark_act(fpid);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Another app (the owner's) in front now means ghost's steal is over. Only the launched app itself still in
        // front is a failure, after one more push (restore_foreground hides ghost's own window if it must).
        let mut last = unsafe { GetForegroundWindow() };
        if in_front(last) {
            win::restore_foreground(back, last);
            std::thread::sleep(std::time::Duration::from_millis(30));
            last = unsafe { GetForegroundWindow() };
        }
        if in_front(last) {
            return ghost(format!("background unavailable: app took focus and it could not be returned (window {})", win::line(found)));
        }
        if profiled {
            let mut b = lock(&BROWSER);
            b.retain(|&k, _| win::is_window(hwnd(k)));
            b.insert(hv(found), Br { exe, dir: pdir });
        }
        Ok((found, url))
    }
}

/// Starts `cmd` unactivated (SW_SHOWNOACTIVATE) in `cwd`, inheriting no handles; returns its pid.
fn spawn(cmd: &str, cwd: &str) -> R<u32> {
    let mut wcmd = win::wide(cmd);
    let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, dwFlags: STARTF_USESHOWWINDOW, wShowWindow: SW_SHOWNOACTIVATE.0 as u16, ..Default::default() };
    let mut pi = PROCESS_INFORMATION::default();
    let dir = if !cwd.is_empty() && std::path::Path::new(cwd).is_dir() { Some(win::wide(cwd)) } else { None };
    unsafe {
        // inherit=false: the child must not hold any of our handles
        CreateProcessW(PCWSTR::null(), Some(PWSTR(wcmd.as_mut_ptr())), None, None, false, PROCESS_CREATION_FLAGS(0), None, dir.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())), &si, &mut pi)
            .map_err(|e| Fail::Ghost(format!("CreateProcess failed for {cmd}: {}", e.message())))?;
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
    }
    Ok(pi.dwProcessId)
}

/// The launch in progress: (root pid, exe base name, start FILETIME). Until launch finds and parks its window,
/// the foreground guard has no other way to know a new window is ghost's: Explorer shows its window active and
/// on screen, and held the foreground ~200 ms until launch found it (measured).
static PENDING: LazyLock<Mutex<Option<(u32, String, u64)>>> = LazyLock::new(Default::default);
struct Pending;
impl Drop for Pending {
    fn drop(&mut self) {
        *lock(&PENDING) = None;
    }
}
/// A process ghost launched: its other windows (a hidden Chromium helper) are not the owner's either.
pub fn ghost_pid(pid: u32) -> bool {
    lock(&LAUNCHED).values().any(|e| e.pid == pid)
}

/// A process the launch in progress started: the root itself, or a same-named process started after it (the
/// rule `from_launch` uses, minus the descendant walk, which is too slow for the guard).
pub fn pending_launch(pid: u32) -> bool {
    let Some((root, base, t0)) = lock(&PENDING).clone() else { return false };
    pid == root || (win::proc_name(pid).to_lowercase() == base && proc_start(pid).map(|s| s + 500_000 >= t0).unwrap_or(false))
}

/// A window that is ghost's: launched or adopted, parked by ghost (its park file names this process), or the sink. Off screen
/// alone proves nothing: the owner's own windows can be off every monitor or minimized.
pub fn ghost_window(h: HWND) -> bool {
    let r = win::root_of(h);
    let r = if r.0.is_null() { h } else { r };
    !r.0.is_null() && (hv(r) == win::SINK.load(std::sync::atomic::Ordering::SeqCst) || lock(&LAUNCHED).get(&hv(r)).is_some_and(|e| alive(r, e)) || win::park_owned(r))
}

/// When ghost last closed a UWP app's window: a launch within UWP_COOLDOWN_MS of it waits out the rest (see close).
static UWP_CLOSED: LazyLock<Mutex<Option<u64>>> = LazyLock::new(Default::default);
/// Launch targets (as given, lowercased) that opened a UWP frame: only these wait out UWP_COOLDOWN_MS and need the
/// foreground lock. Kept in uwp-targets.txt so a daemon restart remembers them.
static UWP_TARGETS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| {
    Mutex::new(std::fs::read_to_string(win::state_dir().join("uwp-targets.txt")).unwrap_or_default().lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
});
const UWP_COOLDOWN_MS: u64 = 1500;

/// Throwaway profile folders are `ghost-<FILETIME>`; cleanup touches nothing else.
const PROFILE_PREFIX: &str = "ghost-";

/// `ghost-<digits>`: a folder ghost made.
fn ghost_profile(name: &str) -> bool {
    name.strip_prefix(PROFILE_PREFIX).map(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())).unwrap_or(false)
}

/// How a ghost-profiled browser was started, for `go`: its exe and the switches the caller gave (no URLs).
#[derive(Clone)]
struct Br {
    exe: String,
    dir: String, // its throwaway --user-data-dir
}
static BROWSER: LazyLock<Mutex<HashMap<isize, Br>>> = LazyLock::new(Default::default);

/// Removes throwaway profiles of browsers that have exited. A running Chromium holds `lockfile` open without
/// delete sharing (and deletes it on exit), so a profile whose lockfile can't be removed is in use and kept.
/// Ten minutes of age keeps a profile whose browser is still starting.
fn prune_profiles(keep: std::path::PathBuf) {
    std::thread::spawn(move || {
        let Some(dir) = keep.parent() else { return };
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let old = e.metadata().and_then(|m| m.modified()).map(|t| t.elapsed().map(|d| d.as_secs() > 600).unwrap_or(false)).unwrap_or(false);
            // Only folders ghost named itself.
            let ours = e.file_name().to_str().map(ghost_profile).unwrap_or(false);
            if p == keep || !ours || !p.is_dir() || !old {
                continue;
            }
            let lock = p.join("lockfile");
            if !lock.exists() || std::fs::remove_file(&lock).is_ok() {
                let _ = std::fs::remove_dir_all(&p);
            }
        }
    });
}

/// `msedge` -> its full path from App Paths, when the bare name isn't found on PATH.
fn app_path(name: &str) -> Option<String> {
    use windows::Win32::Storage::FileSystem::SearchPathW;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    if name.contains(['\\', '/', ':']) {
        return None;
    }
    let exe = if name.to_lowercase().ends_with(".exe") { name.to_string() } else { format!("{name}.exe") };
    let w = win::wide(&exe);
    let mut buf = [0u16; 1024];
    if unsafe { SearchPathW(PCWSTR::null(), PCWSTR(w.as_ptr()), PCWSTR::null(), Some(&mut buf), None) } > 0 {
        return None; // CreateProcess finds it as typed
    }
    let key = win::wide(&format!(r"Software\Microsoft\Windows\CurrentVersion\App Paths\{exe}"));
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        let mut n = (buf.len() * 2) as u32;
        if unsafe { RegGetValueW(root, PCWSTR(key.as_ptr()), PCWSTR::null(), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut n)) }.is_ok() {
            let s = String::from_utf16_lossy(&buf[..(n as usize / 2).saturating_sub(1)]).trim_matches('"').to_string();
            if std::path::Path::new(&s).is_file() {
                return Some(s);
            }
        }
    }
    None
}

/// How long since the owner's last keyboard or mouse input (GetLastInputInfo; reads, never sends).
fn idle_ms() -> u64 {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut li = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    if !unsafe { GetLastInputInfo(&mut li) }.as_bool() {
        return 0;
    }
    unsafe { windows::Win32::System::SystemInformation::GetTickCount() }.wrapping_sub(li.dwTime) as u64
}

/// Whether launching `target` starts a packaged (UWP) app, which the system activates (takes the foreground) as it
/// starts. Before any launch: see uwp_kind; the learned uwp-targets.txt covers the rest.
fn uwp_target(target: &str) -> bool {
    let wa = std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("Microsoft").join("WindowsApps"));
    uwp_kind(target, appexeclink, wa.as_deref(), resolve_exe(target))
}

/// UWP when: an `ms-...:` protocol or `shell:AppsFolder\` path; a program that resolves to an App Execution Alias
/// (an IO_REPARSE_TAG_APPEXECLINK reparse point, the WindowsApps aliases); or a System32 stub that hands off to a
/// packaged app: calc always, notepad, mspaint and snippingtool only when their packaged app's alias is installed.
fn uwp_kind(target: &str, is_link: impl Fn(&std::path::Path) -> bool, windows_apps: Option<&std::path::Path>, resolved: Option<std::path::PathBuf>) -> bool {
    let t = target.trim().to_lowercase();
    if (t.starts_with("ms-") && t.contains(':')) || t.starts_with("shell:appsfolder\\") {
        return true;
    }
    if resolved.as_deref().is_some_and(&is_link) {
        return true;
    }
    let file = t.rsplit(['\\', '/']).next().unwrap_or(&t);
    let stem = file.strip_suffix(".exe").unwrap_or(file);
    match stem {
        "calc" => true,
        "notepad" | "mspaint" | "snippingtool" => windows_apps.is_some_and(|w| is_link(&w.join(format!("{stem}.exe")))),
        _ => false,
    }
}

/// `target` as a file: itself if it names one, else found on the search path (with .exe added).
fn resolve_exe(target: &str) -> Option<std::path::PathBuf> {
    use windows::Win32::Storage::FileSystem::SearchPathW;
    let p = std::path::Path::new(target);
    if target.contains(['\\', '/']) {
        return p.is_file().then(|| p.to_path_buf());
    }
    let exe = if target.to_lowercase().ends_with(".exe") { target.to_string() } else { format!("{target}.exe") };
    let w = win::wide(&exe);
    let mut buf = [0u16; 1024];
    let n = unsafe { SearchPathW(PCWSTR::null(), PCWSTR(w.as_ptr()), PCWSTR::null(), Some(&mut buf), None) } as usize;
    (n > 0 && n < buf.len()).then(|| std::path::PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

/// An App Execution Alias: a reparse point tagged IO_REPARSE_TAG_APPEXECLINK.
fn appexeclink(p: &std::path::Path) -> bool {
    use windows::Win32::Storage::FileSystem::{FindClose, FindFirstFileW, FILE_ATTRIBUTE_REPARSE_POINT, WIN32_FIND_DATAW};
    const IO_REPARSE_TAG_APPEXECLINK: u32 = 0x8000_001B;
    let w = win::wide(&p.to_string_lossy());
    let mut d = WIN32_FIND_DATAW::default();
    match unsafe { FindFirstFileW(PCWSTR(w.as_ptr()), &mut d) } {
        Ok(h) => {
            let _ = unsafe { FindClose(h) };
            d.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 && d.dwReserved0 == IO_REPARSE_TAG_APPEXECLINK
        }
        Err(_) => false,
    }
}

/// A protocol URI (ms-settings:about) or shell:AppsFolder\<AUMID>, as opposed to a program path.
fn is_uri(s: &str) -> bool {
    if s.len() > 6 && s[..6].eq_ignore_ascii_case("shell:") {
        return true;
    }
    match s.find(':') {
        Some(i) if i >= 2 => s[..i].chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) && !std::path::Path::new(s).exists(),
        _ => false,
    }
}

/// Starts a URI or packaged app without activating it where the shell allows, on a short-lived STA thread
/// (the shell's handlers expect one). Returns the app's pid when the shell reports it, else 0.
fn shell_launch(target: &str, rest: &[String]) -> R<u32> {
    let t = target.to_string();
    let args = rest.iter().map(|x| quote(x)).collect::<Vec<_>>().join(" ");
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        let r = shell_launch_sta(&t, &args);
        windows::Win32::System::Com::CoUninitialize();
        r
    })
    .join()
    .unwrap_or_else(|_| Err("shell launch failed".into()))
    .map_err(Fail::Ghost)
}

unsafe fn shell_launch_sta(t: &str, args: &str) -> Result<u32, String> {
    use windows::Win32::UI::Shell::*;
    let wt = win::wide(t);
    let wa = win::wide(args);
    use windows::Win32::System::Com::*;
    let am: Option<IApplicationActivationManager> = CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER).ok();
    // From the daemon this answers "Access is denied" here (seen 2026-10-17); ShellExecuteEx below then starts the app.
    if t.len() > 17 && t[..17].eq_ignore_ascii_case("shell:AppsFolder\\") {
        if let Some(am) = &am {
            let id = win::wide(&t[17..]);
            match am.ActivateApplication(PCWSTR(id.as_ptr()), PCWSTR(wa.as_ptr()), AO_NOSPLASHSCREEN) {
                Ok(pid) => return Ok(pid),
                Err(e) => crate::daemon::dbg(&format!("ActivateApplication {}: {}; using ShellExecuteEx", &t[17..], e.message())),
            }
        }
    }
    // A protocol whose handler is a packaged app: activate that app for the URI, which reports its pid.
    let scheme = win::wide(&t[..t.find(':').unwrap_or(0)]);
    let mut buf = [0u16; 512];
    let mut n = buf.len() as u32;
    if let Some(am) = &am {
        if AssocQueryStringW(ASSOCF_IS_PROTOCOL, ASSOCSTR_APPID, PCWSTR(scheme.as_ptr()), PCWSTR::null(), Some(PWSTR(buf.as_mut_ptr())), &mut n).is_ok() {
            let id = String::from_utf16_lossy(&buf[..(n as usize).saturating_sub(1).min(buf.len())]);
            if id.contains('!') {
                let item: windows::core::Result<IShellItem> = SHCreateItemFromParsingName(PCWSTR(wt.as_ptr()), None);
                if let Ok(arr) = item.and_then(|i| SHCreateShellItemArrayFromShellItem::<_, IShellItemArray>(&i)) {
                    let wid = win::wide(&id);
                    if let Ok(pid) = am.ActivateForProtocol(PCWSTR(wid.as_ptr()), &arr) {
                        return Ok(pid);
                    }
                }
            }
        }
    }
    // Classic handlers: ShellExecuteEx without activation.
    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI | SEE_MASK_NOASYNC,
        lpFile: PCWSTR(wt.as_ptr()),
        lpParameters: if args.is_empty() { PCWSTR::null() } else { PCWSTR(wa.as_ptr()) },
        nShow: SW_SHOWNOACTIVATE.0,
        ..Default::default()
    };
    ShellExecuteExW(&mut sei).map_err(|e| format!("could not open {t}: {}", e.message()))?;
    let mut pid = 0;
    if !sei.hProcess.is_invalid() && !sei.hProcess.0.is_null() {
        pid = GetProcessId(sei.hProcess);
        let _ = CloseHandle(sei.hProcess);
    }
    Ok(pid)
}

fn quote(s: &str) -> String {
    if !s.is_empty() && !s.contains([' ', '\t', '"']) {
        return s.to_string();
    }
    format!("\"{}\"", s.replace('"', "\\\""))
}

fn filetime_now() -> u64 {
    let ft = unsafe { windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime() };
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// The launched process, a descendant of it, or a same-named process started after launch
/// (Win11 notepad.exe is a stub that hands off to a packaged process).
fn from_launch(pid: u32, root: u32, base: &str, t0: u64) -> bool {
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    if pid == root {
        return true;
    }
    let mut parent: HashMap<u32, u32> = HashMap::new();
    unsafe {
        if let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            let mut more = Process32FirstW(snap, &mut e).is_ok();
            while more {
                parent.insert(e.th32ProcessID, e.th32ParentProcessID);
                more = Process32NextW(snap, &mut e).is_ok();
            }
            let _ = CloseHandle(snap);
        }
    }
    let mut p = pid;
    for _ in 0..32 {
        match parent.get(&p) {
            Some(&pp) if pp != 0 => {
                if pp == root {
                    return true;
                }
                p = pp;
            }
            _ => break,
        }
    }
    if win::proc_name(pid).to_lowercase() != base {
        return false;
    }
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let (mut c, mut x, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let ok = GetProcessTimes(h, &mut c, &mut x, &mut k, &mut u).is_ok();
        let _ = CloseHandle(h);
        ok && (((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64) + 500_000 >= t0
    }
}

// ---------- clicking ----------
fn is_push_button(nh: HWND) -> bool {
    if nh.0.is_null() || unsafe { GetParent(nh) }.map(|p| p.0.is_null()).unwrap_or(true) || !win::class_of(nh).to_lowercase().contains("button") {
        return false;
    }
    let style = unsafe { GetWindowLongW(nh, GWL_STYLE) } & 0xF;
    style <= 1 || style == 0xB // push, default push, owner-draw
}

/// Native push button: the UIA proxy's Invoke can activate the window, so send the parent the BN_CLICKED
/// notification a real click would produce (WinForms reflects it to the Button).
fn bn_clicked(nh: HWND) {
    unsafe {
        if let Ok(p) = GetParent(nh) {
            let _ = PostMessageW(Some(p), WM_COMMAND, WPARAM((GetDlgCtrlID(nh) & 0xFFFF) as usize), LPARAM(nh.0 as isize));
        }
    }
}

fn post_click(target: HWND, screen: POINT) {
    let p = win::screen_to_client(target, screen);
    let lp = LPARAM((((p.y & 0xFFFF) << 16) | (p.x & 0xFFFF)) as isize);
    unsafe {
        let _ = PostMessageW(Some(target), WM_MOUSEMOVE, WPARAM(0), lp);
        let _ = PostMessageW(Some(target), WM_LBUTTONDOWN, WPARAM(1), lp);
        let _ = PostMessageW(Some(target), WM_LBUTTONUP, WPARAM(0), lp);
    }
}

/// Windows ghost has disabled right now, journaled to %LOCALAPPDATA%\ghost\shielded.txt so a daemon that dies
/// mid-call (daemon-stop, idle exit, a crash, a retired worker stuck in the call) can't leave the user's app disabled.
static SHIELDS: LazyLock<Mutex<Vec<(isize, u32)>>> = LazyLock::new(Default::default);

fn shield_file() -> std::path::PathBuf {
    win::state_dir().join("shielded.txt")
}
fn save_shields(v: &[(isize, u32)]) {
    let s: String = v.iter().map(|(h, p)| format!("{h} {p}\n")).collect();
    let _ = std::fs::write(shield_file(), s);
}
pub fn shields_active() -> bool {
    !lock(&SHIELDS).is_empty()
}
/// Re-enables every journaled window that still exists with the same pid. Daemon start, stop and idle exit.
/// EnableWindow sends WM_ENABLE and waits, so each runs on its own thread with 500 ms to answer; a hung app
/// stays in the journal for the next start instead of blocking shutdown.
pub fn unshield_all() {
    let todo: Vec<(isize, u32)> = {
        let mut v = lock(&SHIELDS);
        v.clear();
        std::fs::read_to_string(shield_file())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let mut it = l.split(' ');
                Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
            })
            .collect()
    };
    let mut left = vec![];
    for (h, p) in todo {
        if !win::is_window(hwnd(h)) || win::pid(hwnd(h)) != p {
            continue;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            unsafe {
                let _ = EnableWindow(hwnd(h), true);
            }
            let _ = tx.send(());
        });
        if rx.recv_timeout(std::time::Duration::from_millis(500)).is_err() {
            left.push((h, p));
        }
    }
    let _v = lock(&SHIELDS);
    if left.is_empty() {
        let _ = std::fs::remove_file(shield_file());
    } else {
        save_shields(&left);
    }
}

/// A disabled top-level can't be made foreground, yet the UIA call still lands (cua fg_bypass). Re-enabled
/// right after, whatever happened, so the user's app is never left disabled; a window that was already disabled
/// (owner of a modal dialog) or whose app isn't answering is left alone. Win32 proxies refuse disabled windows:
/// those retry unshielded and the foreground guard is the backstop.
pub fn shielded(h: HWND, f: impl Fn() -> windows::core::Result<()>) -> windows::core::Result<()> {
    let root = win::root_of(h);
    let root = if root.0.is_null() { h } else { root };
    let shield = unsafe { IsWindowEnabled(root).as_bool() } && win::responsive(root);
    let entry = (hv(root), win::pid(root));
    if shield {
        let mut v = lock(&SHIELDS);
        v.push(entry);
        save_shields(&v); // journal first: the window is never disabled without a record
        unsafe {
            let _ = EnableWindow(root, false);
        }
    }
    let r = f();
    if shield {
        // Always back to the pre-call state, whatever the call opened: a non-modal popup must never leave the
        // owner disabled, and a dialog created while it was disabled won't re-enable it itself.
        unsafe {
            let _ = EnableWindow(root, true);
        }
        let mut v = lock(&SHIELDS);
        if let Some(i) = v.iter().position(|&e| e == entry) {
            v.remove(i);
        }
        save_shields(&v);
    }
    match r {
        Err(e) if shield && e.code().0 as u32 == UIA_E_ELEMENTNOTENABLED => {
            dbg(&format!("shield refused by {}, retrying unshielded", hv(root)));
            f()
        }
        other => other,
    }
}

/// A user can't press a disabled control, or anything in a window disabled by a modal dialog: ghost won't either.
fn refuse_disabled(h: HWND, el: Option<&IUIAutomationElement>) -> R<()> {
    if let Some(e) = el {
        if unsafe { e.CurrentIsEnabled().map(|b| !b.as_bool()).unwrap_or(false) } {
            return ghost(format!("disabled: {}", unsafe { e.CurrentName().map(|b| b.to_string()).unwrap_or_default() }));
        }
    }
    let root = win::root_of(h);
    let root = if root.0.is_null() { h } else { root };
    let owned = |w: HWND| win::visible(w) && unsafe { GetWindow(w, GW_OWNER) }.map(|o| o == root).unwrap_or(false);
    // A standard modal dialog (#32770) in front of it counts too, even while the owner is enabled.
    let modal = win::top_windows().into_iter().find(|&w| owned(w) && win::class_of(w) == "#32770" && unsafe { IsWindowEnabled(w).as_bool() });
    if !unsafe { IsWindowEnabled(root).as_bool() } || modal.is_some() {
        // Name the dialog, so the agent can act on it without another `windows` call.
        let dlg = modal
            .or_else(|| unsafe { GetWindow(root, GW_ENABLEDPOPUP).ok().filter(|p| !p.0.is_null() && *p != root) })
            .or_else(|| Some(unsafe { GetLastActivePopup(root) }).filter(|p| !p.0.is_null() && *p != root))
            .or_else(|| win::top_windows().into_iter().find(|&w| owned(w)));
        return ghost(match dlg {
            Some(d) => format!("disabled: {}, dialog open: dlg \"{}\" (hwnd {})", win::title(root), clip(&win::title(d), 60), hv(d)),
            None => format!("disabled: {} (a dialog may be open)", win::title(root)),
        });
    }
    Ok(())
}

/// What a click must change, read back after the app settles. Toggle and expand must flip; select must end selected.
pub enum Check {
    Toggle(IUIAutomationElement, i32),
    Select(IUIAutomationElement),
    Expand(IUIAutomationElement, i32),
    Window(HWND, (bool, bool)), // (maximized, minimized) of a top-level
    Text(IUIAutomationElement, HWND, String, bool), // element, its edit HWND if any, wanted text, only as a suffix
    Gone(HWND),                                     // a window that was told to close
}
fn win_state(h: HWND) -> (bool, bool) {
    unsafe { (IsZoomed(h).as_bool(), IsIconic(h).as_bool()) }
}
fn current(el: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Option<Var> {
    unsafe { el.GetCurrentPropertyValue(id).ok().map(Var) }
}
/// The accessibility roles CDP's queryAXTree knows a UIA control type by; none for types not matched over CDP.
fn ax_roles(ct: UIA_CONTROLTYPE_ID) -> &'static [&'static str] {
    let m: [(UIA_CONTROLTYPE_ID, &'static [&'static str]); 8] = [
        (UIA_ButtonControlTypeId, &["button"]),
        (UIA_HyperlinkControlTypeId, &["link"]),
        (UIA_EditControlTypeId, &["textbox", "searchbox", "combobox"]),
        (UIA_CheckBoxControlTypeId, &["checkbox"]),
        (UIA_RadioButtonControlTypeId, &["radio"]),
        (UIA_ComboBoxControlTypeId, &["combobox"]),
        (UIA_MenuItemControlTypeId, &["menuitem"]),
        (UIA_TabItemControlTypeId, &["tab"]),
    ];
    m.iter().find(|(c, _)| *c == ct).map(|(_, r)| *r).unwrap_or(&[])
}

fn check_of(el: &IUIAutomationElement) -> Option<Check> {
    if let Some(t) = current(el, UIA_ToggleToggleStatePropertyId).and_then(|v| v.i32()) {
        return Some(Check::Toggle(el.clone(), t));
    }
    current(el, UIA_SelectionItemIsSelectedPropertyId).and_then(|v| v.bool()).map(|_| Check::Select(el.clone()))
}
impl Check {
    fn wait_ms(&self) -> u64 {
        if matches!(self, Check::Gone(_)) { 1000 } else { 300 }
    }
    /// An unreadable state (element gone) counts as changed: something happened.
    fn done(&self) -> bool {
        match self {
            Check::Toggle(e, b) => current(e, UIA_ToggleToggleStatePropertyId).and_then(|v| v.i32()).map(|t| t != *b).unwrap_or(true),
            Check::Select(e) => current(e, UIA_SelectionItemIsSelectedPropertyId).and_then(|v| v.bool()).unwrap_or(true),
            Check::Expand(e, b) => current(e, UIA_ExpandCollapseExpandCollapseStatePropertyId).and_then(|v| v.i32()).map(|t| t != *b).unwrap_or(true),
            Check::Window(h, b) => !win::is_window(*h) || win_state(*h) != *b,
            Check::Gone(h) => !win::is_window(*h),
            Check::Text(e, hw, want, suffix) => {
                let got = if hw.0.is_null() {
                    match unsafe { e.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) } {
                        Ok(vp) => unsafe { vp.CurrentValue().map(|b| b.to_string()).unwrap_or_default() },
                        Err(_) => return true, // nothing to read it back from
                    }
                } else {
                    get_text(*hw)
                };
                let (got, want) = (got.replace("\r\n", "\n"), want.replace("\r\n", "\n"));
                if *suffix { got.ends_with(&want) } else { got == want }
            }
        }
    }
}

/// Title bar parts. Invoke there, and even a posted SC_MAXIMIZE, let the app activate itself. Minimize and
/// restore use ShowWindow's non-activating commands; maximize has none, so it runs with the window disabled
/// (a disabled window can't be activated); close posts SC_CLOSE. The system menu can't open without
/// activation: refused.
fn titlebar(uia: &IUIAutomation, h: HWND, el: &IUIAutomationElement, ct: UIA_CONTROLTYPE_ID) -> R<Option<(String, Option<Check>)>> {
    unsafe {
        let Ok(w) = uia.ControlViewWalker() else { return Ok(None) };
        let mut p = w.GetParentElement(el).ok();
        let mut in_bar = false;
        for _ in 0..3 {
            let Some(x) = p else { break };
            if x.CurrentControlType().ok() == Some(UIA_TitleBarControlTypeId) {
                in_bar = true;
                break;
            }
            p = w.GetParentElement(&x).ok();
        }
        if !in_bar {
            return Ok(None);
        }
        if ct != UIA_ButtonControlTypeId {
            return ghost("background unavailable: system menu");
        }
        let top = win::root_of(h);
        let top = if top.0.is_null() { h } else { top };
        let name = el.CurrentName().map(|b| b.to_string()).unwrap_or_default();
        // shortcut: matches the English button names; other UI languages get the refusal below.
        let sc = match name.to_lowercase().as_str() {
            "minimize" => SC_MINIMIZE,
            "maximize" | "restore" => {
                if IsZoomed(top).as_bool() || IsIconic(top).as_bool() {
                    SC_RESTORE
                } else {
                    SC_MAXIMIZE
                }
            }
            "close" => SC_CLOSE,
            _ => return ghost(format!("background unavailable: title bar button \"{name}\"")),
        };
        if sc == SC_MINIMIZE && win::parked(top) {
            return ghost("background unavailable: a minimized parked window stops rendering; it is already off screen (park keeps it there)");
        }
        if sc == SC_CLOSE {
            PostMessageW(Some(top), WM_SYSCOMMAND, WPARAM(sc as usize), LPARAM(0))?;
            return Ok(Some(("sc_close".into(), Some(Check::Gone(top)))));
        }
        let check = Some(Check::Window(top, win_state(top)));
        let cmd = match sc {
            SC_MINIMIZE => SW_SHOWMINNOACTIVE,
            SC_RESTORE => SW_SHOWNOACTIVATE,
            _ => SW_SHOWMAXIMIZED,
        };
        if cmd == SW_SHOWMAXIMIZED {
            shielded(top, || {
                let _ = ShowWindow(top, cmd);
                Ok(())
            })?;
        } else {
            let _ = ShowWindow(top, cmd);
        }
        Ok(Some((format!("showwindow {}", cmd.0), check)))
    }
}

fn click_el(uia: &IUIAutomation, h: HWND, el: &IUIAutomationElement) -> R<(String, Option<Check>)> {
    unsafe {
        let nh = el.CurrentNativeWindowHandle().unwrap_or_default();
        let ct = el.CurrentControlType().unwrap_or_default();
        // BN_CLICKED only for real buttons: owner-draw check boxes and radios are BUTTON-class too and ignore it.
        if ct == UIA_ButtonControlTypeId && is_push_button(nh) {
            bn_clicked(nh);
            return Ok(("bn_clicked".into(), None));
        }
        if nh.0.is_null() && [UIA_ButtonControlTypeId, UIA_MenuItemControlTypeId, UIA_MenuBarControlTypeId].contains(&ct) {
            if let Some(r) = titlebar(uia, h, el, ct)? {
                return Ok(r);
            }
        }
        // Elements served through Windows' MSAA proxy (WinForms, classic Win32): the proxy's Invoke and Toggle
        // move the foreground onto the control and it can't be given back; the control's own MSAA default action
        // does the same click without it. Unshielded: the proxy refuses a disabled window anyway.
        let msaa = current(el, UIA_ProviderDescriptionPropertyId).and_then(|v| v.bstr()).map(|d| d.contains("MSAA Proxy")).unwrap_or(false);
        if msaa {
            if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(UIA_LegacyIAccessiblePatternId) {
                let check = if ct == UIA_CheckBoxControlTypeId || ct == UIA_RadioButtonControlTypeId { check_of(el) } else { None };
                if p.DoDefaultAction().is_ok() {
                    return Ok(("default_action".into(), check));
                }
            }
        }
        if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) {
            shielded(h, || p.Invoke())?;
            return Ok(("invoke".into(), None));
        }
        if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId) {
            let before = p.CurrentToggleState()?.0;
            shielded(h, || p.Toggle())?;
            return Ok(("toggle".into(), Some(Check::Toggle(el.clone(), before))));
        }
        if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId) {
            shielded(h, || p.Select())?;
            return Ok(("select".into(), Some(Check::Select(el.clone()))));
        }
        if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId) {
            let before = p.CurrentExpandCollapseState()?;
            let check = Some(Check::Expand(el.clone(), before.0));
            if before == ExpandCollapseState_Expanded {
                shielded(h, || p.Collapse())?;
                return Ok(("collapse".into(), check));
            }
            shielded(h, || p.Expand())?;
            return Ok(("expand".into(), check));
        }
        let b = el.CurrentBoundingRectangle()?;
        if b.right - b.left <= 0 {
            return ghost("background unavailable: element has no pattern and no bounds");
        }
        let check = if ct == UIA_CheckBoxControlTypeId || ct == UIA_RadioButtonControlTypeId { check_of(el) } else { None };
        let pt = POINT { x: (b.left + b.right) / 2, y: (b.top + b.bottom) / 2 };
        post_click(hwnd_up(uia, el, h), pt);
        Ok(("postmessage".into(), check))
    }
}

/// How long a click on this page element may take to start a navigation, and whether it is a button (which
/// stops waiting once the page changes in place). A link to another page: 3 s; to a spot on this page: 300 ms.
fn nav_expect(el: &IUIAutomationElement, url: &str) -> (u64, bool) {
    let ct = unsafe { el.CurrentControlType().unwrap_or_default() };
    if ct == UIA_HyperlinkControlTypeId {
        let href = current(el, UIA_ValueValuePropertyId).and_then(|v| v.bstr()).unwrap_or_default();
        let page = |u: &str| u.split('#').next().unwrap_or("").to_string();
        return (if !href.is_empty() && page(&href) == page(url) { 300 } else { 3000 }, false);
    }
    if ct == UIA_ButtonControlTypeId {
        return (500, true); // a form's submit button navigates; most buttons change the page in place
    }
    (0, false)
}

/// A text box of the browser itself (address bar, find bar), not of the page: no Document above it.
fn chrome_edit(uia: &IUIAutomation, el: &IUIAutomationElement) -> bool {
    unsafe {
        if el.CurrentControlType().ok() != Some(UIA_EditControlTypeId) {
            return false;
        }
        let Ok(w) = uia.ControlViewWalker() else { return false };
        let mut p = w.GetParentElement(el).ok();
        for _ in 0..64 {
            let Some(x) = p else { return true };
            if x.CurrentControlType().ok() == Some(UIA_DocumentControlTypeId) {
                return false;
            }
            p = w.GetParentElement(&x).ok();
        }
        false
    }
}

/// Nearest HWND at or above the element.
fn hwnd_up(uia: &IUIAutomation, el: &IUIAutomationElement, fallback: HWND) -> HWND {
    unsafe {
        let Ok(w) = uia.RawViewWalker() else { return fallback };
        let mut e = Some(el.clone());
        while let Some(x) = e {
            let n = x.CurrentNativeWindowHandle().unwrap_or_default();
            if !n.0.is_null() {
                return n;
            }
            e = w.GetParentElement(&x).ok();
        }
    }
    fallback
}

fn get_text(hw: HWND) -> String {
    unsafe {
        let mut len = 0usize;
        SendMessageTimeoutW(hw, WM_GETTEXTLENGTH, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG, 5000, Some(&mut len));
        let mut b = vec![0u16; len + 2];
        let mut got = 0usize;
        SendMessageTimeoutW(hw, WM_GETTEXT, WPARAM(b.len()), LPARAM(b.as_mut_ptr() as isize), SMTO_ABORTIFHUNG, 5000, Some(&mut got));
        String::from_utf16_lossy(&b[..got.min(b.len())])
    }
}

fn key_target_of(uia: &IUIAutomation, h: HWND, el: &IUIAutomationElement) -> HWND {
    let n = unsafe { el.CurrentNativeWindowHandle().unwrap_or_default() };
    if !n.0.is_null() {
        return n;
    }
    hwnd_up(uia, el, h)
}

fn key_target(h: HWND) -> HWND {
    unsafe {
        let mut gi = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        if GetGUIThreadInfo(win::thread_of(h), &mut gi).is_ok() && !gi.hwndFocus.0.is_null() && (gi.hwndFocus == h || IsChild(h, gi.hwndFocus).as_bool()) {
            return gi.hwndFocus;
        }
    }
    // Thread focus is in another window (apps like Notepad share one UI thread): use the first visible edit-class child.
    win::child_windows(h).into_iter().find(|&c| win::visible(c) && win::class_of(c).to_lowercase().contains("edit")).unwrap_or(h)
}
