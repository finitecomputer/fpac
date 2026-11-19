//! A remote that answers the contract (docs/contract.md) from a folder:
//! `<root>/<disk>/e<epoch>/<name>`. It stands in for the platform's route,
//! on this box and in tests; it is never the product. It holds one disk
//! and one epoch, as a handler is given by the platform, and it fails on
//! request (`Faults`).
//!
//! HTTP/1.1, a thread per connection (at most `CONNECTIONS_MAX`), over TCP
//! or a unix socket (the krun engine hands intercepted requests to one).

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use fpac_remote::{hex, limits, name_epoch, sha256, unhex32, DISK_HEADER, EPOCH_HEADER, SHA256_HEADER};

/// Connections served at once; past it a connection is closed at once.
pub const CONNECTIONS_MAX: usize = 64;
/// Requests on one kept-alive connection.
pub const REQUESTS_PER_CONNECTION_MAX: usize = 100_000;

/// Faults to inject, each counting down to zero.
#[derive(Default)]
pub struct Faults {
    /// Do the next N puts, then close the connection without answering:
    /// a lost answer.
    pub lose_put_answer: AtomicU32,
    /// Answer the next N calls 503 without doing them.
    pub unavailable: AtomicU32,
    /// Send the next N ranged gets' heads and half their bodies, then
    /// close: an answer cut short.
    pub cut_get_body: AtomicU32,
}

fn take(c: &AtomicU32) -> bool {
    c.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok()
}

pub struct Folder {
    pub root: PathBuf,
    pub disk: String,
    pub epoch: u64,
    pub faults: Faults,
    live: AtomicUsize,
}

pub enum Listen {
    Tcp(TcpListener),
    Unix(UnixListener),
}

/// A running folder remote. Its threads end with the process.
pub struct Running {
    pub folder: Arc<Folder>,
    /// `127.0.0.1:<port>` for TCP.
    pub port: Option<u16>,
}

impl Folder {
    pub fn new(root: &Path, disk: &str, epoch: u64) -> Folder {
        assert!(epoch > 0);
        assert!(!disk.is_empty() && disk.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'));
        Folder { root: root.to_path_buf(), disk: disk.to_string(), epoch, faults: Faults::default(), live: AtomicUsize::new(0) }
    }

    pub fn start(self, listen: Listen) -> Running {
        let folder = Arc::new(self);
        let port = match &listen {
            Listen::Tcp(l) => Some(l.local_addr().expect("bound").port()),
            Listen::Unix(_) => None,
        };
        let f = folder.clone();
        std::thread::spawn(move || match listen {
            // Unbounded on purpose: it accepts until the process ends, and
            // each connection is bounded by CONNECTIONS_MAX.
            Listen::Tcp(l) => {
                for s in l.incoming().flatten() {
                    let _ = s.set_nodelay(true);
                    f.clone().connection(s);
                }
            }
            Listen::Unix(l) => {
                for s in l.incoming().flatten() {
                    f.clone().connection(s);
                }
            }
        });
        Running { folder, port }
    }

    fn connection<S: Read + Write + Send + 'static>(self: Arc<Self>, s: S) {
        if self.live.fetch_add(1, Ordering::SeqCst) >= CONNECTIONS_MAX {
            self.live.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        std::thread::spawn(move || {
            let mut s = s;
            for _ in 0..REQUESTS_PER_CONNECTION_MAX {
                match self.one(&mut s) {
                    Ok(true) => continue,
                    _ => break,
                }
            }
            self.live.fetch_sub(1, Ordering::SeqCst);
        });
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(&self.disk).join(name)
    }

    /// Serves one request: whether the connection stays open.
    fn one<S: Read + Write>(&self, s: &mut S) -> std::io::Result<bool> {
        let Some(req) = read_request(s)? else { return Ok(false) };
        if take(&self.faults.unavailable) {
            answer(s, 503, &[], b"unavailable (a fault asked for)")?;
            return Ok(true);
        }
        let route = req.path.strip_prefix("/o/");
        match (req.method.as_str(), req.path.as_str(), route) {
            ("GET", "/open", _) => {
                answer(s, 200, &[(DISK_HEADER, self.disk.clone()), (EPOCH_HEADER, self.epoch.to_string())], b"")?;
            }
            ("GET", _, Some(name)) if name_epoch(name).is_some() => self.get(s, name, req.header("range"))?,
            ("PUT", _, Some(name)) => match name_epoch(name) {
                Some(e) if e == self.epoch => {
                    if !self.put(s, name, &req)? {
                        return Ok(false);
                    }
                }
                Some(_) => answer(s, 403, &[], b"a put names another life's epoch")?,
                None => answer(s, 400, &[], b"a name outside the grammar")?,
            },
            (_, _, Some(_)) => answer(s, 400, &[], b"a name outside the grammar")?,
            _ => answer(s, 404, &[], b"no such route")?,
        }
        Ok(true)
    }

    fn get<S: Write>(&self, s: &mut S, name: &str, range: Option<&str>) -> std::io::Result<()> {
        let Ok(mut f) = std::fs::File::open(self.path(name)) else {
            return answer(s, 404, &[], b"absent");
        };
        let len = f.metadata()?.len();
        match range {
            None => {
                let mut bytes = Vec::with_capacity(len as usize);
                f.read_to_end(&mut bytes)?;
                answer(s, 200, &[(SHA256_HEADER, hex(&sha256(&bytes)))], &bytes)
            }
            Some(r) => match parse_range(r) {
                Some((a, _)) if a >= len => answer(s, 416, &[], b"the range starts past the end"),
                Some((a, b)) => {
                    // only the range is read, as a store would
                    let end = b.min(len - 1);
                    let mut part = vec![0u8; (end - a + 1) as usize];
                    std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(a))?;
                    f.read_exact(&mut part)?;
                    if take(&self.faults.cut_get_body) {
                        let h = format!("HTTP/1.1 206 X\r\ncontent-length: {}\r\ncontent-range: bytes {a}-{end}/{len}\r\n\r\n", part.len());
                        s.write_all(h.as_bytes())?;
                        s.write_all(&part[..part.len() / 2])?;
                        s.flush()?;
                        return Err(std::io::Error::other("cut on purpose"));
                    }
                    answer(s, 206, &[("content-range", format!("bytes {a}-{end}/{len}"))], &part)
                }
                None => answer(s, 400, &[], b"a range we do not serve"),
            },
        }
    }

