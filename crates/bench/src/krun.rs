//! The krun place: the sandcastle engine on this box (a microVM per
//! container, its own kernel), driven over its unix socket as celld
//! drives it. The intercepted host is answered by the folder remote on a
//! unix socket: a stand-in for the platform's route and for R2, named so
//! in every result.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fpac_remote::http::{Conn, Request};
use serde_json::{json, Value};

/// Output an exec may return to the driver.
const EXEC_OUTPUT_BYTES_MAX: usize = 16 << 20;

fn socket() -> PathBuf {
    std::env::var_os("FPAC_ENGINE_SOCKET").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/var/lib/sandcastle/engine.sock"))
}

fn connect() -> Result<UnixStream, String> {
    let s = UnixStream::connect(socket()).map_err(|e| format!("the engine's socket: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(600))).map_err(|e| e.to_string())?;
    Ok(s)
}

/// One call to the engine's API.
pub fn call(method: &str, path: &str, body: &[u8], content_type: &str) -> Result<(u16, Value), String> {
    let mut conn = Conn::new(connect()?);
    let mut all = Vec::new();
    let (h, _) = conn
        .exchange(&Request { method, path, host: "engine", headers: &[("content-type", content_type), ("connection", "close")], body }, 64 << 20, &mut |_, b| {
            all.extend_from_slice(b);
            Ok(())
        })
        .map_err(|e| format!("{method} {path}: {e}"))?;
    let v = serde_json::from_slice(&all).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&all).into_owned()));
    Ok((h.status, v))
}

pub fn load_image(reference: &str, tar: &Path) -> Result<Value, String> {
    let body = std::fs::read(tar).map_err(|e| format!("{}: {e}", tar.display()))?;
    let (s, v) = call("POST", &format!("/v1/images/load?reference={}", reference.replace(':', "%3A")), &body, "application/x-tar")?;
    if s / 100 == 2 {
        Ok(v)
    } else {
        Err(format!("load: {s} {v}"))
    }
}

/// Starts `name` from `image`, 2 vCPU, 6 GiB, 20 GB as a computer, no
/// internet, with the remote's host intercepted to `handler`.
pub fn start(name: &str, image: &str, handler: &Path) -> Result<Value, String> {
    let body = json!({
        "image": image,
        "enableInternet": false,
        "instance": { "vcpu": 2, "memoryMib": 6144, "diskMb": 20000 },
        "intercepts": [{ "scheme": "http", "target": "fpac.fragment.internal", "action": { "kind": "handler" } }],
        "handler": format!("unix:{}", handler.display()),
    });
    let (s, v) = call("POST", &format!("/v1/containers/{name}/start?wait=ready"), body.to_string().as_bytes(), "application/json")?;
    if s == 201 {
        Ok(v)
    } else {
        Err(format!("start {name}: {s} {v}"))
    }
}

pub fn destroy(name: &str) -> Result<(), String> {
    let _ = call("POST", &format!("/v1/containers/{name}/destroy"), b"{}", "application/json")?;
    // gone from the list before anything else starts (bounded: 30 s)
    for _ in 0..300 {
        let (_, v) = call("GET", "/v1/containers", b"", "application/json")?;
        if !v.as_array().is_some_and(|a| a.iter().any(|c| c["name"] == name)) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("{name} is still listed 30 s after its destroy"))
}

pub fn list() -> Result<Value, String> {
    call("GET", "/v1/containers", b"", "application/json").map(|(_, v)| v)
}

pub struct Exec {
    pub code: Option<i64>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `argv` in `name` over the engine's exec stream: frames of a kind
/// byte, three reserved, a big-endian length, then the payload (1 stdout,
/// 2 stderr, 3 the exit, 5 an error).
pub fn exec(name: &str, argv: &[&str]) -> Result<Exec, String> {
    let mut s = connect()?;
    let body = json!({ "cmd": argv, "env": { "PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" } }).to_string();
    let req = format!("POST /v1/containers/{name}/exec HTTP/1.1\r\nHost: e\r\nConnection: Upgrade\r\nUpgrade: sandcastle-exec\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 1 << 16];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = s.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("exec: no answer".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let at = buf.windows(4).position(|w| w == b"\r\n\r\n").expect("found");
    let head = String::from_utf8_lossy(&buf[..at]).into_owned();
    if !head.starts_with("HTTP/1.1 101") {
        return Err(format!("exec refused: {head} {}", String::from_utf8_lossy(&buf[at + 4..])));
    }
    let mut buf = buf[at + 4..].to_vec();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    // Bounded by the process's exit, or by its output's limit.
    loop {
        while buf.len() < 8 {
            let n = s.read(&mut chunk).map_err(|e| e.to_string())?;
            if n == 0 {
                return Ok(Exec { code: None, stdout: String::from_utf8_lossy(&out).into_owned(), stderr: String::from_utf8_lossy(&err).into_owned() });
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let len = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        while buf.len() < 8 + len {
            let n = s.read(&mut chunk).map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("exec: a frame cut short".into());
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let kind = buf[0];
        let payload = buf[8..8 + len].to_vec();
        buf.drain(..8 + len);
        match kind {
            1 => out.extend_from_slice(&payload),
            2 => err.extend_from_slice(&payload),
            3 => {
                let code = serde_json::from_slice::<Value>(&payload).ok().and_then(|v| v["code"].as_i64());
                return Ok(Exec { code, stdout: String::from_utf8_lossy(&out).into_owned(), stderr: String::from_utf8_lossy(&err).into_owned() });
            }
            5 => return Err(format!("exec error: {}", String::from_utf8_lossy(&payload))),
            _ => {}
        }
        if out.len() + err.len() > EXEC_OUTPUT_BYTES_MAX {
            return Err("exec: output past its limit".into());
        }
    }
}
