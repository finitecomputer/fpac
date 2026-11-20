//! `fpac-remote`: the gate to the remote (docs/contract.md). The guest
//! speaks plain HTTP/1.1 to one intercepted host and holds no credential:
//! the handler on the other side knows the disk and this life's epoch
//! from what the platform gave it, never from the guest.
//!
//! Three calls: `open` (who am I), a ranged `get`, and `put`, which is
//! put-if-absent with the body's SHA-256. A put that meets an object
//! already at its name answers that object's checksum: ours is our own
//! earlier put whose answer was lost (`Put::Same`); another is another
//! writer (`Put::Conflict`), and the caller stops.

pub mod http;
pub mod limits;

use std::net::TcpStream;
use std::time::Duration;

use sha2::{Digest, Sha256};

pub use http::Timing;
use http::{Conn, HttpError, Request};

/// The header that carries a body's SHA-256, lowercase hex.
pub const SHA256_HEADER: &str = "x-fpac-sha256";
/// `/open`'s answers.
pub const DISK_HEADER: &str = "x-fpac-disk";
pub const EPOCH_HEADER: &str = "x-fpac-epoch";
pub const VOLUME_HEADER: &str = "x-fpac-volume-bytes";
pub const HEAD_HEADER: &str = "x-fpac-head";
/// `POST /head`'s answer: `taken` or `same`.
pub const HEAD_ANSWER_HEADER: &str = "x-fpac-head-answer";

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// The remote is slow, absent or failing: try again later.
    #[error("the remote is unavailable: {0}")]
    Unavailable(String),
    /// The remote refused the call as asked (a name outside this life's
    /// epoch, a body past a limit). A bug or a fence; never retried.
    #[error("the remote refused it ({status}): {detail}")]
    Refused { status: u16, detail: String },
    /// An answer that breaks the contract.
    #[error("the remote broke the contract: {0}")]
    Protocol(String),
    /// Bytes that fail their checksum.
    #[error("corrupt: {0}")]
    Corrupt(String),
}

impl From<HttpError> for RemoteError {
    fn from(e: HttpError) -> RemoteError {
        match e {
            HttpError::Io(_) | HttpError::Closed => RemoteError::Unavailable(e.to_string()),
            HttpError::Malformed(_) | HttpError::TooBig(_) => RemoteError::Protocol(e.to_string()),
            HttpError::Sink(s) => RemoteError::Corrupt(s),
        }
    }
}

/// A put's outcome.
#[derive(Debug, PartialEq, Eq)]
pub enum Put {
    Created,
    /// The same bytes were already there: our earlier put landed.
    Same,
    /// Other bytes are there: another writer. The caller is fenced.
    Conflict { sha256: [u8; 32] },
}

#[derive(Debug, PartialEq, Eq)]
pub struct Open {
    pub disk: String,
    pub epoch: u64,
    pub volume_bytes: u64,
    /// None: a new disk.
    pub head: Option<Head>,
}

/// A checkpoint by its name's parts (`e<epoch>/c<seq>`) and its object's
/// SHA-256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointRef {
    pub epoch: u64,
    pub seq: u64,
    pub sum: [u8; 32],
}

/// The head (docs/design.md, Consistency): which checkpoint a disk opens
/// from, and which life's segments follow it. On the wire it is one line,
/// `<epoch> <checkpoint epoch> <checkpoint seq> <sha256 hex>`, or
/// `<epoch> - - -` for a disk with no checkpoint yet, and ` clean` after
/// it when the life slept cleanly: its checkpoint covers everything it
/// shipped, so the next open replays nothing and asks for no segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    pub epoch: u64,
    pub checkpoint: Option<CheckpointRef>,
    pub clean: bool,
}

impl Head {
    pub fn encode(&self) -> String {
        let line = match self.checkpoint {
            Some(c) => format!("{} {} {} {}", self.epoch, c.epoch, c.seq, hex(&c.sum)),
            None => format!("{} - - -", self.epoch),
        };
        if self.clean {
            line + " clean"
        } else {
            line
        }
    }

    pub fn decode(s: &str) -> Option<Head> {
        let p: Vec<&str> = s.split(' ').collect();
        let (e, ce, cs, sum, clean) = match p.as_slice() {
            [e, ce, cs, sum] => (e, ce, cs, sum, false),
            [e, ce, cs, sum, "clean"] => (e, ce, cs, sum, true),
            _ => return None,
        };
        let epoch: u64 = e.parse().ok().filter(|&e| e > 0)?;
        if *ce == "-" && *cs == "-" && *sum == "-" {
            return Some(Head { epoch, checkpoint: None, clean });
        }
        let c = CheckpointRef { epoch: ce.parse().ok().filter(|&e| e > 0)?, seq: cs.parse().ok().filter(|&s| s > 0)?, sum: unhex32(sum)? };
        Some(Head { epoch, checkpoint: Some(c), clean })
    }
}

