//! The DevTools protocol on ghost's own browsers: `go` navigates the existing tab (no new tab and no tab close, each
//! of which activated the browser, measured 2026-10-17), `click` and `type` act on page elements without UIA's
//! SetValue (which activated Edge too), and `close` asks the browser to shut down. Only ghost's throwaway profiles
//! are started with `--remote-debugging-port=0` (Chromium binds it to 127.0.0.1 on a random port and writes it to
//! <profile>\DevToolsActivePort), so this never reaches a user's own browser. A minimal websocket client, std only:
//! the upgrade handshake and masked text frames; localhost needs no TLS.
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// One websocket to a DevTools target of a ghost-launched browser.
pub struct Session {
    s: TcpStream,
    id: u32,
}

/// Why a command failed: before it went out (nothing happened) or after (the browser may still act on it).
#[derive(Debug)]
pub enum CallErr {
    Unsent(String),
    Sent(String),
}
impl std::fmt::Display for CallErr {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            CallErr::Unsent(m) | CallErr::Sent(m) => f.write_str(m),
        }
    }
}

fn port_of(profile: &str) -> Result<(u16, String), String> {
    let f = std::fs::read_to_string(std::path::Path::new(profile).join("DevToolsActivePort")).map_err(|e| format!("no DevToolsActivePort: {e}"))?;
    let mut l = f.lines();
    let port: u16 = l.next().and_then(|l| l.trim().parse().ok()).ok_or("DevToolsActivePort has no port")?;
    Ok((port, l.next().unwrap_or("").trim().to_string()))
}

fn connect(port: u16, read: u64) -> Result<TcpStream, String> {
    let s = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(2)).map_err(|e| format!("connect 127.0.0.1:{port}: {e}"))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(read)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
    Ok(s)
}

impl Session {
    /// The page target whose title starts the window's `title` (else the first page).
    pub fn page(profile: &str, title: &str) -> Result<Session, String> {
        let (port, _) = port_of(profile)?;
        let mut s = connect(port, 2)?;
        write!(s, "GET /json/list HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").map_err(|e| e.to_string())?;
        // Chromium keeps the connection open despite Connection: close, so read by Content-Length, not to EOF.
        let mut body = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = s.read(&mut buf).map_err(|e| format!("/json/list: {e}"))?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&buf[..n]);
            let t = String::from_utf8_lossy(&body);
            if let Some(i) = t.find("\r\n\r\n") {
                let len = t[..i].lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()));
                if len.is_some_and(|len| body.len() >= i + 4 + len) {
                    break;
                }
            }
        }
        let body = String::from_utf8_lossy(&body);
        let pages: Vec<(String, String)> = objects(&body)
            .into_iter()
            .filter(|o| get(o, "type") == Some("page"))
            .filter_map(|o| Some((get(&o, "title").unwrap_or("").to_string(), get(&o, "webSocketDebuggerUrl")?.to_string())))
            .collect();
        let ws = pages.iter().find(|(t, _)| !t.is_empty() && title.starts_with(t.as_str())).or(pages.first()).map(|(_, w)| w.clone()).ok_or("no page target in /json/list")?;
        let path = ws.find("/devtools/").map(|i| ws[i..].to_string()).ok_or_else(|| format!("odd target url {ws}"))?;
        Session::open(port, &path)
    }

    /// The browser target (DevToolsActivePort's second line), for Browser.close.
    pub fn browser(profile: &str) -> Result<Session, String> {
        let (port, path) = port_of(profile)?;
        if !path.starts_with("/devtools/browser/") {
            return Err("DevToolsActivePort has no browser target".into());
        }
        Session::open(port, &path)
    }

    fn open(port: u16, path: &str) -> Result<Session, String> {
        let mut s = connect(port, 10)?;
        // shortcut: a fixed Sec-WebSocket-Key and no check of the server's accept hash; fine for our own localhost browser.
        write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").map_err(|e| e.to_string())?;
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() > 8192 || s.read(&mut b).map_err(|e| format!("handshake: {e}"))? == 0 {
                return Err("handshake: connection closed".into());
            }
            head.push(b[0]);
        }
        if !head.starts_with(b"HTTP/1.1 101") {
            return Err(format!("handshake refused: {}", String::from_utf8_lossy(&head).lines().next().unwrap_or("")));
        }
        Ok(Session { s, id: 0 })
    }

    /// Sends one command (`params` is a JSON object) and returns its reply's text.
    pub fn call(&mut self, method: &str, params: &str) -> Result<String, CallErr> {
        self.id += 1;
        let id = self.id;
        let msg = format!(r#"{{"id":{id},"method":"{method}","params":{params}}}"#);
        self.s.write_all(&frame(msg.as_bytes(), mask_key())).map_err(|e| CallErr::Unsent(format!("{method}: {e}")))?;
        let (tag, tag_end) = (format!("\"id\":{id},"), format!("\"id\":{id}}}"));
        loop {
            match read_frame(&mut self.s) {
                Ok((1, p)) => {
                    let t = String::from_utf8_lossy(&p).into_owned();
                    if t.starts_with(&format!("{{{tag}")) || t.starts_with(&format!("{{{tag_end}")) {
                        // Chromium writes `{"id":N,"result":...}` or `{"id":N,"error":...}`: only that top-level
                        // key is an error; "error" inside a result (page text, a value) is not.
                        if top_level_error(&t) {
                            let m = objects(&t).iter().find_map(|o| get(o, "message").map(str::to_string)).unwrap_or_else(|| t.clone());
                            return Err(CallErr::Sent(format!("{method}: {m}")));
                        }
                        return Ok(t);
                    }
                }
                Ok((8, _)) => return Err(CallErr::Sent(format!("{method}: the browser closed the connection"))),
                Ok(_) => {}
                Err(e) => return Err(CallErr::Sent(format!("{method}: no reply: {e}"))),
            }
        }
    }

    /// `call` for read-only or retry-safe steps: any failure as text.
    fn ask(&mut self, method: &str, params: &str) -> Result<String, String> {
        self.call(method, params).map_err(|e| e.to_string())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.s.write_all(&frame_op(0x8, &[], mask_key()));
    }
}

