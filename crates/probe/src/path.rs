//! The path to the remote (docs/spike.md, Acceptance 2), measured through
//! the product's own client: put-if-absent's three cases; puts of 64 KiB
//! and of 8 MiB; a 50 MiB object streamed whole, one stream and four in
//! parallel; ranged gets of 64 KiB (a miss's extent) and of 1 MiB.
//! Everything it writes is under this life's epoch with names starting
//! `<tag>-`, which the driver deletes afterwards.

use std::time::Instant;

use std::net::TcpStream;

use fpac_remote::http::{Conn, Request};
use fpac_remote::{Put, Remote};
use fpac_testkit::conformance::noise;
use serde_json::{json, Value};

use crate::{ms, stats};

const SMALL_PUTS: usize = 30;
const BIG_PUTS: usize = 10;
const STREAMS: usize = 5;
const PARALLEL_RUNS: usize = 3;
const PARALLEL: usize = 4;
const RANGED_64K: usize = 200;
const RANGED_1M: usize = 100;
const STREAM_BYTES: usize = 50 << 20;
const NULLS: usize = 100;

/// The stand-in Worker's `/null` (outside the contract): the intercept's
/// round trip with no store behind it, 64 KiB answered from memory, on one
/// kept-alive connection. Its first-byte times and where the handler ran;
/// empty when the remote has no such route (the folder remote).
fn nulls(host: &str, port: u16) -> Value {
    let Ok(s) = TcpStream::connect((host, port)) else { return json!(null) };
    let mut c = Conn::new(s);
    let (mut first, mut colo) = (Vec::new(), None);
    for _ in 0..NULLS {
        let mut n = 0usize;
        let req = Request { method: "GET", path: "/null?bytes=65536", host, headers: &[], body: &[] };
        match c.exchange(&req, 1 << 20, &mut |_, b| {
            n += b.len();
            Ok(())
        }) {
            Ok((h, t)) if h.status == 200 && n == 65536 => {
                first.push(ms(t.first_byte));
                colo = h.header("x-fpac-colo").map(String::from);
                if h.close {
                    return json!({ "first_byte_ms": stats(first), "handler_colo": colo, "note": "the remote closed the connection" });
                }
            }
            _ => return json!({ "first_byte_ms": stats(first), "handler_colo": colo, "note": "no /null on this remote" }),
        }
    }
    json!({ "first_byte_ms": stats(first), "handler_colo": colo })
}

fn mbs(bytes: usize, millis: f64) -> f64 {
    (bytes as f64 / 1e6 / (millis / 1000.0) * 10.0).round() / 10.0
}

/// A ranged read whose bytes differ from what was put: where they first
/// differ, how many differ, and whether the bytes read are the object's
/// at some other offset (a shifted or stale answer) or all zeros.
pub fn mismatch(off: u64, got: &[u8], object: &[u8]) -> String {
    let want = &object[off as usize..off as usize + got.len()];
    let first = got.iter().zip(want).position(|(a, b)| a != b).unwrap_or(0);
    let differ = got.iter().zip(want).filter(|(a, b)| a != b).count();
    let probe = &got[first..(first + 64).min(got.len())];
    let elsewhere = object.windows(probe.len()).position(|w| w == probe);
    let zeros = got[first..].iter().all(|&b| b == 0);
    format!("ranged {off}: {} bytes, wrong bytes from {first} on ({differ} differ); the first wrong 64 are the object's at {elsewhere:?}; all zeros after: {zeros}", got.len())
}