/// A head post's answer.
#[derive(Debug, PartialEq, Eq)]
pub enum HeadAnswer {
    Taken,
    /// The same head again: an earlier post whose answer was lost.
    Same,
}

pub fn hex(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(D[(x >> 4) as usize] as char);
        s.push(D[(x & 15) as usize] as char);
    }
    s
}

/// 64 lowercase hex digits as 32 bytes; anything else is refused.
pub fn unhex32(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (i, pair) in b.as_chunks::<2>().0.iter().enumerate() {
        out[i] = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(out)
}

pub fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

/// An object's name, checked: `e<epoch>/<rest>`, where the epoch is a
/// number with no leading zero and at least 1, and the rest starts with a
/// lowercase letter followed by lowercase letters, digits, `.` and `-`.
/// Answers the epoch.
pub fn name_epoch(name: &str) -> Option<u64> {
    if name.len() > limits::NAME_BYTES_MAX {
        return None;
    }
    let (e, rest) = name.strip_prefix('e')?.split_once('/')?;
    let digits_ok = !e.is_empty() && e.len() <= 20 && e.bytes().all(|c| c.is_ascii_digit()) && !e.starts_with('0');
    let r = rest.as_bytes();
    let rest_ok = !r.is_empty() && r[0].is_ascii_lowercase() && r.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'-');
    if digits_ok && rest_ok {
        e.parse().ok()
    } else {
        None
    }
}

/// A client of the remote at `host:port`, keeping one connection open
/// between calls when the remote allows it.
pub struct Remote {
    host: String,
    port: u16,
    conn: Option<Conn<TcpStream>>,
    /// The last call's timing.
    pub timing: Timing,
    /// The last answer's headers (names lowercased), for a caller that
    /// must say which answer it was (a wrong read's request id, say).
    pub last_headers: Vec<(String, String)>,
    /// Connections opened so far (a sniff for keep-alive).
    pub connects: u64,
}

impl Remote {
    pub fn new(host: &str, port: u16) -> Remote {
        Remote { host: host.to_string(), port, conn: None, timing: Timing::default(), last_headers: Vec::new(), connects: 0 }
    }

    fn connect(&mut self) -> Result<Conn<TcpStream>, RemoteError> {
        let s = TcpStream::connect((self.host.as_str(), self.port)).map_err(|e| RemoteError::Unavailable(format!("connect {}:{}: {e}", self.host, self.port)))?;
        let t = Some(Duration::from_millis(limits::IO_MS_MAX));
        s.set_read_timeout(t).and_then(|_| s.set_write_timeout(t)).and_then(|_| s.set_nodelay(true)).map_err(|e| RemoteError::Unavailable(e.to_string()))?;
        self.connects += 1;
        Ok(Conn::new(s))
    }

