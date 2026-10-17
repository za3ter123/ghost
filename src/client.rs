//! Thin client: no COM, no UI work. Connect to the per-user pipe (starting the daemon hidden if it is
//! missing), send argv + cwd + session key, stream the reply, exit with the daemon's code.
use crate::proto;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::{Duration, Instant};
use windows::core::{PCWSTR, PWSTR};
use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows::Win32::System::Threading::{
    CreateProcessW, OpenProcess, QueryFullProcessImageNameW, CREATE_NO_WINDOW, DETACHED_PROCESS,
    PROCESS_INFORMATION, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    STARTF_USESHOWWINDOW, STARTUPINFOW,
};

pub fn run(args: Vec<String>) -> i32 {
    let args = ps51_args().unwrap_or(args);
    let exe = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => return fail(&e.to_string()),
    };
    let stop = args.len() == 1 && args[0] == "daemon-stop";
    let mut f = match connect(&proto::pipe_name(), &exe, stop) {
        Ok(Some(f)) => f,
        Ok(None) => return 0,
        Err(m) => return fail(&m),
    };
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let mut parts = Vec::with_capacity(args.len() + 2);
    parts.push(session());
    parts.push(cwd);
    parts.extend(args);
    if let Err(e) = f.write_all(&proto::encode(&parts)) {
        return fail(&format!("write: {e}"));
    }
    match proto::read_reply(&mut f) {
        Ok((code, out, err)) => {
            let _ = std::io::stdout().write_all(&out);
            let _ = std::io::stderr().write_all(&err);
            code
        }
        Err(e) => fail(&format!("daemon closed the connection: {e}")),
    }
}

/// Windows PowerShell 5.1 passes an argument that has spaces as "..." without escaping the quotes inside it, so
/// 'key "Address bar" enter' arrives as several broken argv pieces. When our parent is powershell.exe, argv
/// is rebuilt from the raw command line by inverting exactly that rule (pwsh 7.3+ escapes properly).
fn ps51_args() -> Option<Vec<String>> {
    let (parent, _) = parent_of(std::process::id());
    if parent_of(parent).1 != "powershell.exe" {
        return None;
    }
    let raw = unsafe { windows::Win32::System::Environment::GetCommandLineW().to_string().ok()? };
    let mut v = split_ps51(&raw);
    (!v.is_empty()).then(|| {
        v.remove(0); // the exe
        v
    })
}

/// A token PowerShell wrapped starts with a quote; it ends at a quote followed by whitespace (or the end) once
/// the unescaped quotes inside it are paired. Inner text, \" included, is kept as typed for `run` to split.
fn split_ps51(s: &str) -> Vec<String> {
    let c: Vec<char> = s.chars().collect();
    let (mut out, mut i) = (vec![], 0);
    while i < c.len() {
        if c[i].is_whitespace() {
            i += 1;
            continue;
        }
        if c[i] != '"' {
            let st = i;
            while i < c.len() && !c[i].is_whitespace() {
                i += 1;
            }
            out.push(c[st..i].iter().collect());
            continue;
        }
        let (st, mut inner) = (i + 1, 0);
        i += 1;
        while i < c.len() {
            if c[i] == '"' {
                let at_end = i + 1 >= c.len() || c[i + 1].is_whitespace();
                if at_end && inner % 2 == 0 {
                    break;
                }
                if c[i - 1] != '\\' {
                    inner += 1;
                }
            }
            i += 1;
        }
        out.push(c[st..i.min(c.len())].iter().collect());
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn ps51_raw_line() {
        // What Windows PowerShell 5.1 really passed (seen via cmd /c echo) for:
        // run 123 'key "Address and search bar" enter' 'type Body ""' 'type Body "a \"q\" b"' 'read Body' 'C:\o d\'
        let raw = r#"ghost.exe run 123 "key "Address and search bar" enter" "type Body """ "type Body "a \"q\" b"" "read Body" "C:\o d\""#;
        let want = ["ghost.exe", "run", "123", r#"key "Address and search bar" enter"#, r#"type Body """#, r#"type Body "a \"q\" b""#, "read Body", r"C:\o d\"];
        assert_eq!(super::split_ps51(raw), want);
    }
}

fn fail(m: &str) -> i32 {
    eprintln!("ghost: {m}");
    1
}

fn open(pipe: &str) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).open(pipe)
}

