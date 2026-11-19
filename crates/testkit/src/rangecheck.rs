//! Ranged reads checked byte for byte, at volume, to pin down the wrong
//! reads seen on Cloudflare (docs/spike.md, Results). One object of known
//! bytes (`noise`) is read by `threads` connections, each its own kept-
//! alive HTTP/1.1 connection, at random ranges; every answer is compared
//! with what was put. A wrong read is recorded whole: what was asked, what
//! the answer's head said, where the bytes first differ, and where in the
//! object the wrong bytes came from.
//!
//! The transport is the caller's (`Dial`): plain TCP to the intercepted
//! host from a guest, TLS to the Worker from outside. Requests carry the
//! handler's mode (`x-fpac-mode`: `stream`, the contract's own path;
//! `tee`, streamed while a copy is hashed against `x-fpac-expect-sha256`;
//! `buffer`, read whole and hashed before it is sent), so a wrong read can
//! be put on R2's side of the handler or on the path after it.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fpac_remote::http::{Conn, Request};
use fpac_remote::{hex, sha256};

/// Bad reads recorded whole; past this only counted.
pub const RECORDS_MAX: usize = 200;
/// Failed calls recorded; past this only counted.
pub const FAILURES_MAX: usize = 20;
/// Connections at once.
pub const THREADS_MAX: usize = 64;
/// Reads in one run.
pub const READS_MAX: usize = 1_000_000;

pub trait ReadWrite: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWrite for T {}

/// The caller's transport: a new connection, the host to name, and any
/// headers a request needs (the driver's signature, from outside).
pub trait Dial: Sync {
    fn dial(&self) -> std::io::Result<Box<dyn ReadWrite>>;
    fn host(&self) -> &str;
    fn headers(&self, method: &str, path: &str) -> Vec<(String, String)>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Stream,
    Tee,
    Buffer,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Stream => "stream",
            Mode::Tee => "tee",
            Mode::Buffer => "buffer",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "stream" => Some(Mode::Stream),
            "tee" => Some(Mode::Tee),
            "buffer" => Some(Mode::Buffer),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// 4 KiB-aligned offsets, sizes of 4 KiB times a power of two up to
    /// `size_max` (the store's own reads).
    Aligned,
    /// Any byte offset, any size from 1 byte to `size_max`.
    Unaligned,
}

impl Pattern {
    pub fn parse(s: &str) -> Option<Pattern> {
        match s {
            "aligned" => Some(Pattern::Aligned),
            "unaligned" => Some(Pattern::Unaligned),
            _ => None,
        }
    }
}

pub struct Check<'a> {
    /// The object's path on the remote (`/o/e1/x`, `/r2/repro/x`).
    pub path: &'a str,
    pub object: &'a [u8],
    pub reads: usize,
    pub threads: usize,
    pub mode: Mode,
    pub pattern: Pattern,
    pub size_max: usize,
    pub seed: u64,
}

/// One wrong read, whole.
#[derive(Debug, Clone)]
pub struct Bad {
    pub at_ms: u64,
    pub offset: u64,
    pub length: u64,
    pub content_range: Option<String>,
    pub content_length: Option<String>,
    pub got: u64,
    pub first_diff: Option<u64>,
    pub differ: u64,
    /// Runs of wrong bytes: (offset in the answer, offset in the object
    /// they match, length), up to four; `None` where no match was found.
    pub runs: Vec<Run>,
    pub all_zero_after: bool,
    /// The answer's identifying headers: request id, version, cf-ray.
    pub ids: Vec<(String, String)>,
    pub thread: usize,
    pub nth: u64,
}

pub struct Outcome {
    pub reads: u64,
    pub right: u64,
    pub wrong: u64,
    pub failed: u64,
    pub bytes: u64,
    pub ms: u64,
    pub bad: Vec<Bad>,
    pub failures: Vec<String>,
    /// First-byte times of right reads, every 16th, ms.
    pub first_byte_sample: Vec<f64>,
}