pub fn run(host: &str, port: u16, tag: &str) -> Value {
    let mut r = Remote::new(host, port);
    let t = Instant::now();
    let open = match r.open() {
        Ok(o) => o,
        Err(e) => return json!({ "evidence": "path", "error": format!("open: {e}") }),
    };
    let open_ms = ms(t.elapsed());
    let e = open.epoch;
    let mut errors: Vec<String> = Vec::new();
    // put-if-absent's three cases
    let a = noise(64 << 10, 11);
    let b = noise(64 << 10, 12);
    let cases = json!({
        "created": format!("{:?}", r.put(&format!("e{e}/{tag}-pia"), &a)),
        "same": format!("{:?}", r.put(&format!("e{e}/{tag}-pia"), &a)),
        "conflict": format!("{:?}", r.put(&format!("e{e}/{tag}-pia"), &b)),
    });
    let mut put = |label: &str, n: usize, bytes: usize, seed: u64| -> (Vec<f64>, Vec<f64>) {
        let (mut v, mut server) = (Vec::new(), Vec::new());
        for i in 0..n {
            let body = noise(bytes, seed + i as u64);
            let t = Instant::now();
            match r.put(&format!("e{e}/{tag}-{label}-{i}"), &body) {
                Ok(Put::Created) => {
                    v.push(ms(t.elapsed()));
                    server.extend(r.timing.server_ms);
                }
                other => errors.push(format!("{label} {i}: {other:?}")),
            }
        }
        (v, server)
    };
    let (small, small_server) = put("p64k", SMALL_PUTS, 64 << 10, 100);
    let (big, big_server) = put("p8m", BIG_PUTS, 8 << 20, 200);
    let stream_body = noise(STREAM_BYTES, 300);
    let stream_name = format!("e{e}/{tag}-stream");
    let t = Instant::now();
    let stream_put = r.put(&stream_name, &stream_body);
    let stream_put_ms = ms(t.elapsed());
    if !matches!(stream_put, Ok(Put::Created)) {
        errors.push(format!("the stream's object: {stream_put:?}"));
    }
    let mut streams = Vec::new();
    for _ in 0..STREAMS {
        let t = Instant::now();
        let mut first: Option<f64> = None;
        let mut n = 0usize;
        match r.get(&stream_name, &mut |b| {
            first.get_or_insert_with(|| ms(t.elapsed()));
            n += b.len();
        }) {
            Ok(Some(_)) => {
                let total = ms(t.elapsed());
                streams.push(json!({ "first_byte_ms": first, "ms": total, "mbs": mbs(n, total) }));
            }
            other => errors.push(format!("stream: {other:?}")),
        }
    }
    let mut parallel = Vec::new();
    for _ in 0..PARALLEL_RUNS {
        let t = Instant::now();
        let part = STREAM_BYTES / PARALLEL;
        let handles: Vec<_> = (0..PARALLEL)
            .map(|k| {
                let (host, name) = (host.to_string(), stream_name.clone());
                std::thread::spawn(move || {
                    let mut rr = Remote::new(&host, port);
                    let mut buf = vec![0u8; part];
                    rr.get_range(&name, (k * part) as u64, &mut buf).map(|n| n == Some(part))
                })
            })
            .collect();
        let all = handles.into_iter().all(|h| matches!(h.join(), Ok(Ok(true))));
        let total = ms(t.elapsed());
        parallel.push(json!({ "ok": all, "ms": total, "mbs": mbs(STREAM_BYTES, total) }));
    }
    let mut ranged = |count: usize, bytes: usize, seed: u64| -> (Vec<f64>, Vec<f64>, Vec<f64>) {
        let (mut first, mut total, mut server) = (Vec::new(), Vec::new(), Vec::new());
        let mut buf = vec![0u8; bytes];
        let extents = (STREAM_BYTES / bytes) as u64;
        let mut x = seed;
        for _ in 0..count {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let off = (x % extents) * bytes as u64;
            let t = Instant::now();
            match r.get_range(&stream_name, off, &mut buf) {
                Ok(Some(n)) if n == bytes && buf[..] == stream_body[off as usize..off as usize + bytes] => {
                    total.push(ms(t.elapsed()));
                    first.push(ms(r.timing.first_byte));
                    server.extend(r.timing.server_ms);
                }
                Ok(Some(n)) if n == bytes => errors.push(mismatch(off, &buf, &stream_body)),
                other => errors.push(format!("ranged {off}: {other:?}")),
            }
        }
        (first, total, server)
    };
    let (r64_first, r64_total, r64_server) = ranged(RANGED_64K, 64 << 10, 0x1234_5678);
    let (r1m_first, r1m_total, _) = ranged(RANGED_1M, 1 << 20, 0x8765_4321);
    let intercept_only = nulls(host, port);
    let big_mbs: Vec<f64> = big.iter().map(|&m| mbs(8 << 20, m)).collect();
    json!({
        "evidence": "path",
        "remote": format!("{host}:{port}"),
        "disk": open.disk, "epoch": e, "open_ms": open_ms,
        "put_if_absent": cases,
        "put_64k_ms": stats(small),
        "put_64k_store_ms": stats(small_server),
        "put_8m_ms": stats(big),
        "put_8m_store_ms": stats(big_server),
        "put_8m_mbs": stats(big_mbs),
        "put_50m": { "ms": stream_put_ms, "mbs": mbs(STREAM_BYTES, stream_put_ms) },
        "stream_50m": streams,
        "stream_50m_parallel4": parallel,
        "ranged_64k_first_byte_ms": stats(r64_first),
        "ranged_64k_ms": stats(r64_total),
        "ranged_64k_store_ms": stats(r64_server),
        "intercept_null_64k": intercept_only,
        "ranged_1m_first_byte_ms": stats(r1m_first),
        "ranged_1m_ms": stats(r1m_total),
        "connections_opened": r.connects,
        "errors": errors,
    })
}

