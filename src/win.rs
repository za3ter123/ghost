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
            let _ = SetWindowPos(h, Some(HWND_BOTTOM), s.left, s.top, s.right - s.left, s.bottom - s.top, SWP_NOACTIVATE);
        }
    }
    unsafe {
        let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    let _ = std::fs::remove_file(park_file(h));
}

/// Where the foreground goes back to after an app took it: the user's window `prev`, or, when there was none
/// (foreground 0), it is gone, or it is the taskbar, ghost's sink, a hidden window that swallows every key, so
/// keystrokes go nowhere, as they did at foreground 0. The taskbar only when there is no sink.
pub fn handback(prev: HWND) -> HWND {
    let sink = hwnd(SINK.load(std::sync::atomic::Ordering::SeqCst));
    if is_window(prev) && user_window(prev) && !crate::engine::ghost_window(prev) && class_of(prev) != "Shell_TrayWnd" {
        return prev;
    }
    if is_window(sink) {
        crate::daemon::dbg(&format!("handback: sink, prev {} (window {}, user {}, class {})", hv(prev), is_window(prev), user_window(prev), class_of(prev)));
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

/// `cur` is the app window that took the foreground; give it back to `prev`.
pub fn restore_foreground(prev: HWND, cur: HWND) {
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
        let ours = crate::engine::ghost_window(root);
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