    /// One exchange. A connection kept from an earlier call may have been
    /// closed by the remote meanwhile: then the call is made once more on
    /// a new one. Every call here may be repeated (a get; a put that
    /// lands twice answers `Same`).
    fn call(&mut self, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8], body_max: u64, sink: &mut http::BodySink<'_>) -> Result<http::Head, RemoteError> {
        let reused = self.conn.is_some();
        let mut conn = match self.conn.take() {
            Some(c) => c,
            None => self.connect()?,
        };
        let host = self.host.clone();
        let req = Request { method, path, host: &host, headers, body };
        let mut wrote = false;
        let mut counting = |status: u16, b: &[u8]| {
            wrote = true;
            sink(status, b)
        };
        let answer = match conn.exchange(&req, body_max, &mut counting) {
            Err(HttpError::Closed | HttpError::Io(_)) if reused && !wrote => {
                conn = self.connect()?;
                conn.exchange(&req, body_max, sink)
            }
            other => other,
        };
        let (head, timing) = answer?;
        self.timing = timing;
        self.last_headers.clone_from(&head.headers);
        if !head.close {
            self.conn = Some(conn);
        }
        Ok(head)
    }

    fn detail(status: u16, body: &[u8]) -> RemoteError {
        let detail = String::from_utf8_lossy(&body[..body.len().min(500)]).into_owned();
        if status >= 500 {
            RemoteError::Unavailable(format!("{status}: {detail}"))
        } else {
            RemoteError::Refused { status, detail }
        }
    }

    pub fn open(&mut self) -> Result<Open, RemoteError> {
        let mut body = Vec::new();
        let h = self.call("GET", "/open", &[], &[], limits::SMALL_BODY_BYTES_MAX as u64, &mut |_, b| {
            body.extend_from_slice(b);
            Ok(())
        })?;
        if h.status != 200 {
            return Err(Remote::detail(h.status, &body));
        }
        let disk = h.header(DISK_HEADER).ok_or_else(|| RemoteError::Protocol("open names no disk".into()))?.to_string();
        let epoch = h.header(EPOCH_HEADER).and_then(|e| e.parse().ok()).filter(|&e| e > 0).ok_or_else(|| RemoteError::Protocol("open names no epoch".into()))?;
        let volume_bytes = h.header(VOLUME_HEADER).and_then(|v| v.parse().ok()).filter(|&v| v > 0).ok_or_else(|| RemoteError::Protocol("open names no volume".into()))?;
        let head = match h.header(HEAD_HEADER) {
            Some("none") => None,
            Some(line) => Some(Head::decode(line).ok_or_else(|| RemoteError::Protocol(format!("open names a malformed head: {line}")))?),
            None => return Err(RemoteError::Protocol("open names no head".into())),
        };
        if head.is_some_and(|h| h.epoch >= epoch) {
            return Err(RemoteError::Protocol("open names a head no older than this life".into()));
        }
        Ok(Open { disk, epoch, volume_bytes, head })
    }

    /// Posts this life's head: taken, or the same again. A head the
    /// platform refuses (another life's, or one going back) is `Refused`.
    pub fn post_head(&mut self, head: &Head) -> Result<HeadAnswer, RemoteError> {
        let line = head.encode();
        let mut answer = Vec::new();
        let h = self.call("POST", "/head", &[("content-type", "text/plain")], line.as_bytes(), limits::SMALL_BODY_BYTES_MAX as u64, &mut |_, b| {
            answer.extend_from_slice(b);
            Ok(())
        })?;
        match (h.status, h.header(HEAD_ANSWER_HEADER)) {
            (200, Some("taken")) => Ok(HeadAnswer::Taken),
            (200, Some("same")) => Ok(HeadAnswer::Same),
            (200, other) => Err(RemoteError::Protocol(format!("a head post answered {other:?}"))),
            (status, _) => Err(Remote::detail(status, &answer)),
        }
    }

    /// Puts `body` at `name` if nothing is there.
    pub fn put(&mut self, name: &str, body: &[u8]) -> Result<Put, RemoteError> {
        assert!(name_epoch(name).is_some(), "a name we make is valid: {name}");
        if body.len() as u64 > limits::OBJECT_BYTES_MAX {
            return Err(RemoteError::Refused { status: 413, detail: format!("{} bytes", body.len()) });
        }
        let sum = sha256(body);
        let sum_hex = hex(&sum);
        let mut answer = Vec::new();
        let h = self.call("PUT", &format!("/o/{name}"), &[(SHA256_HEADER, &sum_hex), ("content-type", "application/octet-stream")], body, limits::SMALL_BODY_BYTES_MAX as u64, &mut |_, b| {
            answer.extend_from_slice(b);
            Ok(())
        })?;
        let stored = h.header(SHA256_HEADER).and_then(unhex32);
        match (h.status, stored) {
            (201, Some(s)) if s == sum => Ok(Put::Created),
            (200, Some(s)) if s == sum => Ok(Put::Same),
            (409, Some(s)) if s != sum => Ok(Put::Conflict { sha256: s }),
            (200 | 201 | 409, _) => Err(RemoteError::Protocol(format!("a put answered {} with checksum {:?}", h.status, stored.map(|s| hex(&s))))),
            (status, _) => Err(Remote::detail(status, &answer)),
        }
    }

    /// Reads `buf.len()` bytes of `name` from `offset`: the bytes read
    /// (fewer only at the object's end), or None when it is absent.
    pub fn get_range(&mut self, name: &str, offset: u64, buf: &mut [u8]) -> Result<Option<usize>, RemoteError> {
        assert!(!buf.is_empty());
        assert!(name_epoch(name).is_some(), "a name we read is valid: {name}");
        let last = offset + buf.len() as u64 - 1;
        let range = format!("bytes={offset}-{last}");
        let mut at = 0usize;
        let mut small = Vec::new();
        // An error's text is kept apart from the object's bytes; it may be
        // longer than the range, so its own bound is the small body's.
        let h = self.call("GET", &format!("/o/{name}"), &[("range", &range)], &[], (buf.len() as u64).max(limits::SMALL_BODY_BYTES_MAX as u64), &mut |status, b| {
            if status != 206 && status != 200 {
                small.extend_from_slice(&b[..b.len().min(500usize.saturating_sub(small.len()))]);
                return Ok(());
            }
            if at + b.len() > buf.len() {
                return Err("more than the range".into());
            }
            buf[at..at + b.len()].copy_from_slice(b);
            at += b.len();
            Ok(())
        })?;
        match h.status {
            206 => Ok(Some(at)),
            // a whole object no larger than the range
            200 if offset == 0 => Ok(Some(at)),
            404 => Ok(None),
            s => Err(Remote::detail(s, &small)),
        }
    }

    /// Streams all of `name` to `sink` and checks it against the checksum
    /// the remote stored with it: its length, or None when it is absent.
    pub fn get(&mut self, name: &str, sink: &mut dyn FnMut(&[u8])) -> Result<Option<u64>, RemoteError> {
        assert!(name_epoch(name).is_some(), "a name we read is valid: {name}");
        let mut hasher = Sha256::new();
        let mut n = 0u64;
        let mut small = Vec::new();
        let h = self.call("GET", &format!("/o/{name}"), &[], &[], limits::OBJECT_BYTES_MAX, &mut |status, b| {
            if status != 200 {
                small.extend_from_slice(&b[..b.len().min(500usize.saturating_sub(small.len()))]);
                return Ok(());
            }
            hasher.update(b);
            n += b.len() as u64;
            sink(b);
            Ok(())
        })?;
        match h.status {
            200 => {
                let stored = h.header(SHA256_HEADER).and_then(unhex32).ok_or_else(|| RemoteError::Protocol("an object with no checksum".into()))?;
                let got: [u8; 32] = hasher.finalize().into();
                if got == stored {
                    Ok(Some(n))
                } else {
                    Err(RemoteError::Corrupt(format!("{name}: {} bytes hash {}, stored {}", n, hex(&got), hex(&stored))))
                }
            }
            404 => Ok(None),
            s => Err(Remote::detail(s, &small)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Goal: names are refused outside their grammar. Method: the valid
    // shapes and one of each invalid kind.
    #[test]
    fn names() {
        assert_eq!(name_epoch("e1/s1"), Some(1));
        assert_eq!(name_epoch("e18446744073709551615/c0.x-y"), Some(u64::MAX));
        for bad in ["e0/s1", "e01/s1", "e/s1", "x1/s1", "e1/", "e1/S1", "e1/1s", "e1/s/1", "e1/s 1", "e99999999999999999999/s1"] {
            assert_eq!(name_epoch(bad), None, "{bad}");
        }
        assert_eq!(name_epoch(&format!("e1/{}", "a".repeat(100))), None);
    }

    // Goal: a head's line round-trips, with and without a checkpoint, and
    // a malformed one is refused. Method: both, then six bad lines.
    #[test]
    fn head_lines() {
        let h = Head { epoch: 7, checkpoint: Some(CheckpointRef { epoch: 6, seq: 2, sum: sha256(b"c") }), clean: false };
        assert_eq!(Head::decode(&h.encode()), Some(h));
        let n = Head { epoch: 1, checkpoint: None, clean: false };
        let c = Head { clean: true, ..h };
        assert!(c.encode().ends_with(" clean"));
        assert_eq!(Head::decode(&c.encode()), Some(c));
        assert_eq!(Head::decode(&format!("{} dirty", h.encode())), None);
        assert_eq!(Head::decode(&n.encode()), Some(n));
        for bad in ["", "7", "0 - - -", "7 6 2", "7 6 0 aa", "7 - 2 -", "x - - -"] {
            assert_eq!(Head::decode(bad), None, "{bad}");
        }
    }

    // Goal: hex round-trips, and anything but 64 lowercase digits is
    // refused. Method: a checksum, then wrong lengths and cases.
    #[test]
    fn hex_round_trip() {
        let s = sha256(b"fpac");
        assert_eq!(unhex32(&hex(&s)), Some(s));
        assert_eq!(unhex32(&hex(&s).to_uppercase()), None);
        assert_eq!(unhex32(&hex(&s)[1..]), None);
    }
}