/// Ranged reads checked byte for byte (docs/spike.md, Results: wrong
/// reads): a 50 MiB object put once, then `count` reads of a random 4
/// KiB-aligned offset and a random size from 4 KiB to 1 MiB, each compared
/// with what was put, and a whole get checked by its SHA-256 at the start
/// and the end. `diagnose`: the reads ask the stand-in Worker to read each
/// range whole and name the SHA-256 of what it sent (outside the
/// contract), so a wrong read says where it went wrong: R2's side of the
/// handler (what was sent is already wrong) or the path to the guest
/// (what was sent is right, what arrived is not).
pub fn ranges(host: &str, port: u16, tag: &str, count: usize, diagnose: bool) -> Value {
    let mut r = Remote::new(host, port);
    let e = match r.open() {
        Ok(o) => o.epoch,
        Err(err) => return json!({ "evidence": "ranges", "error": format!("open: {err}") }),
    };
    let object = noise(STREAM_BYTES, 400);
    let name = format!("e{e}/{tag}-ranges");
    if !matches!(r.put(&name, &object), Ok(Put::Created)) {
        return json!({ "evidence": "ranges", "error": "the object's put" });
    }
    let whole = |r: &mut Remote| matches!(r.get(&name, &mut |_| {}), Ok(Some(n)) if n == STREAM_BYTES as u64);
    let whole_before = whole(&mut r);
    let mut raw = match TcpStream::connect((host, port)) {
        Ok(s) => Conn::new(s),
        Err(err) => return json!({ "evidence": "ranges", "error": format!("connect: {err}") }),
    };
    let mut buf = vec![0u8; 1 << 20];
    let (mut ok, mut wrong, mut failed) = (0usize, Vec::new(), Vec::new());
    let (mut first, mut bytes_read) = (Vec::new(), 0u64);
    let mut x = 0x0bad_5eed_u64 ^ u64::from(diagnose);
    for _ in 0..count {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let size = (((x >> 20) % 256) as usize + 1) * 4096;
        let off = (x % ((STREAM_BYTES - size) as u64 / 4096)) * 4096;
        let want = &object[off as usize..off as usize + size];
        let b = &mut buf[..size];
        if diagnose {
            let range = format!("bytes={off}-{}", off + size as u64 - 1);
            let path = format!("/o/{name}");
            let mut at = 0usize;
            let req = Request { method: "GET", path: &path, host, headers: &[("range", &range), ("x-fpac-diagnose", "sha256")], body: &[] };
            match raw.exchange(&req, size as u64, &mut |_, chunk| {
                if at + chunk.len() > size {
                    return Err("more than the range".into());
                }
                b[at..at + chunk.len()].copy_from_slice(chunk);
                at += chunk.len();
                Ok(())
            }) {
                Ok((h, t)) if h.status == 206 && at == size => {
                    let sent = h.header("x-fpac-sent-sha256").unwrap_or("").to_string();
                    let (sent_right, got_right) = (sent == fpac_remote::hex(&fpac_remote::sha256(want)), b[..] == *want);
                    if got_right {
                        ok += 1;
                        bytes_read += size as u64;
                        first.push(ms(t.first_byte));
                    } else {
                        let arrived_as_sent = sent == fpac_remote::hex(&fpac_remote::sha256(b));
                        wrong.push(format!("{}; what the handler sent was {}; what arrived was {} it sent", mismatch(off, b, &object), if sent_right { "right" } else { "already wrong" }, if arrived_as_sent { "what" } else { "not what" }));
                    }
                    if !sent_right && got_right {
                        let named = if sent.is_empty() { "none".to_string() } else { sent[..16.min(sent.len())].to_string() };
                        let names: Vec<&str> = h.headers.iter().map(|(n, _)| n.as_str()).collect();
                        failed.push(format!("{off}+{size}: arrived right, but the handler named checksum {named}…; the answer's headers: {names:?}"));
                    }
                    if h.close {
                        match TcpStream::connect((host, port)) {
                            Ok(s) => raw = Conn::new(s),
                            Err(err) => return json!({ "evidence": "ranges", "error": format!("reconnect: {err}") }),
                        }
                    }
                }
                Ok((h, _)) => failed.push(format!("{off}+{size}: {} with {at} bytes", h.status)),
                Err(err) => {
                    failed.push(format!("{off}+{size}: {err}"));
                    match TcpStream::connect((host, port)) {
                        Ok(s) => raw = Conn::new(s),
                        Err(err) => return json!({ "evidence": "ranges", "error": format!("reconnect: {err}") }),
                    }
                }
            }
            continue;
        }
        match r.get_range(&name, off, b) {
            Ok(Some(n)) if n == size && b[..] == *want => {
                ok += 1;
                bytes_read += size as u64;
                first.push(ms(r.timing.first_byte));
            }
            Ok(Some(n)) if n == size => wrong.push(mismatch(off, b, &object)),
            other => failed.push(format!("{off}+{size}: {other:?}")),
        }
    }
    let whole_after = whole(&mut r);
    json!({
        "evidence": "ranges", "remote": format!("{host}:{port}"), "object_bytes": STREAM_BYTES,
        "mode": if diagnose { "diagnose: each range read whole by the handler and sent with its SHA-256" } else { "streamed: R2's body streamed through the handler, as the contract serves it" },
        "reads": count, "right": ok, "wrong": wrong.len(), "failed": failed.len(), "bytes_read": bytes_read,
        "wrong_detail": wrong, "failed_detail": failed.into_iter().take(20).collect::<Vec<_>>(),
        "whole_get_checked": { "before": whole_before, "after": whole_after },
        "first_byte_ms": stats(first), "connections_opened": r.connects,
    })
}