fn connect(pipe: &str, exe: &str, stop: bool) -> Result<Option<File>, String> {
    let mut deadline = Instant::now() + Duration::from_secs(2);
    let mut started = false;
    let mut takeover: Option<Instant> = None; // overall limit for replacing an older daemon
    loop {
        match open(pipe) {
            Ok(mut f) => {
                let img = server_image(&f).unwrap_or_default();
                if img.eq_ignore_ascii_case(exe) {
                    return Ok(Some(f));
                }
                // Trust boundary: only talk to our own daemon. The one exception is the legacy C# ghostd.exe
                // next to us, which we ask to stop so this version can take the pipe over.
                if is_legacy(&img, exe) {
                    let until = *takeover.get_or_insert_with(|| Instant::now() + Duration::from_secs(3));
                    if Instant::now() > until {
                        return Err(format!("an older ghost daemon ({img}) still holds {pipe} after 3 s; end that process, then retry"));
                    }
                    let _ = f.write_all(&proto::encode(&["0".into(), String::new(), "daemon-stop".into()]));
                    let _ = proto::read_reply(&mut f);
                    drop(f);
                    let t = Instant::now() + Duration::from_secs(3);
                    while open(pipe).is_ok() && Instant::now() < t {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    if stop {
                        return Ok(None);
                    }
                    deadline = Instant::now() + Duration::from_secs(2);
                    continue;
                }
                return Err(format!("pipe {pipe} is not served by {exe} ({img})"));
            }
            Err(e) => {
                if stop {
                    return Ok(None); // nothing running, nothing to stop
                }
                if !started && e.raw_os_error() != Some(231) {
                    start_daemon(exe).map_err(|e| format!("cannot start daemon: {e}"))?;
                    started = true;
                }
                if Instant::now() > deadline {
                    return Err(format!("daemon did not answer within 2 s: {e}"));
                }
                std::thread::sleep(Duration::from_millis(3));
            }
        }
    }
}

/// An older ghost daemon from our own directory: the C# ghostd.exe, or a previous build that build.ps1
/// renamed aside (ghost.old*.exe) because a running daemon locks its exe.
fn is_legacy(img: &str, exe: &str) -> bool {
    let (i, e) = (Path::new(img), Path::new(exe));
    let name = i.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    (name == "ghostd.exe" || (name.starts_with("ghost.old") && name.ends_with(".exe")))
        && i.parent().map(|p| p.to_string_lossy().to_lowercase()) == e.parent().map(|p| p.to_string_lossy().to_lowercase())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Detached, no console, no inherited handles: our stdout pipe must close when we exit.
fn start_daemon(exe: &str) -> windows::core::Result<()> {
    let app = wide(exe);
    let mut cmd = wide(&format!("\"{exe}\" --daemon"));
    let dir = wide(&Path::new(exe).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default());
    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESHOWWINDOW,
        wShowWindow: 0, // SW_HIDE
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            PCWSTR(app.as_ptr()),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            DETACHED_PROCESS | CREATE_NO_WINDOW,
            None,
            PCWSTR(dir.as_ptr()),
            &si,
            &mut pi,
        )?;
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
    }
    Ok(())
}

fn image_of(h: HANDLE) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut n = buf.len() as u32;
    unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n).ok()? };
    Some(String::from_utf16_lossy(&buf[..n as usize]))
}

fn server_image(f: &File) -> Option<String> {
    let mut pid = 0u32;
    unsafe {
        GetNamedPipeServerProcessId(HANDLE(f.as_raw_handle()), &mut pid).ok()?;
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let img = image_of(h);
        let _ = CloseHandle(h);
        img
    }
}

const SHELLS: [&str; 7] = ["bash.exe", "sh.exe", "powershell.exe", "pwsh.exe", "cmd.exe", "conhost.exe", "env.exe"];

/// Names the caller so each agent keeps its own current window in the shared daemon:
/// GHOST_SESSION if set, else the nearest ancestor that is not a shell (an agent's node process, say).
fn session() -> String {
    if let Ok(s) = std::env::var("GHOST_SESSION") {
        if !s.is_empty() {
            return s;
        }
    }
    let (mut p, _) = parent_of(std::process::id());
    for _ in 0..16 {
        let (pp, name) = parent_of(p);
        if !SHELLS.contains(&name.as_str()) || pp == 0 || pp == p {
            break;
        }
        p = pp;
    }
    p.to_string()
}

/// Parent pid and lowercase exe name, from per-process queries (a Toolhelp snapshot costs ~5 ms).
fn parent_of(pid: u32) -> (u32, String) {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return (0, String::new()) };
        let mut pbi = [0usize; 6]; // PROCESS_BASIC_INFORMATION; [5] = InheritedFromUniqueProcessId
        let mut n = 0u32;
        let st = NtQueryInformationProcess(h, ProcessBasicInformation, pbi.as_mut_ptr() as *mut _, std::mem::size_of_val(&pbi) as u32, &mut n);
        let name = image_of(h)
            .and_then(|p| Path::new(&p).file_name().map(|n| n.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        let _ = CloseHandle(h);
        let parent = if st.is_ok() { pbi[5] as u32 } else { 0 };
        (parent, name)
    }
}
