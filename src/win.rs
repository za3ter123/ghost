//! Plain Win32 window helpers: listing, naming, parking, the foreground restore, small files.
use std::path::PathBuf;
use windows::core::{BOOL, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::*;

pub fn hwnd(n: isize) -> HWND {
    HWND(n as *mut _)
}
pub fn hv(h: HWND) -> isize {
    h.0 as isize
}

pub fn top_windows() -> Vec<HWND> {
    unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> BOOL {
        (*(l.0 as *mut Vec<HWND>)).push(h);
        true.into()
    }
    let mut v: Vec<HWND> = Vec::with_capacity(256);
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

pub fn child_windows(h: HWND) -> Vec<HWND> {
    unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> BOOL {
        (*(l.0 as *mut Vec<HWND>)).push(h);
        true.into()
    }
    let mut v: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumChildWindows(Some(h), Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

pub fn title(h: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(h);
        if n <= 0 {
            return String::new();
        }
        let mut b = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(h, &mut b);
        String::from_utf16_lossy(&b[..got.max(0) as usize])
    }
}

pub fn class_of(h: HWND) -> String {
    let mut b = [0u16; 256];
    let n = unsafe { GetClassNameW(h, &mut b) };
    String::from_utf16_lossy(&b[..n.max(0) as usize])
}

pub fn is_window(h: HWND) -> bool {
    !h.0.is_null() && unsafe { IsWindow(Some(h)).as_bool() }
}
pub fn visible(h: HWND) -> bool {
    unsafe { IsWindowVisible(h).as_bool() }
}
pub fn cloaked(h: HWND) -> bool {
    let mut v: u32 = 0;
    unsafe { DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut v as *mut _ as *mut _, 4).is_ok() && v != 0 }
}
pub fn listable(h: HWND) -> bool {
    visible(h) && !title(h).is_empty() && !cloaked(h)
}
pub fn pid(h: HWND) -> u32 {
    let mut p = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut p)) };
    p
}
/// The process behind a window: for a UWP frame (ApplicationFrameHost) the app whose CoreWindow it hosts.
pub fn app_pid(h: HWND) -> u32 {
    if class_of(h) == "ApplicationFrameWindow" {
        if let Some(c) = child_windows(h).into_iter().find(|&c| class_of(c) == "Windows.UI.Core.CoreWindow") {
            return pid(c);
        }
    }
    pid(h)
}

pub fn thread_of(h: HWND) -> u32 {
    unsafe { GetWindowThreadProcessId(h, None) }
}

/// Exe name without extension; OpenProcess + QueryFullProcessImageName costs microseconds.
pub fn proc_name(pid: u32) -> String {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return "?".into() };
        let mut b = [0u16; 1024];
        let mut n = b.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(b.as_mut_ptr()), &mut n).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return "?".into();
        }
        let p = String::from_utf16_lossy(&b[..n as usize]);
        std::path::Path::new(&p).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "?".into())
    }
}

pub fn line(h: HWND) -> String {
    let p = pid(h);
    format!("{} {} {} \"{}\"", hv(h), p, proc_name(p), title(h))
}

pub fn rect(h: HWND) -> RECT {
    let mut r = RECT::default();
    unsafe {
        let _ = GetWindowRect(h, &mut r);
    }
    r
}

pub fn virtual_screen() -> RECT {
    unsafe {
        let l = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let t = GetSystemMetrics(SM_YVIRTUALSCREEN);
        RECT { left: l, top: t, right: l + GetSystemMetrics(SM_CXVIRTUALSCREEN), bottom: t + GetSystemMetrics(SM_CYVIRTUALSCREEN) }
    }
}
pub fn intersects(a: &RECT, b: &RECT) -> bool {
    a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top
}
pub fn parked(h: HWND) -> bool {
    if h.0.is_null() {
        return false;
    }
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r).is_ok() && !intersects(&r, &virtual_screen()) }
}
pub fn root_of(h: HWND) -> HWND {
    unsafe { GetAncestor(h, GA_ROOT) }
}

