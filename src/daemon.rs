//! `ghost --daemon`: the resident server. Owns the pipe, the per-session current window, the event pump
//! (foreground guard + per-process UI activity) and a worker thread that runs commands under a deadline.
use crate::engine::{Engine, Reply};
use crate::proto;
use crate::win::{self, hv, hwnd};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::Storage::FileSystem::{FlushFileBuffers, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::*;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, GetCurrentThread, GetCurrentThreadId, OpenProcessToken, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::*;

static CLOCK: LazyLock<Instant> = LazyLock::new(Instant::now);
pub fn now_ms() -> u64 {
    CLOCK.elapsed().as_millis() as u64
}
static LAST_ACTIVITY: AtomicU64 = AtomicU64::new(0);
static STOP: AtomicBool = AtomicBool::new(false);
pub fn request_stop() {
    STOP.store(true, Ordering::SeqCst);
}

/// The debug log: GHOST_DEBUG, else the path in %LOCALAPPDATA%\ghost\debug.txt, read once per daemon (so it
/// survives build.ps1's restart). The file only turns on this log; the `ok <path>` output lines stay GHOST_DEBUG's.
static DEBUG_LOG: LazyLock<Option<String>> = LazyLock::new(|| {
    std::env::var("GHOST_DEBUG").ok().or_else(|| std::fs::read_to_string(win::state_dir().join("debug.txt")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
});

pub fn dbg(m: &str) {
    if let Some(f) = DEBUG_LOG.as_deref() {
        use std::io::Write;
        if let Ok(mut fh) = std::fs::OpenOptions::new().create(true).append(true).open(f) {
            let _ = writeln!(fh, "{} t{} {}", now_ms(), unsafe { windows::Win32::System::SystemInformation::GetTickCount() }, m);
        }
    }
}

// ---------- event pump ----------
#[derive(Default)]
pub struct Fg {
    pub prior_fg: isize, // the last foreground not ghost's, any window (hidden, Task View); 0 = none
    pub user_fg: isize,  // the last of those that was a visible user window
    pub act_until: HashMap<u32, u64>, // pid -> guard deadline
    hook_pid: HashMap<isize, u32>,
    pub pid_hooks: HashMap<u32, [isize; 2]>,
    hook_order: Vec<u32>,
    pub last_evt: HashMap<u32, u64>,
}
pub static FG: LazyLock<Mutex<Fg>> = LazyLock::new(|| Mutex::new(Fg::default()));
static PUMP_TID: AtomicU32 = AtomicU32::new(0);
type Job = Box<dyn FnOnce() + Send>;
static PUMP_Q: LazyLock<Mutex<VecDeque<(Job, mpsc::Sender<()>)>>> = LazyLock::new(|| Mutex::new(VecDeque::new()));

fn fg() -> std::sync::MutexGuard<'static, Fg> {
    FG.lock().unwrap_or_else(|e| e.into_inner())
}

fn start_pump() {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || unsafe {
        let mut m = MSG::default();
        let _ = PeekMessageW(&mut m, None, 0, 0, PM_NOREMOVE); // create the queue
        // The foreground guard runs on this thread's WinEvent callback. Time-critical priority so it reclaims the
        // foreground within a tick even when other processes saturate the CPU (measured: at ~90% load the guard
        // otherwise took up to ~110 ms, a visible flash).
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        PUMP_TID.store(GetCurrentThreadId(), Ordering::SeqCst);
        win::create_sink();
        SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(on_foreground), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
        SetWinEventHook(EVENT_OBJECT_SHOW, EVENT_OBJECT_SHOW, None, Some(on_show), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
        let _ = tx.send(());
        while GetMessageW(&mut m, None, 0, 0).as_bool() {
            if m.hwnd.0.is_null() && m.message == WM_APP {
                let job = PUMP_Q.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
                if let Some((f, done)) = job {
                    f();
                    let _ = done.send(());
                }
                continue;
            }
            DispatchMessageW(&m);
        }
    });
    let _ = rx.recv();
    let f = unsafe { GetForegroundWindow() };
    note_owner(&mut fg(), f);
}

/// Remember `f`, a foreground that is not ghost's, as the prior and, when visible, as the last user window.
fn note_owner(g: &mut Fg, f: HWND) {
    let o = hv(win::owner_window(f));
    g.prior_fg = o;
    if win::user_window(f) {
        g.user_fg = o;
    }
}

/// Hooks must be set on the pump thread, which is the thread that receives their callbacks.
fn on_pump(f: Job) {
    let (tx, rx) = mpsc::channel();
    PUMP_Q.lock().unwrap_or_else(|e| e.into_inner()).push_back((f, tx));
    unsafe {
        let _ = PostThreadMessageW(PUMP_TID.load(Ordering::SeqCst), WM_APP, WPARAM(0), LPARAM(0));
    }
    let _ = rx.recv_timeout(Duration::from_secs(2));
}

pub fn watch_pid(pid: u32) {
    if pid == 0 {
        return;
    }
    {
        let mut g = fg();
        if g.pid_hooks.contains_key(&pid) {
            g.hook_order.retain(|&p| p != pid);
            g.hook_order.push(pid);
            return;
        }
    }
    on_pump(Box::new(move || unsafe {
        // Object events from that process only; LOCATIONCHANGE (0x800B) is skipped, it fires on every caret blink.
        let a = SetWinEventHook(0x8000, 0x800A, None, Some(on_object), pid, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
        let b = SetWinEventHook(0x800C, 0x800E, None, Some(on_object), pid, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
        let mut g = fg();
        let (a, b) = (a.0 as isize, b.0 as isize);
        g.pid_hooks.insert(pid, [a, b]);
        g.hook_pid.insert(a, pid);
        g.hook_pid.insert(b, pid);
        g.hook_order.push(pid);
        while g.hook_order.len() > 8 {
            // bounded: at most 8 watched processes
            let old = g.hook_order.remove(0);
            if let Some(hs) = g.pid_hooks.remove(&old) {
                for x in hs {
                    let _ = UnhookWinEvent(HWINEVENTHOOK(x as *mut _));
                    g.hook_pid.remove(&x);
                }
            }
            g.last_evt.remove(&old);
        }
    }));
}

unsafe extern "system" fn on_object(hook: HWINEVENTHOOK, _e: u32, _h: HWND, _o: i32, _c: i32, _t: u32, _time: u32) {
    let mut g = fg();
    if let Some(&p) = g.hook_pid.get(&(hook.0 as isize)) {
        g.last_evt.insert(p, now_ms());
    }
}

/// A new top-level window owned by one of ghost's windows (a dialog, a popup), or the main window a launched
/// app shows within 30 s of launch (after a splash), is parked the moment it shows. Nothing else is touched.
unsafe extern "system" fn on_show(_hook: HWINEVENTHOOK, _e: u32, h: HWND, obj: i32, child: i32, _t: u32, _time: u32) {
    if h.0.is_null() || obj != 0 || child != 0 || GetAncestor(h, GA_PARENT) != GetDesktopWindow() {
        return; // only whole top-level windows (OBJID_WINDOW, CHILDID_SELF)
    }
    if !win::visible(h) || win::parked(h) {
        return;
    }
    let Some(at) = crate::engine::claim_new(h) else { return };
    win::park_new(h);
    crate::engine::adopt(h, at);
    dbg(&format!("parked new window {} of launched pid {}", hv(h), win::pid(h)));
}

unsafe extern "system" fn on_foreground(_hook: HWINEVENTHOOK, _e: u32, h: HWND, _o: i32, _c: i32, _t: u32, time: u32) {
    if h.0.is_null() {
        return;
    }
    let p = win::pid(h);
    // Restore outside the lock: it calls into other threads' windows and a re-entrant callback must not deadlock.
    if let Some(user) = guard_target(h) {
        win::restore_foreground(user, h);
        let tick = windows::Win32::System::SystemInformation::GetTickCount();
        dbg(&format!("fg guard: {} (pid {}) -> {} reacted in {} ms, event t{time}, now in front {}", hv(h), p, hv(user), tick.wrapping_sub(time), hv(GetForegroundWindow())));
    }
}

/// The guard's decision for a window that just took the foreground: where to give it back, or None to leave it.
fn guard_target(h: HWND) -> Option<HWND> {
    let now = now_ms();
    let p = win::pid(h);
    let mut g = fg();
    let acting = g.act_until.get(&p).map(|&u| now < u).unwrap_or(false);
    // A ghost window holding the foreground is never right, whenever it happens: nobody can see it.
    let ours = acting || crate::engine::ghost_window(h) || crate::engine::pending_launch(p);
    if !ours {
        note_owner(&mut g, h); // the user's own switch: remember it, never fight it
    }
    let user = give_back(&g);
    (ours && user != h && !user.0.is_null()).then_some(user)
}

/// Where the foreground goes back to: win::handback's choice from the prior and the last user window.
fn give_back(g: &Fg) -> HWND {
    if FAKE_FG0.load(Ordering::SeqCst) {
        return win::handback(HWND::default(), HWND::default());
    }
    win::handback(hwnd(g.prior_fg), hwnd(g.user_fg))
}

/// The owner's last visible window, for win::restore_foreground's step-down.
pub fn last_user() -> isize {
    fg().user_fg
}

/// win::handback for a prior foreground the caller read itself (launch, go).
pub fn handback_from(prior: HWND) -> HWND {
    win::handback(prior, hwnd(fg().user_fg))
}

static FAKE_FG0: AtomicBool = AtomicBool::new(false);
/// `ghost debug-fg0 <win>`, for the web-features gate: the guard's decision for that window taking the
/// foreground, with the prior foreground treated as 0, without moving the foreground. A real steal from the
/// owner's window can't be undone in time: measured 2026-10-17, Windows often refused both ghost and the test the
/// foreground back from the sink, leaving the owner's window behind for seconds.
pub fn debug_fg0(h: HWND) -> Result<String, String> {
    let sink = win::SINK.load(Ordering::SeqCst);
    FAKE_FG0.store(true, Ordering::SeqCst);
    let to = guard_target(win::root_of(h));
    FAKE_FG0.store(false, Ordering::SeqCst);
    match to {
        Some(t) if hv(t) == sink => Ok(format!("fg0: the guard hands {} to the sink {sink}", hv(h))),
        Some(t) => Err(format!("debug-fg0: the guard hands it to {} ({}), not the sink", hv(t), win::class_of(t))),
        None => Err("debug-fg0: the guard leaves it in front".into()),
    }
}

/// Once the user has been idle past ForegroundLockTimeout any process may take the foreground, and Chromium does
/// on every UIA action and navigation (measured 2026-10-17; the guard could not always take it back). While ghost
/// acts, and 1.5 s after, SetForegroundWindow is locked as it is while the user types. Windows only grants the
/// lock in that idle state and drops it on the user's next Alt or click. One hold lasts 2 s at most, then 300 ms
/// free, so a long launch or a long `run` never locks the owner out for longer. An action never starts inside
/// that gap (wait_rest): measured, Explorer and Calculator launches that did took the foreground for ~200 ms.
#[derive(Default)]
struct FgLock {
    held: bool,
    since: u64,
    until: u64,
    rest_until: u64,
    last: String, // the last LockSetForegroundWindow(LSFW_LOCK) outcome, for `go`'s debug line
}
static FG_LOCK: LazyLock<Mutex<FgLock>> = LazyLock::new(Default::default);
const LOCK_MAX_MS: u64 = 2000;
fn lock_fg() {
    let mut l = FG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let now = now_ms();
    if l.held && now - l.since >= LOCK_MAX_MS {
        release(&mut l, now);
        return;
    }
    if !l.held {
        if now < l.rest_until {
            return;
        }
        // Fails harmlessly when the user is active (then nothing can take the foreground anyway).
        let r = unsafe { LockSetForegroundWindow(LSFW_LOCK) };
        l.last = if r.is_ok() { "ok".into() } else { format!("failed, GetLastError {}", unsafe { GetLastError().0 }) };
        dbg(&format!("fg lock: {}", if r.is_ok() { "taken".into() } else { format!("{r:?}") }));
        if r.is_err() {
            return; // not held: the next action tries again
        }
        (l.held, l.since) = (true, now);
    }
    l.until = now + 1500;
}
/// Whether ghost holds the foreground lock now (it can only while the owner is idle).
pub fn lock_held() -> bool {
    FG_LOCK.lock().unwrap_or_else(|e| e.into_inner()).held
}

/// The foreground lock right now, for debug lines.
pub fn lock_state() -> String {
    let l = FG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let now = now_ms();
    format!("lock held {} ({} ms), last LockSetForegroundWindow {}", l.held, if l.held { now - l.since } else { 0 }, if l.last.is_empty() { "never called" } else { &l.last })
}
fn release(l: &mut FgLock, now: u64) {
    unsafe {
        let _ = LockSetForegroundWindow(LSFW_UNLOCK);
    }
    dbg(&format!("fg lock: released after {} ms", now - l.since));
    if now - l.since >= LOCK_MAX_MS {
        l.rest_until = now + 300;
    }
    l.held = false;
}
/// Called before an action sends anything. The action should run inside one hold: a lock already held 1.2 s or
/// more is released first and its 300 ms rest waited out, then a fresh one taken (measured: an Explorer launch
/// that straddled the 2 s cap took the foreground in the gap for ~200 ms).
pub fn wait_rest() {
    let until = {
        let mut l = FG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_ms();
        if l.held && now - l.since >= 1200 {
            release(&mut l, now);
            l.rest_until = now + 300;
        }
        l.rest_until
    };
    let now = now_ms();
    if until > now {
        std::thread::sleep(Duration::from_millis(until - now));
    }
    lock_fg();
}
fn unlock_fg() {
    let mut l = FG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if l.held {
        release(&mut l, now_ms());
    }
}

pub fn mark_act(pid: u32) {
    lock_fg();
    let mut g = fg();
    let now = now_ms();
    let busy = g.act_until.values().any(|&u| u >= now); // mid-action, a foreground 0 is an app's activation in flight
    g.act_until.insert(pid, now + 1000);
    g.act_until.retain(|_, &mut u| u >= now);
    let f = unsafe { GetForegroundWindow() };
    if f.0.is_null() {
        // A transient foreground 0 (a window closing, a navigation) must not wipe a still-valid owner, or every
        // later handback would go to the sink and the owner would never get the foreground back. Only forget the
        // owner when its window is truly gone; give_back then falls to the sink on its own.
        if !busy && !win::user_window(hwnd(g.user_fg)) {
            g.user_fg = 0;
            g.prior_fg = 0;
        }
    } else if !g.act_until.contains_key(&win::pid(f)) && !crate::engine::ghost_window(f) {
        note_owner(&mut g, f);
    }
}

/// Wait until the target process stops emitting UI events: 15 ms of quiet, 150 ms at most.
pub fn settle(pid: u32) {
    let start = now_ms();
    loop {
        let last = fg().last_evt.get(&pid).copied().unwrap_or(0);
        let now = now_ms();
        if now - start.max(last) >= 15 || now - start >= 150 {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Last check after an action: the hook normally already fixed it. The app (or a parked window) still holding
/// the foreground gets it taken back, retried for 200 ms; only a foreground that stays there is an error.
pub fn fg_check(pid: u32) -> Result<(), String> {
    for _ in 0..10 {
        let cur = unsafe { GetForegroundWindow() };
        if cur.0.is_null() || (win::pid(cur) != pid && !crate::engine::ghost_window(cur)) {
            return Ok(());
        }
        let prev = give_back(&fg());
        if prev == cur || prev.0.is_null() {
            return Ok(()); // the user's own window
        }
        win::restore_foreground(prev, cur);
        std::thread::sleep(Duration::from_millis(20));
    }
    Err("background unavailable: app took focus and it could not be returned".into())
}

/// True when no object event came from that process since `since` (and it is being watched).
pub fn quiet_since(pid: u32, since: u64) -> bool {
    let g = fg();
    g.pid_hooks.contains_key(&pid) && g.last_evt.get(&pid).map(|&t| t < since).unwrap_or(true)
}
pub fn last_event(pid: u32) -> u64 {
    fg().last_evt.get(&pid).copied().unwrap_or(0)
}

// ---------- workers with a deadline ----------
struct WorkItem {
    args: Vec<String>,
    cwd: String,
    cur: isize,
    deadline: Instant,
    reply: mpsc::Sender<Reply>,
}
struct Worker {
    tx: mpsc::Sender<WorkItem>,
    retired: Arc<AtomicBool>,
}

/// One worker per target window: requests for one window run in order, and a hung app (a UIA call that never
/// returns) stalls only its own window. Past the deadline the caller gets "busy", that worker is retired and a
/// fresh one takes over; nothing queued behind it, and nothing after the deadline, is ever sent to the app.
fn new_worker() -> Worker {
    let (tx, rx) = mpsc::channel::<WorkItem>();
    let retired = Arc::new(AtomicBool::new(false));
    let r2 = retired.clone();
    std::thread::spawn(move || {
        let mut eng = Engine::new();
        for w in rx {
            if r2.load(Ordering::SeqCst) {
                return; // dropping the queue tells waiting callers "nothing sent"
            }
            if Instant::now() >= w.deadline {
                continue; // its caller already got "busy"
            }
            let rep = eng.exec(w.args, w.cwd, w.cur, w.deadline);
            let _ = w.reply.send(rep);
        }
    });
    Worker { tx, retired }
}

fn deadline(a: &[String]) -> Duration {
    let mut ms: u64 = 10_000;
    for w in a.windows(2) {
        if w[0] == "--timeout" {
            if let Ok(t) = w[1].parse::<u64>() {
                ms = ms.max(t + 3000);
            }
        }
    }
    if a.first().map(|c| c == "wait").unwrap_or(false) {
        if let Some(d) = a.last().and_then(|p| crate::engine::delay_ms(p)) {
            ms = ms.max(d + 3000);
        }
    }
    if a.first().map(|c| c == "launch" || c == "run" || c == "go").unwrap_or(false) {
        ms = ms.max(30_000);
    }
    // In a browser a click or key may wait for a navigation: up to 3 s to start and 10 s to settle.
    if a.first().map(|c| c == "click" || c == "key").unwrap_or(false) {
        ms = ms.max(16_000);
    }
    Duration::from_millis(ms)
}

struct Serve {
    workers: HashMap<isize, Worker>, // route key -> worker
    current_by: HashMap<String, isize>,
}
static SERVE: LazyLock<Mutex<Serve>> = LazyLock::new(|| Mutex::new(Serve { workers: HashMap::new(), current_by: HashMap::new() }));
const ACTING: [&str; 8] = ["click", "type", "key", "park", "show", "close", "run", "go"];

fn serve() -> std::sync::MutexGuard<'static, Serve> {
    SERVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The lock is held only to look up state, never while a command runs.
fn exec(session: &str, cwd: String, args: Vec<String>) -> (i32, String, String) {
    LAST_ACTIVITY.store(now_ms(), Ordering::SeqCst);
    let cur = serve().current_by.get(session).copied().unwrap_or(0);
    let key = crate::engine::route(&args, cur);
    let dl = deadline(&args);
    let (rtx, rrx) = mpsc::channel();
    let item = WorkItem { args: args.clone(), cwd, cur, deadline: Instant::now() + dl, reply: rtx };
    let (tx, retired) = {
        let mut s = serve();
        if !s.workers.contains_key(&key) && s.workers.len() >= 16 {
            s.workers.retain(|&k, _| k <= 0 || win::is_window(hwnd(k))); // bounded: drop workers of closed windows
        }
        let w = s.workers.entry(key).or_insert_with(new_worker);
        (w.tx.clone(), w.retired.clone())
    };
    let app = if key > 0 { win::proc_name(win::pid(hwnd(key))) } else { args.first().cloned().unwrap_or_default() };
    let acting = args.first().map(|c| ACTING.contains(&c.to_lowercase().as_str())).unwrap_or(false);
    // Before queueing behind whatever that window's worker is doing: a hung app gets an answer now, and nothing.
    if acting && key > 0 && !win::responsive(hwnd(key)) {
        return (1, String::new(), format!("busy: {app} not responding, nothing sent\n"));
    }
    if tx.send(item).is_err() {
        serve().workers.remove(&key);
        return (1, String::new(), "error: worker gone\n".into());
    }
    match rrx.recv_timeout(dl) {
        Ok(r) => {
            let mut s = serve();
            if s.current_by.len() >= 256 && !s.current_by.contains_key(session) {
                s.current_by.clear();
            }
            s.current_by.insert(session.to_string(), r.cur);
            (r.code, r.out, r.err)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => (1, String::new(), format!("busy: {app} not responding, nothing sent\n")),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            retired.store(true, Ordering::SeqCst);
            let mut s = serve();
            if s.workers.get(&key).map(|w| Arc::ptr_eq(&w.retired, &retired)).unwrap_or(false) {
                s.workers.remove(&key);
            }
            let m = if acting { format!("busy: {app} stopped responding mid-action; it may still apply\n") } else { format!("busy: {app} not responding\n") };
            (1, String::new(), m)
        }
    }
}

/// Exit without leaving any window disabled: wait (bounded) for in-flight shields, then re-enable the rest.
fn shutdown() -> ! {
    let t = Instant::now();
    while crate::engine::shields_active() && t.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
    }
    crate::engine::unshield_all();
    unlock_fg();
    std::process::exit(0)
}

// ---------- pipe server ----------
struct PipeIo(HANDLE);
impl Read for PipeIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut n = 0u32;
        unsafe { ReadFile(self.0, Some(buf), Some(&mut n), None) }.map_err(|e| io::Error::other(e.message()))?;
        Ok(n as usize)
    }
}
impl Drop for PipeIo {
    fn drop(&mut self) {
        unsafe {
            let _ = FlushFileBuffers(self.0);
            let _ = DisconnectNamedPipe(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

fn handle(h: isize) {
    let mut p = PipeIo(HANDLE(h as *mut _));
    let Ok(parts) = proto::read_request(&mut p) else { return };
    let mut it = parts.into_iter();
    let session = it.next().unwrap_or_default();
    let cwd = it.next().unwrap_or_default();
    let (code, out, err) = exec(&session, cwd, it.collect());
    let bytes = proto::encode_reply(code, &out, &err);
    let mut done = 0usize;
    while done < bytes.len() {
        let mut n = 0u32;
        if unsafe { WriteFile(p.0, Some(&bytes[done..]), Some(&mut n), None) }.is_err() || n == 0 {
            break;
        }
        done += n as usize;
    }
    drop(p);
    if STOP.load(Ordering::SeqCst) {
        shutdown();
    }
}

/// Only this user may connect: owner-only DACL, network logons denied, remote clients rejected.
fn security() -> Option<SECURITY_ATTRIBUTES> {
    unsafe {
        let mut tok = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).ok()?;
        let mut buf = [0u64; 64]; // u64 storage: TOKEN_USER needs pointer alignment
        let mut n = 0u32;
        let ok = GetTokenInformation(tok, TokenUser, Some(buf.as_mut_ptr() as *mut _), std::mem::size_of_val(&buf) as u32, &mut n).is_ok();
        let _ = CloseHandle(tok);
        if !ok {
            return None;
        }
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut sid = windows::core::PWSTR::null();
        ConvertSidToStringSidW(tu.User.Sid, &mut sid).ok()?;
        let sddl = format!("D:P(D;;GA;;;NU)(A;;GA;;;{})", sid.to_string().ok()?);
        let w = win::wide(&sddl);
        let mut sd = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(w.as_ptr()), SDDL_REVISION_1, &mut sd, None).ok()?;
        Some(SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: false.into() })
    }
}

pub fn run() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); // physical pixels everywhere
        timeBeginPeriod(1); // 1 ms sleeps for the settle/wait loops (per process on Win10 2004+)
    }
    let name = proto::pipe_name();
    let mname = win::wide(&format!("Local\\{}-daemon-{}", proto::PIPE_STEM, proto::user()));
    let _single = unsafe { CreateMutexW(None, true, PCWSTR(mname.as_ptr())) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return; // another daemon already serves this user
    }
    crate::engine::unshield_all(); // a previous daemon that died mid-call
    win::prune_parks();
    start_pump();
    LAST_ACTIVITY.store(now_ms(), Ordering::SeqCst);
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(30));
        if now_ms() - LAST_ACTIVITY.load(Ordering::SeqCst) > 15 * 60 * 1000 {
            shutdown();
        }
    });
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_millis(25));
        let mut l = FG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_ms();
        if l.held && (now > l.until || now - l.since >= LOCK_MAX_MS) {
            release(&mut l, now);
        }
    });
    let Some(sa) = security() else { return };
    let wname = win::wide(&name);
    let mut first = true;
    loop {
        let open = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { Default::default() };
        let h = unsafe {
            CreateNamedPipeW(
                PCWSTR(wname.as_ptr()),
                open,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                65536,
                65536,
                0,
                Some(&sa),
            )
        };
        if h.is_invalid() {
            if first {
                return; // someone else owns the name: never serve as a second instance
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        first = false;
        let connected = unsafe { ConnectNamedPipe(h, None) }.is_ok() || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
        if !connected {
            unsafe {
                let _ = CloseHandle(h);
            }
            continue;
        }
        let hv = h.0 as isize;
        std::thread::spawn(move || handle(hv));
    }
}