fn now_ms() -> u64 {
    u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).expect("after 1970").as_millis()).expect("fits")
}

fn next(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

/// The `nth` range of a thread's sequence: (offset, length).
pub fn range(pattern: Pattern, x: &mut u64, object_len: u64, size_max: u64) -> (u64, u64) {
    assert!(size_max >= 4096 && object_len > size_max);
    let r = next(x);
    match pattern {
        Pattern::Aligned => {
            let steps = 64 - (size_max / 4096).leading_zeros() as u64; // sizes 4 KiB << 0..steps
            let len = 4096u64 << (r % steps);
            let len = len.min(size_max);
            let off = (next(x) % ((object_len - len) / 4096)) * 4096;
            (off, len)
        }
        Pattern::Unaligned => {
            let len = 1 + r % size_max;
            let off = next(x) % (object_len - len);
            (off, len)
        }
    }
}

/// A run of wrong bytes: (its offset in the answer, the offset in the
/// object it matches, if any, its length).
pub type Run = (u64, Option<u64>, u64);

/// Where `got` differs from `want`: the first differing offset, how many
/// bytes differ, the runs of wrong bytes and where they came from, and
/// whether everything after the first difference is zero.
pub fn diagnose(got: &[u8], want: &[u8], object: &[u8]) -> (Option<u64>, u64, Vec<Run>, bool) {
    assert_eq!(got.len(), want.len());
    let first = got.iter().zip(want).position(|(a, b)| a != b);
    let differ = got.iter().zip(want).filter(|(a, b)| a != b).count() as u64;
    let mut runs = Vec::new();
    let mut at = first;
    // Bounded: four runs.
    while let Some(start) = at {
        if runs.len() == 4 {
            break;
        }
        let probe = &got[start..(start + 32).min(got.len())];
        let src = if probe.len() >= 8 { object.windows(probe.len()).position(|w| w == probe) } else { None };
        let len = match src {
            Some(s) => got[start..].iter().zip(&object[s..]).take_while(|(a, b)| a == b).count(),
            None => probe.len(),
        };
        runs.push((start as u64, src.map(|s| s as u64), len as u64));
        let resume = start + len.max(1);
        at = got[resume.min(got.len())..].iter().zip(&want[resume.min(want.len())..]).position(|(a, b)| a != b).map(|p| p + resume);
    }
    let zero = first.is_some_and(|f| got[f..].iter().all(|&b| b == 0));
    (first.map(|f| f as u64), differ, runs, zero)
}

/// Runs the check: blocks until every read is done.
pub fn run(check: &Check<'_>, dial: &dyn Dial, progress: &(dyn Fn(u64, u64, u64) + Sync)) -> Outcome {
    assert!(check.threads >= 1 && check.threads <= THREADS_MAX);
    assert!(check.reads <= READS_MAX);
    let t0 = Instant::now();
    let done = AtomicUsize::new(0);
    let (right, wrong, failed, bytes) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    let bad = Mutex::new(Vec::new());
    let failures = Mutex::new(Vec::new());
    let sample = Mutex::new(Vec::new());
    let object_len = check.object.len() as u64;
    std::thread::scope(|s| {
        for t in 0..check.threads {
            let (done, right, wrong, failed, bytes, bad, failures, sample) = (&done, &right, &wrong, &failed, &bytes, &bad, &failures, &sample);
            s.spawn(move || {
                let mut x = check.seed ^ ((t as u64 + 1) * 0x9E37_79B9_7F4A_7C15) | 1;
                let mut conn: Option<Conn<Box<dyn ReadWrite>>> = None;
                let mut buf = vec![0u8; check.size_max];
                // Bounded by `reads`: each pass takes one.
                loop {
                    let nth = done.fetch_add(1, Ordering::SeqCst);
                    if nth >= check.reads {
                        return;
                    }
                    let (off, len) = range(check.pattern, &mut x, object_len, check.size_max as u64);
                    let want = &check.object[off as usize..(off + len) as usize];
                    let range_h = format!("bytes={off}-{}", off + len - 1);
                    let expect = if check.mode == Mode::Stream { String::new() } else { hex(&sha256(want)) };
                    let mut headers: Vec<(String, String)> = dial.headers("GET", check.path);
                    headers.push(("range".into(), range_h));
                    headers.push(("x-fpac-mode".into(), check.mode.name().into()));
                    if !expect.is_empty() {
                        headers.push(("x-fpac-expect-sha256".into(), expect));
                    }
                    let hs: Vec<(&str, &str)> = headers.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
                    if conn.is_none() {
                        match dial.dial() {
                            Ok(s) => conn = Some(Conn::new(s)),
                            Err(e) => {
                                failed.fetch_add(1, Ordering::SeqCst);
                                let mut f = failures.lock().unwrap();
                                if f.len() < FAILURES_MAX {
                                    f.push(format!("dial: {e}"));
                                }
                                std::thread::sleep(Duration::from_millis(500));
                                continue;
                            }
                        }
                    }
                    let c = conn.as_mut().expect("dialed");
                    let mut got = 0usize;
                    let mut over = false;
                    let b = &mut buf[..len as usize];
                    let answer = c.exchange(&Request { method: "GET", path: check.path, host: dial.host(), headers: &hs, body: &[] }, check.size_max as u64 + 65536, &mut |status, chunk| {
                        if status != 206 {
                            return Ok(());
                        }
                        if got + chunk.len() > b.len() {
                            over = true;
                            return Err("more bytes than the range".into());
                        }
                        b[got..got + chunk.len()].copy_from_slice(chunk);
                        got += chunk.len();
                        Ok(())
                    });
                    match answer {
                        Ok((h, timing)) if h.status == 206 && got as u64 == len && b[..] == *want => {
                            right.fetch_add(1, Ordering::SeqCst);
                            bytes.fetch_add(len, Ordering::SeqCst);
                            if nth % 16 == 0 {
                                sample.lock().unwrap().push(timing.first_byte.as_secs_f64() * 1000.0);
                            }
                            if h.close {
                                conn = None;
                            }
                        }
                        Ok((h, _)) if h.status == 206 => {
                            wrong.fetch_add(1, Ordering::SeqCst);
                            let (first_diff, differ, runs, zero) = if got as u64 == len { diagnose(b, want, check.object) } else { (None, 0, Vec::new(), false) };
                            let ids = ["x-fpac-request", "x-fpac-version", "x-fpac-ray", "cf-ray", "server-timing"].iter().filter_map(|n| h.header(n).map(|v| (n.to_string(), v.to_string()))).collect();
                            let record = Bad { at_ms: now_ms(), offset: off, length: len, content_range: h.header("content-range").map(String::from), content_length: h.header("content-length").map(String::from), got: got as u64, first_diff, differ, runs, all_zero_after: zero, ids, thread: t, nth: nth as u64 };
                            let mut v = bad.lock().unwrap();
                            if v.len() < RECORDS_MAX {
                                v.push(record);
                            }
                            if h.close {
                                conn = None;
                            }
                        }
                        Ok((h, _)) => {
                            failed.fetch_add(1, Ordering::SeqCst);
                            let mut f = failures.lock().unwrap();
                            if f.len() < FAILURES_MAX {
                                f.push(format!("{off}+{len}: status {}", h.status));
                            }
                            if h.close {
                                conn = None;
                            }
                        }
                        Err(e) => {
                            // a cut or refused answer is an error, never
                            // bytes: the connection is not reused
                            if over {
                                wrong.fetch_add(1, Ordering::SeqCst);
                            } else {
                                failed.fetch_add(1, Ordering::SeqCst);
                            }
                            let mut f = failures.lock().unwrap();
                            if f.len() < FAILURES_MAX {
                                f.push(format!("{off}+{len}: {e}"));
                            }
                            conn = None;
                        }
                    }
                    let n = nth as u64 + 1;
                    if n.is_multiple_of(5000) {
                        progress(n, wrong.load(Ordering::SeqCst), failed.load(Ordering::SeqCst));
                    }
                }
            });
        }
    });
    Outcome {
        reads: (right.load(Ordering::SeqCst) + wrong.load(Ordering::SeqCst) + failed.load(Ordering::SeqCst)),
        right: right.into_inner(),
        wrong: wrong.into_inner(),
        failed: failed.into_inner(),
        bytes: bytes.into_inner(),
        ms: u64::try_from(t0.elapsed().as_millis()).expect("fits"),
        bad: bad.into_inner().unwrap(),
        failures: failures.into_inner().unwrap(),
        first_byte_sample: sample.into_inner().unwrap(),
    }
}

/// A check's outcome as evidence (one JSON document).
pub fn evidence(o: &Outcome, c: &Check<'_>, key: &str) -> serde_json::Value {
    use serde_json::json;
    let bad: Vec<serde_json::Value> = o
        .bad
        .iter()
        .map(|b| {
            json!({
                "at_ms": b.at_ms, "nth": b.nth, "thread": b.thread, "key": key,
                "range": [b.offset, b.offset + b.length - 1], "length": b.length,
                "content_range": b.content_range, "content_length": b.content_length, "got": b.got,
                "first_diff": b.first_diff, "differ": b.differ, "all_zero_after": b.all_zero_after,
                "runs": b.runs.iter().map(|(at, src, len)| json!({ "at": at, "object_offset": src, "len": len })).collect::<Vec<_>>(),
                "ids": b.ids.iter().map(|(k, v)| (k.clone(), json!(v))).collect::<serde_json::Map<_, _>>(),
            })
        })
        .collect();
    let mut fb = o.first_byte_sample.clone();
    fb.sort_by(|a, b| a.total_cmp(b));
    let at = |q: f64| if fb.is_empty() { 0.0 } else { (fb[((fb.len() as f64 - 1.0) * q).round() as usize] * 10.0).round() / 10.0 };
    json!({
        "evidence": "rangecheck", "mode": c.mode.name(), "pattern": format!("{:?}", c.pattern).to_lowercase(),
        "size_max": c.size_max, "threads": c.threads, "object_bytes": c.object.len(),
        "reads": o.reads, "right": o.right, "wrong": o.wrong, "failed": o.failed, "bytes": o.bytes, "ms": o.ms,
        "bad": bad, "failures": o.failures,
        "first_byte_ms": { "n": fb.len(), "p50": at(0.5), "p99": at(0.99) },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Goal: every range lies inside the object and has the pattern's
    // shape. Method: many draws of each pattern.
    #[test]
    fn ranges_fit_their_pattern() {
        let mut x = 7u64;
        for _ in 0..10_000 {
            let (o, l) = range(Pattern::Aligned, &mut x, 50 << 20, 1 << 20);
            assert!(o % 4096 == 0 && l % 4096 == 0 && l <= 1 << 20 && o + l <= 50 << 20);
            let (o, l) = range(Pattern::Unaligned, &mut x, 50 << 20, 1 << 20);
            assert!((1..=1 << 20).contains(&l) && o + l <= 50 << 20);
        }
    }

    // Goal: a wrong read is described as it was seen on Cloudflare: right
    // for its first 8 KiB, then the object's bytes from 28 KiB on.
    // Method: build exactly that answer and diagnose it.
    #[test]
    fn diagnose_finds_the_wrong_bytes_source() {
        let object = crate::conformance::noise(1 << 20, 5);
        let (off, len) = (100 * 4096, 64 * 1024);
        let want = &object[off..off + len];
        let mut got = want.to_vec();
        got[8192..].copy_from_slice(&object[off + 8192 + 28672..off + len + 28672]);
        let (first, differ, runs, zero) = diagnose(&got, want, &object);
        assert_eq!(first, Some(8192));
        assert!(differ > 50_000);
        assert_eq!(runs[0], (8192, Some((off + 8192 + 28672) as u64), (len - 8192) as u64));
        assert!(!zero);
    }
}