/// The volume range check (docs/spike.md, Results: wrong reads), from the
/// guest through the intercept: see `fpac_testkit::rangecheck`. Puts one
/// 50 MiB object of known bytes, reads it `--count` times on `--threads`
/// connections, and writes one JSON document to `--out` (or stdout), with
/// progress on stderr.
pub fn rangecheck(host: &str, port: u16, tag: &str, args: &[String]) -> Value {
    use fpac_testkit::rangecheck::{self, Check, Dial, Mode, Pattern, ReadWrite};
    struct Tcp<'a>(&'a str, u16);
    impl Dial for Tcp<'_> {
        fn dial(&self) -> std::io::Result<Box<dyn ReadWrite>> {
            let s = TcpStream::connect((self.0, self.1))?;
            s.set_read_timeout(Some(std::time::Duration::from_secs(60)))?;
            s.set_nodelay(true)?;
            Ok(Box::new(s))
        }
        fn host(&self) -> &str {
            self.0
        }
        fn headers(&self, _: &str, _: &str) -> Vec<(String, String)> {
            Vec::new()
        }
    }
    let get = |n: &str| crate::arg(args, n);
    let (Some(mode), Some(pattern)) = (get("--mode").as_deref().and_then(Mode::parse), get("--pattern").as_deref().and_then(Pattern::parse)) else {
        return json!({ "evidence": "rangecheck", "error": "--mode stream|tee|buffer and --pattern aligned|unaligned" });
    };
    let reads: usize = get("--count").and_then(|c| c.parse().ok()).unwrap_or(10_000).min(rangecheck::READS_MAX);
    let threads: usize = get("--threads").and_then(|c| c.parse().ok()).unwrap_or(16).clamp(1, rangecheck::THREADS_MAX);
    let size_max: usize = get("--size-max").and_then(|c| c.parse().ok()).unwrap_or(1 << 20).clamp(4096, 1 << 20);
    let mut r = Remote::new(host, port);
    let e = match r.open() {
        Ok(o) => o.epoch,
        Err(err) => return json!({ "evidence": "rangecheck", "error": format!("open: {err}") }),
    };
    let object = noise(STREAM_BYTES, 500);
    let name = format!("e{e}/{tag}-rc");
    if !matches!(r.put(&name, &object), Ok(Put::Created)) {
        return json!({ "evidence": "rangecheck", "error": "the object's put" });
    }
    let path = format!("/o/{name}");
    let check = Check { path: &path, object: &object, reads, threads, mode, pattern, size_max, seed: 0x5eed_0001 };
    let outcome = rangecheck::run(&check, &Tcp(host, port), &|n, wrong, failed| eprintln!("{n} reads, {wrong} wrong, {failed} failed"));
    let whole_after = matches!(r.get(&name, &mut |_| {}), Ok(Some(n)) if n == STREAM_BYTES as u64);
    let mut out = rangecheck::evidence(&outcome, &check, &name);
    out["whole_get_after"] = json!(whole_after);
    if let Some(file) = get("--out") {
        let _ = std::fs::write(&file, out.to_string());
        let _ = std::fs::write(format!("{file}.done"), "done");
    }
    out
}