/// Navigates the window's tab. Err: CDP is not available (caller falls back to the hand-off); Ok(Some(note)): the
/// command went out but the browser reported a problem or no reply (it may still be under way, so no fallback).
pub fn navigate(profile: &str, title: &str, url: &str) -> Result<Option<String>, String> {
    let mut s = Session::page(profile, title)?;
    match s.call("Page.navigate", &format!(r#"{{"url":{}}}"#, json_str(url))) {
        Ok(t) => Ok(objects(&t).iter().find_map(|o| get(o, "errorText").map(|e| format!("browser reported: {e}")))),
        Err(CallErr::Unsent(e)) => Err(e),
        Err(CallErr::Sent(e)) => Ok(Some(e)),
    }
}

/// Browser.close on a ghost-launched browser: it shuts down whatever its UI thread is busy with.
pub fn close(profile: &str) -> Result<(), String> {
    let mut s = Session::browser(profile)?;
    match s.call("Browser.close", "{}") {
        Ok(_) | Err(CallErr::Sent(_)) => Ok(()), // it may drop the socket before replying
        Err(CallErr::Unsent(e)) => Err(e),
    }
}

/// The one page element with accessible name `name` and one of `roles`: its backendDOMNodeId. None when there is
/// none or more than one (the caller then uses UIA, which picked its element its own way).
pub fn find(s: &mut Session, name: &str, roles: &[&str]) -> Result<Option<u64>, String> {
    let doc = s.ask("DOM.getDocument", r#"{"depth":0}"#)?;
    let root = objects(&doc).iter().find(|o| get(o, "nodeName") == Some("#document")).and_then(|o| get(o, "nodeId")).map(str::to_string).ok_or("no document node")?;
    let mut ids = Vec::new();
    for r in roles {
        let t = s.ask("Accessibility.queryAXTree", &format!(r#"{{"nodeId":{root},"accessibleName":{},"role":{}}}"#, json_str(name), json_str(r)))?;
        ids.extend(objects(&t).iter().filter(|o| get(o, "ignored") == Some("false")).filter_map(|o| get(o, "backendDOMNodeId")?.parse::<u64>().ok()));
    }
    ids.sort();
    ids.dedup();
    Ok((ids.len() == 1).then(|| ids[0]))
}

/// Runs `function` (a JS function declaration) with `this` bound to the element.
pub fn on_node(s: &mut Session, node: u64, function: &str) -> Result<(), CallErr> {
    let r = s.call("DOM.resolveNode", &format!(r#"{{"backendNodeId":{node}}}"#))?;
    let obj = objects(&r).iter().find_map(|o| get(o, "objectId").map(str::to_string)).ok_or_else(|| CallErr::Unsent("resolveNode gave no objectId".into()))?;
    let t = s.call("Runtime.callFunctionOn", &format!(r#"{{"objectId":{},"functionDeclaration":{}}}"#, json_str(&obj), json_str(function)))?;
    if t.contains("\"exceptionDetails\"") {
        return Err(CallErr::Sent(format!("the page threw: {}", objects(&t).iter().find_map(|o| get(o, "description").map(str::to_string)).unwrap_or_default())));
    }
    Ok(())
}

pub fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Whether a reply's top level has an `error` member (never one nested in its result).
fn top_level_error(t: &str) -> bool {
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for (i, c) in t.char_indices() {
        if in_str {
            match c {
                '\\' if !esc => esc = true,
                '"' if !esc => in_str = false,
                _ => esc = false,
            }
            continue;
        }
        match c {
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            '"' => {
                if depth == 1 && t[i..].starts_with("\"error\"") && t[i + 7..].trim_start().starts_with(':') {
                    return true;
                }
                in_str = true;
            }
            _ => {}
        }
    }
    false
}

fn mask_key() -> [u8; 4] {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0x5a5a5a5a);
    n.to_le_bytes()
}

fn frame(payload: &[u8], key: [u8; 4]) -> Vec<u8> {
    frame_op(0x1, payload, key)
}

/// One final client frame: client frames must be masked (RFC 6455 5.3).
fn frame_op(op: u8, payload: &[u8], key: [u8; 4]) -> Vec<u8> {
    let mut f = vec![0x80 | op];
    let n = payload.len();
    if n < 126 {
        f.push(0x80 | n as u8);
    } else if n <= 0xffff {
        f.push(0x80 | 126);
        f.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        f.push(0x80 | 127);
        f.extend_from_slice(&(n as u64).to_be_bytes());
    }
    f.extend_from_slice(&key);
    f.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
    f
}

/// A server frame (unmasked): (opcode, payload). Fragmented messages are not expected for these small replies.
fn read_frame(s: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut h = [0u8; 2];
    s.read_exact(&mut h)?;
    let mut n = (h[1] & 0x7f) as u64;
    if n == 126 {
        let mut e = [0u8; 2];
        s.read_exact(&mut e)?;
        n = u16::from_be_bytes(e) as u64;
    } else if n == 127 {
        let mut e = [0u8; 8];
        s.read_exact(&mut e)?;
        n = u64::from_be_bytes(e);
    }
    if n > 16 << 20 {
        return Err(std::io::Error::other("frame over 16 MB"));
    }
    let mut key = [0u8; 4];
    if h[1] & 0x80 != 0 {
        s.read_exact(&mut key)?;
    }
    let mut p = vec![0u8; n as usize];
    s.read_exact(&mut p)?;
    if h[1] & 0x80 != 0 {
        p.iter_mut().enumerate().for_each(|(i, b)| *b ^= key[i % 4]);
    }
    Ok((h[0] & 0x0f, p))
}

/// Every JSON object's scalar members (strings, and numbers or bools as their text), at any depth, as (key, value)
/// lists; a nested object is an entry of its own. Enough for /json/list and CDP replies.
fn objects(s: &str) -> Vec<Vec<(String, String)>> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut stack: Vec<Vec<(String, String)>> = Vec::new();
    let mut key: Option<String> = None;
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '{' => stack.push(Vec::new()),
            '}' => {
                if let Some(o) = stack.pop() {
                    out.push(o);
                }
                key = None;
            }
            '"' => {
                let mut v = String::new();
                i += 1;
                while i < c.len() && c[i] != '"' {
                    if c[i] == '\\' && i + 1 < c.len() {
                        i += 1;
                        match c[i] {
                            'n' => v.push('\n'),
                            't' => v.push('\t'),
                            'u' if i + 4 < c.len() => {
                                let h: String = c[i + 1..i + 5].iter().collect();
                                v.push(u32::from_str_radix(&h, 16).ok().and_then(char::from_u32).unwrap_or('?'));
                                i += 4;
                            }
                            x => v.push(x),
                        }
                    } else {
                        v.push(c[i]);
                    }
                    i += 1;
                }
                let mut j = i + 1;
                while j < c.len() && c[j].is_whitespace() {
                    j += 1;
                }
                if j < c.len() && c[j] == ':' {
                    key = Some(v);
                } else if let (Some(k), Some(o)) = (key.take(), stack.last_mut()) {
                    o.push((k, v));
                }
            }
            ',' => key = None,
            c0 if c0.is_ascii_alphanumeric() || c0 == '-' => {
                // A bare scalar (number, true, false, null) as its text.
                let st = i;
                while i + 1 < c.len() && (c[i + 1].is_ascii_alphanumeric() || ".+-".contains(c[i + 1])) {
                    i += 1;
                }
                if let (Some(k), Some(o)) = (key.take(), stack.last_mut()) {
                    o.push((k, c[st..=i].iter().collect()));
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

fn get<'a>(o: &'a [(String, String)], k: &str) -> Option<&'a str> {
    o.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_list_and_frames() {
        let list = r#"[ {
   "description": "", "id": "A1", "title": "Tab {one} \"q\"", "type": "page", "url": "file:///x",
   "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/A1"
}, { "id": "W", "title": "sw", "type": "service_worker", "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/W" } ]"#;
        let o = objects(list);
        assert_eq!(o.len(), 2);
        assert_eq!(get(&o[0], "title"), Some("Tab {one} \"q\""));
        assert_eq!(get(&o[0], "webSocketDebuggerUrl"), Some("ws://127.0.0.1:9222/devtools/page/A1"));
        assert_eq!(get(&o[1], "type"), Some("service_worker"));
        let r = objects(r#"{"id":1,"result":{"frameId":"F","loaderId":"L","errorText":"net::ERR_FILE_NOT_FOUND"}}"#);
        assert!(r.iter().any(|x| get(x, "errorText") == Some("net::ERR_FILE_NOT_FOUND")));
        // queryAXTree: numbers and bools come through as text; the nested role object is its own entry.
        let q = objects(r#"{"id":3,"result":{"nodes":[{"nodeId":"9","ignored":false,"role":{"type":"role","value":"button"},"backendDOMNodeId":42}]}}"#);
        assert!(q.iter().any(|o| get(o, "backendDOMNodeId") == Some("42") && get(o, "ignored") == Some("false")));
        assert!(q.iter().any(|o| get(o, "id") == Some("3")));
        assert_eq!(json_str("a\"b\\c\n"), r#""a\"b\\c\u000a""#);
        assert!(top_level_error(r#"{"id":4,"error":{"code":-32000,"message":"x"}}"#));
        assert!(!top_level_error(r#"{"id":4,"result":{"result":{"type":"string","value":"\"error\":{ in page text"}}}"#));
        assert!(!top_level_error(r#"{"id":4,"result":{"error":{"nested":1}}}"#));
        // A masked frame read back (the reader unmasks) gives the payload; 200 bytes takes the 16-bit length form.
        let p = vec![b'x'; 200];
        let f = frame(&p, [1, 2, 3, 4]);
        assert_eq!((f[0], f[1], f[2], f[3]), (0x81, 0x80 | 126, 0, 200));
        assert_eq!(read_frame(&mut &f[..]).unwrap(), (1, p));
    }
}