    /// Answers a put: false when the answer was dropped on purpose.
    fn put<S: Write>(&self, s: &mut S, name: &str, req: &Req) -> std::io::Result<bool> {
        let Some(claimed) = req.header(SHA256_HEADER).and_then(unhex32) else {
            answer(s, 400, &[], b"a put names its SHA-256")?;
            return Ok(true);
        };
        let sum = sha256(&req.body);
        if sum != claimed {
            answer(s, 400, &[], b"the body does not match its SHA-256")?;
            return Ok(true);
        }
        let path = self.path(name);
        std::fs::create_dir_all(path.parent().expect("a parent"))?;
        // Write aside, then link into place: a link fails if the name is
        // taken, so a put never replaces an object (put-if-absent).
        let aside = path.with_extension(format!("tmp-{}-{:?}", std::process::id(), std::thread::current().id()).replace(['(', ')'], ""));
        std::fs::write(&aside, &req.body)?;
        std::fs::File::open(&aside)?.sync_all()?;
        let linked = std::fs::hard_link(&aside, &path);
        std::fs::remove_file(&aside)?;
        let (status, stored) = match linked {
            Ok(()) => (201, sum),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stored = sha256(&std::fs::read(&path)?);
                (if stored == sum { 200 } else { 409 }, stored)
            }
            Err(e) => return Err(e),
        };
        if take(&self.faults.lose_put_answer) {
            return Ok(false);
        }
        answer(s, status, &[(SHA256_HEADER, hex(&stored))], b"")?;
        Ok(true)
    }
}

/// `bytes=a-b` only, the one form the contract's client sends.
fn parse_range(r: &str) -> Option<(u64, u64)> {
    let (a, b) = r.strip_prefix("bytes=")?.split_once('-')?;
    let (a, b): (u64, u64) = (a.parse().ok()?, b.parse().ok()?);
    if a <= b {
        Some((a, b))
    } else {
        None
    }
}

struct Req {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, n: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str())
    }
}

/// Reads one request; None when the peer closed between requests. Reads
/// a byte at a time up to the head's end (a stand-in, not a fast path),
/// then the body by its length.
fn read_request<S: Read>(s: &mut S) -> std::io::Result<Option<Req>> {
    let mut head = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if s.read(&mut byte)? == 0 {
            return Ok(None);
        }
        head.push(byte[0]);
        if head.len() > limits::HEAD_BYTES_MAX {
            return Err(std::io::Error::other("a head past its limit"));
        }
    }
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, path) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    let len: u64 = headers.iter().find(|(n, _)| n == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    if len > limits::OBJECT_BYTES_MAX {
        return Err(std::io::Error::other("a body past its limit"));
    }
    let mut body = vec![0u8; len as usize];
    s.read_exact(&mut body)?;
    Ok(Some(Req { method, path, headers, body }))
}

fn answer<S: Write>(s: &mut S, status: u16, headers: &[(&str, String)], body: &[u8]) -> std::io::Result<()> {
    let mut h = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\n", body.len());
    for (n, v) in headers {
        h.push_str(&format!("{n}: {v}\r\n"));
    }
    h.push_str("\r\n");
    s.write_all(h.as_bytes())?;
    s.write_all(body)?;
    s.flush()
}
