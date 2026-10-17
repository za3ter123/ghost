//! Wire format shared by client and daemon (same as the C# v2, so either side can talk to the other):
//! request  = u32 count, then count x (u32 len, utf-8 bytes): session, cwd, args...
//! reply    = i32 exit code, u32 len + stdout bytes, u32 len + stderr bytes. All little-endian.
use std::io::{self, Read};

/// Pipe name stem (the C# v2 used the same name; its ghostd.exe is stopped by the client on first contact).
pub const PIPE_STEM: &str = "ghost";

pub fn pipe_name() -> String {
    format!(r"\\.\pipe\{}-{}", PIPE_STEM, user())
}

pub fn user() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "user".into())
}

pub fn encode(parts: &[String]) -> Vec<u8> {
    let mut v = Vec::with_capacity(64);
    v.extend_from_slice(&(parts.len() as u32).to_le_bytes());
    for p in parts {
        v.extend_from_slice(&(p.len() as u32).to_le_bytes());
        v.extend_from_slice(p.as_bytes());
    }
    v
}

fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_blob<R: Read>(r: &mut R, max: usize) -> io::Result<Vec<u8>> {
    let n = read_u32(r)? as usize;
    if n > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let mut b = vec![0u8; n];
    r.read_exact(&mut b)?;
    Ok(b)
}

/// Request from an untrusted-size stream: bounded count and lengths.
pub fn read_request<R: Read>(r: &mut R) -> io::Result<Vec<String>> {
    let n = read_u32(r)? as usize;
    if !(2..=4096).contains(&n) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad part count"));
    }
    (0..n).map(|_| read_blob(r, 1 << 20).map(|b| String::from_utf8_lossy(&b).into_owned())).collect()
}

pub fn encode_reply(code: i32, out: &str, err: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(12 + out.len() + err.len());
    v.extend_from_slice(&code.to_le_bytes());
    v.extend_from_slice(&(out.len() as u32).to_le_bytes());
    v.extend_from_slice(out.as_bytes());
    v.extend_from_slice(&(err.len() as u32).to_le_bytes());
    v.extend_from_slice(err.as_bytes());
    v
}

pub fn read_reply<R: Read>(r: &mut R) -> io::Result<(i32, Vec<u8>, Vec<u8>)> {
    let code = read_u32(r)? as i32;
    let out = read_blob(r, 64 << 20)?;
    let err = read_blob(r, 64 << 20)?;
    Ok((code, out, err))
}