/// The wrong reads seen first came through the product's own client
/// (`Remote`), on one kept-alive connection that had carried whole 50 MiB
/// gets and puts. This check repeats that pattern at volume: `--threads`
/// connections, each its own `Remote`, each putting nothing and reading
/// the object whole every `--whole-every` reads, and between those, ranged
/// reads (4 KiB-aligned, 4 KiB to 1 MiB) through `Remote::get_range`, each
/// compared with what was put.
pub fn remotecheck(host: &str, port: u16, tag: &str, args: &[String]) -> Value {
    use fpac_testkit::rangecheck::{diagnose, range, Pattern};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    let get = |n: &str| crate::arg(args, n);
    let reads: u64 = get("--count").and_then(|c| c.parse().ok()).unwrap_or(10_000).min(1_000_000);
    let threads: u64 = get("--threads").and_then(|c| c.parse().ok()).unwrap_or(16).clamp(1, 64);
    let whole_every: u64 = get("--whole-every").and_then(|c| c.parse().ok()).unwrap_or(200).max(1);
    let mut r = Remote::new(host, port);
    let e = match r.open() {
        Ok(o) => o.epoch,
        Err(err) => return json!({ "evidence": "remotecheck", "error": format!("open: {err}") }),
    };
    let object = noise(STREAM_BYTES, 600);
    let name = format!("e{e}/{tag}-remote");
    if !matches!(r.put(&name, &object), Ok(Put::Created)) {
        return json!({ "evidence": "remotecheck", "error": "the object's put" });
    }
    let (done, right, wrong, failed, wholes, whole_bad) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    let bad = Mutex::new(Vec::new());
    let failures = Mutex::new(Vec::new());
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for t in 0..threads {
            let (done, right, wrong, failed, wholes, whole_bad, bad, failures, object, name) = (&done, &right, &wrong, &failed, &wholes, &whole_bad, &bad, &failures, &object, &name);
            s.spawn(move || {
                let mut c = Remote::new(host, port);
                let mut x = (0x5eed_0003u64 ^ (t + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)) | 1;
                let mut buf = vec![0u8; 1 << 20];
                let mut mine = 0u64;
                // Bounded by `reads`: each pass takes one.
                loop {
                    let nth = done.fetch_add(1, Ordering::SeqCst);
                    if nth >= reads {
                        return;
                    }
                    if mine.is_multiple_of(whole_every) {
                        wholes.fetch_add(1, Ordering::SeqCst);
                        if !matches!(c.get(name, &mut |_| {}), Ok(Some(n)) if n == STREAM_BYTES as u64) {
                            whole_bad.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    mine += 1;
                    let (off, len) = range(Pattern::Aligned, &mut x, STREAM_BYTES as u64, 1 << 20);
                    let b = &mut buf[..len as usize];
                    let want = &object[off as usize..(off + len) as usize];
                    match c.get_range(name, off, b) {
                        Ok(Some(n)) if n as u64 == len && b[..] == *want => {
                            right.fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(Some(n)) if n as u64 == len => {
                            wrong.fetch_add(1, Ordering::SeqCst);
                            let (first, differ, runs, zero) = diagnose(b, want, object);
                            let ids: serde_json::Map<String, Value> = c.last_headers.iter().filter(|(k, _)| k.starts_with("x-fpac") || k == "content-range" || k == "content-length" || k == "transfer-encoding" || k == "cf-ray").map(|(k, v)| (k.clone(), json!(v))).collect();
                            let mut v = bad.lock().unwrap();
                            if v.len() < 200 {
                                v.push(json!({ "nth": nth, "thread": t, "at_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64, "range": [off, off + len - 1], "length": len, "first_diff": first, "differ": differ, "all_zero_after": zero, "runs": runs.iter().map(|(a, src, l)| json!({ "at": a, "object_offset": src, "len": l })).collect::<Vec<_>>(), "headers": ids, "reads_on_this_connection": mine, "connections": c.connects }));
                            }
                        }
                        other => {
                            failed.fetch_add(1, Ordering::SeqCst);
                            let mut f = failures.lock().unwrap();
                            if f.len() < 20 {
                                f.push(format!("{off}+{len}: {other:?}"));
                            }
                        }
                    }
                    let n = nth + 1;
                    if n.is_multiple_of(5000) {
                        eprintln!("{n} reads, {} wrong, {} failed", wrong.load(Ordering::SeqCst), failed.load(Ordering::SeqCst));
                    }
                }
            });
        }
    });
    let out = json!({
        "evidence": "remotecheck", "client": "fpac_remote::Remote, one per thread, kept-alive", "threads": threads, "whole_every": whole_every,
        "reads": reads, "right": right.into_inner(), "wrong": wrong.into_inner(), "failed": failed.into_inner(),
        "whole_gets": wholes.into_inner(), "whole_gets_bad": whole_bad.into_inner(), "ms": t0.elapsed().as_millis() as u64,
        "bad": bad.into_inner().unwrap(), "failures": failures.into_inner().unwrap(),
    });
    if let Some(file) = get("--out") {
        let _ = std::fs::write(&file, out.to_string());
    }
    out
}