pub fn state_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    let d = PathBuf::from(base).join("ghost");
    let _ = std::fs::create_dir_all(&d);
    d
}
pub fn park_file(h: HWND) -> PathBuf {
    state_dir().join(format!("park-{}.txt", hv(h)))
}
/// `left top right bottom pid start`: the rect to restore, and the window's process by pid and creation time,
/// so a park file left behind never claims a reused hwnd.
fn save_park(h: HWND, r: &RECT) {
    let p = pid(h);
    let start = crate::engine::proc_start(p).unwrap_or(0);
    let _ = std::fs::write(park_file(h), format!("{} {} {} {} {p} {start}", r.left, r.top, r.right, r.bottom));
}
/// Ghost parked this window: its park file names this window's process (pid and creation time).
pub fn park_owned(h: HWND) -> bool {
    let Ok(s) = std::fs::read_to_string(park_file(h)) else { return false };
    let p: Vec<u64> = s.split(' ').skip(4).filter_map(|x| x.trim().parse().ok()).collect();
    p.len() == 2 && is_window(h) && pid(h) as u64 == p[0] && crate::engine::proc_start(p[0] as u32) == Some(p[1])
}
/// Daemon start: park files whose window is gone or now belongs to another process are dropped.
pub fn prune_parks() {
    let Ok(d) = std::fs::read_dir(state_dir()) else { return };
    for e in d.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if let Some(v) = n.strip_prefix("park-").and_then(|x| x.strip_suffix(".txt")).and_then(|x| x.parse::<isize>().ok()) {
            if !park_owned(hwnd(v)) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

const HWND_BOTTOM: HWND = HWND(1 as *mut _);

pub fn sink(h: HWND) {
    unsafe {
        let _ = SetWindowPos(h, Some(HWND_BOTTOM), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOMOVE);
    }
}

/// Off every monitor, bottom of the z-order, never minimized (it must keep rendering for UIA and capture).
pub fn park(h: HWND) -> Result<bool, String> {
    unsafe {
        if IsIconic(h).as_bool() || IsZoomed(h).as_bool() {
            let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        }
        let r = rect(h);
        let vs = virtual_screen();
        if !intersects(&r, &vs) {
            sink(h); // already parked; keep the saved rect
            return Ok(false);
        }
        save_park(h, &r);
        let x = vs.left - (r.right - r.left) - 200;
        let y = r.top.min(vs.bottom - 100).max(vs.top);
        // HWND_BOTTOM: a parked window high in the z-order is what Alt+Tab, or a closing foreground window, hands focus to.
        SetWindowPos(h, Some(HWND_BOTTOM), x, y, 0, 0, SWP_NOACTIVATE | SWP_NOSIZE)
            .map_err(|e| format!("SetWindowPos failed (elevated window?) {}", e.message()))?;
        Ok(true)
    }
}

/// For windows an app opens by itself (dialogs, popups, a main window after a splash): parked like `park`, but
/// posted (SWP_ASYNCWINDOWPOS) so the event pump never waits on a busy app. Next to a parked owner it is
/// centred on the owner, so it stays beside it; if that would still touch a monitor it is parked on its own.
pub fn park_new(h: HWND) {
    let r = rect(h);
    let vs = virtual_screen();
    if !intersects(&r, &vs) {
        return;
    }
    save_park(h, &r);
    let (w, ht) = (r.right - r.left, r.bottom - r.top);
    let mut at = (vs.left - w - 200, r.top.min(vs.bottom - 100).max(vs.top));
    if let Ok(o) = unsafe { GetWindow(h, GW_OWNER) } {
        if !o.0.is_null() && parked(o) {
            let or = rect(o);
            let (x, y) = (or.left + (or.right - or.left - w) / 2, or.top + (or.bottom - or.top - ht) / 2);
            if !intersects(&RECT { left: x, top: y, right: x + w, bottom: y + ht }, &vs) {
                at = (x, y);
            }
        }
    }
    unsafe {
        let _ = SetWindowPos(h, Some(HWND_BOTTOM), at.0, at.1, 0, 0, SWP_NOACTIVATE | SWP_NOSIZE | SWP_ASYNCWINDOWPOS);
    }
}

fn saved_rect(h: HWND) -> Option<RECT> {
    let s = std::fs::read_to_string(park_file(h)).ok()?;
    let p: Vec<i32> = s.split(' ').take(4).filter_map(|x| x.trim().parse().ok()).collect();
    (p.len() == 4).then(|| RECT { left: p[0], top: p[1], right: p[2], bottom: p[3] })
}

pub fn show(h: HWND) {
    let r = rect(h);
    let (x, y, w, ht) = match saved_rect(h) {
        Some(s) => {
            let _ = std::fs::remove_file(park_file(h));
            (s.left, s.top, s.right - s.left, s.bottom - s.top)
        }
        None => unsafe {
            let (w, ht) = (r.right - r.left, r.bottom - r.top);
            ((GetSystemMetrics(SM_CXSCREEN) - w) / 2, (GetSystemMetrics(SM_CYSCREEN) - ht) / 2, w, ht)
        },
    };
    // HWND_TOP without activation: visible, focus untouched.
    unsafe {
        let _ = SetWindowPos(h, None, x, y, w, ht, SWP_NOACTIVATE);
    }
}

pub fn close(h: HWND) {
    if let Some(s) = saved_rect(h) {
        // Un-park behind every window first, so apps that remember their position don't reopen off screen.
        unsafe {
            // Async: a synchronous SetWindowPos waits on a busy app's UI thread (a browser under load).
            let _ = SetWindowPos(h, Some(HWND_BOTTOM), s.left, s.top, s.right - s.left, s.bottom - s.top, SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS);
        }
    }
    unsafe {
        let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    let _ = std::fs::remove_file(park_file(h));
}

/// A window that can take the foreground back: it exists and is not ghost's (not a launched or parked window, not
/// any window of a process ghost launched). Shared by handback and restore_foreground's step-down.
fn owners(h: HWND) -> bool {
    is_window(h) && !crate::engine::ghost_window(h) && !crate::engine::ghost_pid(app_pid(h))
}

/// Where the foreground goes back to after an app took it: the `prior` foreground if it still exists, else the
/// owner's `last_user` visible window. Only when there was none (prior 0 or the taskbar, or both gone) ghost's
/// sink, a hidden window that swallows every key, so keystrokes go nowhere, as they did at foreground 0. The
/// taskbar only when there is no sink.
pub fn handback(prior: HWND, last_user: HWND) -> HWND {
    let sink = hwnd(SINK.load(std::sync::atomic::Ordering::SeqCst));
    let theirs = owners;
    // The taskbar in front is how Windows shows "no window" (desktop click, natural fg 0): treated as prior 0.
    if !prior.0.is_null() && class_of(prior) != "Shell_TrayWnd" {
        // Still there: that exact window, even hidden or transient (Task View, ForegroundStaging). It is what the
        // owner had, and the sink in its place swallowed their keys (round 10: 19 of 31 sink handbacks were these).
        if theirs(prior) {
            return prior;
        }
        // Gone: the owner's last visible window, which the hook tracks.
        if theirs(last_user) && user_window(last_user) {
            crate::daemon::dbg(&format!("handback: prior {} gone, last user window {}", hv(prior), hv(last_user)));
            return last_user;
        }
    }
    if is_window(sink) {
        crate::daemon::dbg(&format!("handback: sink, prior {} (window {}, class {}), last user {}", hv(prior), is_window(prior), class_of(prior), hv(last_user)));
        return sink;
    }
    taskbar()
}
pub fn taskbar() -> HWND {
    unsafe { FindWindowW(windows::core::w!("Shell_TrayWnd"), PCWSTR::null()).unwrap_or_default() }
}

/// The foreground sink: a 1x1 top-level tool window off every monitor (so not on screen, not on the taskbar, not
/// in Alt+Tab, no title so not in `ghost windows`) that discards keyboard input. Created on the daemon's pump
/// thread, whose message loop serves it.
pub static SINK: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
pub const SINK_CLASS: &str = "ghost-sink";
unsafe extern "system" fn sink_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> windows::Win32::Foundation::LRESULT {
    match m {
        // Keys (Alt+F4 included) and close requests do nothing.
        WM_KEYDOWN | WM_KEYUP | WM_CHAR | WM_DEADCHAR | WM_SYSKEYDOWN | WM_SYSKEYUP | WM_SYSCHAR | WM_SYSDEADCHAR | WM_CLOSE => windows::Win32::Foundation::LRESULT(0),
        WM_SYSCOMMAND if (w.0 & 0xFFF0) as u32 == SC_CLOSE => windows::Win32::Foundation::LRESULT(0),
        _ => DefWindowProcW(h, m, w, l),
    }
}
pub fn create_sink() {
    unsafe {
        let cls = wide(SINK_CLASS);
        // A null module handle names the exe itself for a class it registers.
        let wc = WNDCLASSW { lpfnWndProc: Some(sink_proc), lpszClassName: PCWSTR(cls.as_ptr()), ..Default::default() };
        RegisterClassW(&wc);
        let vs = virtual_screen();
        let empty = wide("");
        if let Ok(h) = CreateWindowExW(WS_EX_TOOLWINDOW, PCWSTR(cls.as_ptr()), PCWSTR(empty.as_ptr()), WS_POPUP, vs.left - 4000, vs.top, 1, 1, None, None, None, None) {
            let _ = ShowWindow(h, SW_SHOWNOACTIVATE); // visible (off screen): an invisible window can't be foreground
            SINK.store(hv(h), std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// Transient shell windows (Alt+Tab's ForegroundStaging, invisible owners) are never a window to give focus back to.
pub fn user_window(h: HWND) -> bool {
    visible(h) && class_of(h) != "ForegroundStaging"
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(name: *const u16) -> isize;
    fn GetProcAddress(module: isize, name: *const u8) -> isize;
}
/// user32's HungWindowFromGhostWindow: exported but not in the import library, so looked up at run time.
unsafe fn hung_from_ghost(h: HWND) -> HWND {
    let m = GetModuleHandleW(wide("user32.dll").as_ptr());
    let f = if m == 0 { 0 } else { GetProcAddress(m, b"HungWindowFromGhostWindow\0".as_ptr()) };
    if f == 0 {
        return HWND::default();
    }
    let f: extern "system" fn(HWND) -> HWND = std::mem::transmute(f);
    f(h)
}
/// The owner's window to remember for `h`. When an app hangs, DWM puts its own stand-in (class "Ghost") in front,
/// and destroys it once the app answers again; remembering the stand-in made the handback fall to the sink while
/// the owner's hung app was still in front (measured 2026-10-17, a hung desktop app). The hung app's real window is kept.
pub fn owner_window(h: HWND) -> HWND {
    // HungWindowFromGhostWindow answers null for any window that isn't DWM's stand-in.
    if class_of(h) == "Ghost" {
        let real = unsafe { hung_from_ghost(h) };
        if is_window(real) {
            return real;
        }
    }
    h
}

/// `cur` is the app window that took the foreground; give it back to `prev`.
/// Gives the foreground back to `prev`, and checks it took: a window that exists can still refuse it (explorer's
/// ForegroundStaging did, and the owner was left on it for 2.7 s, budget N2 2026-10-17). Not there within ~30 ms,
/// it steps down to the owner's last visible window, then the sink. Stops if someone else took the foreground.
pub fn restore_foreground(prev: HWND, cur: HWND) {
    let sink = hwnd(SINK.load(std::sync::atomic::Ordering::SeqCst));
    let last = hwnd(crate::daemon::last_user());
    let steps = steps(prev, last, sink, |h| owners(h) && user_window(h));
    let got = first_taken(&steps, |t| {
        // From whatever of ghost's is in front now: another window of the same app may have taken over from `cur`
        // (budget N2, 2026-10-18: a Calculator frame held it while the guard kept restoring from an Explorer window).
        let f = unsafe { GetForegroundWindow() };
        restore_once(t, if !f.0.is_null() && is_window(f) && !owners(f) { f } else { cur });
        let start = std::time::Instant::now();
        loop {
            let f = unsafe { GetForegroundWindow() };
            if f == t {
                return Some(true);
            }
            if !f.0.is_null() && f != cur && owners(f) {
                return None; // the owner (or another app) moved on: ghost's steal is over, never fight it
            }
            if start.elapsed() >= std::time::Duration::from_millis(30) {
                return Some(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    });
    if got != Some(prev) {
        crate::daemon::dbg(&format!("restore: {} refused; {}", hv(prev), got.map(|g| format!("gave it to {}", hv(g))).unwrap_or("stopped".into())));
    }
}

/// The hand-back order: `to`, then the last user window (when it is one), then the sink; each once. To the sink
/// means there was no window, so nothing comes before it.
fn steps(to: HWND, last: HWND, sink: HWND, usable: impl Fn(HWND) -> bool) -> Vec<HWND> {
    let mut v = vec![to];
    if to != sink {
        if last != to && usable(last) {
            v.push(last);
        }
        if !sink.0.is_null() {
            v.push(sink);
        }
    }
    v
}

/// The first step `attempt` reports taken (Some(true)); Some(false) tries the next, None stops.
fn first_taken(steps: &[HWND], mut attempt: impl FnMut(HWND) -> Option<bool>) -> Option<HWND> {
    for &t in steps {
        match attempt(t) {
            Some(true) => return Some(t),
            Some(false) => continue,
            None => return None,
        }
    }
    None
}

fn restore_once(prev: HWND, cur: HWND) {
    unsafe {
        let t = thread_of(cur);
        let me = GetCurrentThreadId();
        // To the sink, try without attaching first: attached, SetForegroundWindow waits on the app's thread, and a
        // browser busy starting a navigation kept the foreground ~100 ms (measured 2026-10-17). A refusal here
        // can't flash anything: the sink has no taskbar button.
        let to_sink = hv(prev) == SINK.load(std::sync::atomic::Ordering::SeqCst);
        let sfw = is_window(prev) && ((to_sink && SetForegroundWindow(prev).as_bool() && GetForegroundWindow() == prev) || {
            let att = t != 0 && t != me && AttachThreadInput(t, me, true).as_bool();
            let r = SetForegroundWindow(prev).as_bool();
            if att {
                let _ = AttachThreadInput(t, me, false);
            }
            r
        });
        let root = root_of(cur);
        let ours = is_window(root) && !owners(root); // same rule as handback: launched, parked, or a launched pid
        if ours {
            sink(root);
        }
        if sfw && GetForegroundWindow() == prev {
            return;
        }
        // Hiding the active window makes its own (foreground) thread hand activation to the next window.
        // Only for ghost's own (parked) windows: the user never sees the blink, and never their windows.
        if !ours {
            return;
        }
        let _ = ShowWindow(root, SW_HIDE);
        let _ = ShowWindow(root, SW_SHOWNA);
        sink(root);
        if is_window(prev) && GetForegroundWindow() != prev {
            let _ = SetForegroundWindow(prev);
        }
        // The sink refused: the taskbar, logged (keys typed there do no harm).
        if prev == hwnd(SINK.load(std::sync::atomic::Ordering::SeqCst)) && GetForegroundWindow() != prev {
            let now = GetForegroundWindow();
            let t = thread_of(now);
            let att = t != 0 && t != me && AttachThreadInput(t, me, true).as_bool();
            let _ = SetForegroundWindow(taskbar());
            if att {
                let _ = AttachThreadInput(t, me, false);
            }
            crate::daemon::dbg(&format!("sink could not be activated; foreground to the taskbar, now {}", hv(GetForegroundWindow())));
        }
        crate::daemon::dbg(&format!("restore via hide, fg={}", hv(GetForegroundWindow())));
    }
}


#[cfg(test)]
mod cascade_tests {
    use super::*;
    #[test]
    fn refused_prior_steps_down_to_last_user_then_sink() {
        let (prior, last, sink) = (hwnd(11), hwnd(22), hwnd(33));
        let st = steps(prior, last, sink, |_| true);
        assert_eq!(st, vec![prior, last, sink]);
        // prior exists but refuses the foreground: the owner's last window gets it
        assert_eq!(first_taken(&st, |t| Some(t != prior)), Some(last));
        // both refuse: the sink
        assert_eq!(first_taken(&st, |t| Some(t == sink)), Some(sink));
        // someone else took the foreground meanwhile: stop at once
        assert_eq!(first_taken(&st, |_| None), None);
        // no usable last window: straight to the sink; a sink target has no step before or after it
        assert_eq!(steps(prior, last, sink, |_| false), vec![prior, sink]);
        assert_eq!(steps(sink, last, sink, |_| true), vec![sink]);
    }
}

pub fn screen_to_client(h: HWND, p: POINT) -> POINT {
    let mut q = p;
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::ScreenToClient(h, &mut q);
    }
    q
}

/// The window's thread answers a no-op message within 200 ms (a hung or busy app does not).
pub fn responsive(h: HWND) -> bool {
    let mut r = 0usize;
    unsafe { SendMessageTimeoutW(h, WM_NULL, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG | SMTO_BLOCK, 200, Some(&mut r)).0 != 0 }
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
