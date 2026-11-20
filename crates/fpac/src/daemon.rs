//! `fpac`: the disk's daemon in the guest (docs/design.md).
//!
//! fpac mount --dir <path> [--host <h>] [--port <p>] [--state <dir>] [--cache <file>]
//! fpac status [--state <dir>]
//!
//! `mount` opens this life's disk from the remote, serves it as one FUSE
//! file, puts a loop device over that and ext4 on the loop device at
//! `--dir` (making ext4 first on a new disk), and stays: it flushes about
//! once a second, and on SIGTERM or SIGINT it unmounts, makes its last
//! flush, the pack and a checkpoint, and exits 0. Each step is one JSON
//! line on stderr with its time. `status` prints what the daemon last
//! wrote to `<state>/status.json`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fpac_device::linux::{self, FuseMount, Loop};
use fpac_device::serve::{Backing, File};
use fpac_format::limits::EXTENT_BYTES;
use fpac_remote::Remote;
use fpac_store::{Ask, OpenInfo, PackView, Store, StoreError};
use serde_json::{json, Value};

use crate::disk::{self, Disk, FileCache};
use crate::flush::{self, Progress};

/// The intercepted host the guest reaches the remote by.
const REMOTE_HOST: &str = "fpac.fragment.internal";
/// How long the boot's reads are noted for the next wake's pack.
const BOOT_RECORD_MS: u64 = 60_000;
/// Bytes of the pack each parallel ranged get brings, and gets at once.
const PACK_RANGE_BYTES: u64 = 4 << 20;
const PACK_FETCHES: usize = 8;
/// Opening may wait this long for an unavailable remote.
const OPEN_MS_MAX: u64 = 60_000;
/// How long the unmount waits for the last user of `--dir` to leave;
/// past it the file system is frozen instead, which makes the device
/// consistent with files still open (a sleep's caller stops its programs
/// first; this is the guard if one is left).
const UMOUNT_MS_MAX: u64 = 2_000;
/// The kernel's writeback in the guest (docs/contract.md, the loss
/// window): a page only written goes to the device within 5 s, not
/// Linux's default 30 s, and the flusher wakes every second.
const DIRTY_EXPIRE_CENTISECS: &str = "500";
const DIRTY_WRITEBACK_CENTISECS: &str = "100";

/// The daemon's memory may be locked up to this (its buffers, the cut's
/// copy): so reclaim never waits on the daemon that serves the writes.
const MEMLOCK_BYTES: u64 = 768 << 20;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn event(t0: Instant, what: &str, extra: Value) {
    let mut v = json!({ "fpac": what, "ms": t0.elapsed().as_secs_f64() * 1000.0 });
    if let (Some(o), Value::Object(e)) = (v.as_object_mut(), extra) {
        o.extend(e);
    }
    eprintln!("{v}");
}

/// A disk's data key from its file: 64 hex digits, a newline allowed. The
/// file is removed once read: the key lives in the daemon's (locked)
/// memory alone.
fn read_key(p: &Path) -> Result<[u8; 32], String> {
    let s = std::fs::read_to_string(p).map_err(|e| format!("the key file: {e}"))?;
    std::fs::remove_file(p).map_err(|e| format!("the key file, removed once read: {e}"))?;
    fpac_remote::unhex32(s.trim_end_matches('\n')).ok_or_else(|| "the key file holds no key (64 lowercase hex digits)".to_string())
}

pub fn main() {
    let args: Vec<String> = std::env::args().collect();
    let state = PathBuf::from(arg(&args, "--state").unwrap_or_else(|| "/run/fpac".into()));
    match args.get(1).map(String::as_str) {
        Some("mount") => {
            let Some(dir) = arg(&args, "--dir") else {
                eprintln!("fpac mount --dir <path>");
                std::process::exit(2);
            };
            let host = arg(&args, "--host").unwrap_or_else(|| REMOTE_HOST.into());
            let port: u16 = arg(&args, "--port").and_then(|p| p.parse().ok()).unwrap_or(80);
            let cache = PathBuf::from(arg(&args, "--cache").unwrap_or_else(|| "/var/lib/fpac/cache".into()));
            // a measurement aid (docs/costs.md): the cache's bound in bytes
            let cache_max: Option<u64> = arg(&args, "--cache-max").and_then(|b| b.parse().ok());
            // a sealed disk (docs/encryption.md): its data key, 64 hex
            // digits in a file the caller made (never on a command line)
            let key = match arg(&args, "--key-file").map(|p| read_key(Path::new(&p))).transpose() {
                Ok(k) => k,
                Err(e) => {
                    eprintln!("{}", json!({ "fpac": "error", "why": e }));
                    std::process::exit(1);
                }
            };
            match mount(Path::new(&dir), &host, port, &state, &cache, cache_max, key) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("{}", json!({ "fpac": "error", "why": e }));
                    std::process::exit(1);
                }
            }
        }
        Some("status") => match std::fs::read_to_string(state.join("status.json")) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("fpac status: {e}");
                std::process::exit(1);
            }
        },
        _ => {
            eprintln!("usage: fpac mount --dir <path> [--host h] [--port p] [--state dir] [--cache file] [--cache-max bytes] [--key-file path] | fpac status [--state dir]");
            std::process::exit(2);
        }
    }
}

/// Blocks SIGTERM and SIGINT in this thread and every thread it starts,
/// so only `sigwait` below takes them.
fn block_signals() -> libc::sigset_t {
    // SAFETY: signal-set calls on a set we own.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    }
}

/// Locks the daemon's memory where the container allows it (it may raise
/// its own limit with CAP_SYS_RESOURCE): answers whether it did.
fn lock_memory() -> bool {
    let lim = libc::rlimit { rlim_cur: MEMLOCK_BYTES, rlim_max: MEMLOCK_BYTES };
    // SAFETY: plain syscalls.
    // MCL_ONFAULT: pages are locked as they are first touched, not all
    // at once (a thread's whole stack, faulted in, cost tens of ms).
    unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &lim) == 0 && libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE | libc::MCL_ONFAULT) == 0 }
}

/// Fetches at once in the open's round (docs/design.md, A wake): a
/// 20 GB ext4 spreads its metadata over 22 map pages, with 16 segment
/// headers and the pack's head that is 39 asks, and each round trip to R2
/// is about 100 ms, so all of them go at once.
const OPEN_FETCHES: usize = 64;

/// Runs `jobs` on at most `workers` threads, in order of start, each
/// answer in its job's place.
fn pool<T: Send, R: Send>(jobs: Vec<T>, workers: usize, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let n = jobs.len();
    let jobs = std::sync::Mutex::new(jobs.into_iter().enumerate().collect::<std::collections::VecDeque<_>>());
    let out = std::sync::Mutex::new((0..n).map(|_| None).collect::<Vec<Option<R>>>());
    std::thread::scope(|s| {
        for _ in 0..workers.min(n) {
            s.spawn(|| {
                // Bounded by the jobs.
                loop {
                    let Some((i, job)) = jobs.lock().unwrap().pop_front() else { break };
                    let r = f(job);
                    out.lock().unwrap()[i] = Some(r);
                }
            });
        }
    });
    out.into_inner().unwrap().into_iter().map(|r| r.expect("every job ran")).collect()
}

/// The remote for an open. The store names its second round at once
/// (`prefetch`: pages, the replay's first headers, the pack's head); this
/// fetches them in parallel and answers the calls that follow from them.
/// A replay past the asked headers fetches the next OPEN_SEGMENT_HEADERS
/// at once on its first miss. A fetch that failed is not kept: the call
/// that follows makes it again, directly.
struct Prefetching<'a> {
    remote: &'a mut Remote,
    host: String,
    port: u16,
    fetched: std::collections::HashMap<Ask, Option<Vec<u8>>>,
    /// Each parallel round: its asks and its time, for the open's event.
    rounds: Vec<Value>,
}

/// A ranged ask larger than this is fetched as pieces of it in parallel
/// and joined: one 4 MiB get took 250 to 590 ms on Cloudflare, four 1 MiB
/// ones at once about a round trip (P5's four parallel gets: 176 to 194
/// MB/s).
const OPEN_PIECE_BYTES: usize = 1 << 20;

impl Prefetching<'_> {
    fn fetch_all(&mut self, asks: Vec<Ask>) {
        // the big ranges in pieces, joined below
        let mut pieces: Vec<(usize, Ask)> = Vec::new();
        for (i, a) in asks.iter().enumerate() {
            match a {
                Ask::Range { name, offset, len } if *len > OPEN_PIECE_BYTES => {
                    let mut at = 0;
                    while at < *len {
                        let n = OPEN_PIECE_BYTES.min(len - at);
                        pieces.push((i, Ask::Range { name: name.clone(), offset: offset + at as u64, len: n }));
                        at += n;
                    }
                }
                _ => pieces.push((i, a.clone())),
            }
        }
        let t = Instant::now();
        let count = asks.len();
        let (host, port) = (self.host.clone(), self.port);
        let fetched = pool(pieces.iter().map(|(_, a)| a.clone()).collect(), OPEN_FETCHES, |a| {
            let mut r = Remote::new(&host, port);
            match &a {
                Ask::Whole(name) => {
                    let mut body = Vec::new();
                    r.get(name, &mut |b| body.extend_from_slice(b)).ok().map(|o| o.map(|_| body))
                }
                Ask::Range { name, offset, len } => {
                    let mut b = vec![0u8; *len];
                    match r.get_range(name, *offset, &mut b) {
                        Ok(o) => Some(o.map(|k| {
                            b.truncate(k);
                            b
                        })),
                        // a piece past the object's end: it ended before
                        Err(fpac_remote::RemoteError::Refused { status: 416, .. }) => Some(None),
                        Err(_) => None,
                    }
                }
            }
        });
        // join each ask's pieces in order: absent if the first is, short
        // where the object ended, failed if any piece failed
        let mut joined: Vec<Option<Option<Vec<u8>>>> = vec![Some(None); asks.len()];
        let mut started = vec![false; asks.len()];
        for ((i, _), g) in pieces.iter().zip(fetched) {
            let slot = &mut joined[*i];
            match (g, slot.as_mut()) {
                (None, _) => *slot = None,
                (Some(None), Some(_)) if !started[*i] => {}
                (Some(Some(b)), Some(acc)) => {
                    started[*i] = true;
                    acc.get_or_insert_with(Vec::new).extend_from_slice(&b);
                }
                _ => {}
            }
        }
        let mut failed = 0;
        for (a, g) in asks.into_iter().zip(joined) {
            match g {
                Some(g) => {
                    self.fetched.insert(a, g);
                }
                None => failed += 1,
            }
        }
        self.rounds.push(json!({ "asks": count, "failed": failed, "ms": t.elapsed().as_secs_f64() * 1000.0 }));
    }
}

impl fpac_store::RemoteGate for Prefetching<'_> {
    fn get_range(&mut self, name: &str, offset: u64, buf: &mut [u8]) -> Result<Option<usize>, fpac_remote::RemoteError> {
        let ask = Ask::Range { name: name.to_string(), offset, len: buf.len() };
        if !self.fetched.contains_key(&ask) {
            // a replay's header past the asked ones: the next round of them
            let seg = name.split_once("/s").and_then(|(e, n)| Some((e.to_string(), n.parse::<u64>().ok()?)));
            if let (0, Some((epoch, seq))) = (offset, seg) {
                let asks = (seq..seq + u64::from(fpac_store::OPEN_SEGMENT_HEADERS)).map(|q| Ask::Range { name: format!("{epoch}/s{q}"), offset: 0, len: buf.len() }).collect();
                self.fetch_all(asks);
            }
        }
        // one answer per ask: a header that fails its checksum is
        // fetched again directly
        if let Some(h) = self.fetched.remove(&ask) {
            return Ok(h.map(|b| {
                buf[..b.len()].copy_from_slice(&b);
                b.len()
            }));
        }
        self.remote.get_range(name, offset, buf)
    }

    fn get(&mut self, name: &str, sink: &mut dyn FnMut(&[u8])) -> Result<Option<u64>, fpac_remote::RemoteError> {
        if let Some(b) = self.fetched.remove(&Ask::Whole(name.to_string())) {
            return Ok(b.map(|b| {
                sink(&b);
                b.len() as u64
            }));
        }
        self.remote.get(name, sink)
    }

    fn put(&mut self, name: &str, body: &[u8]) -> Result<fpac_remote::Put, fpac_remote::RemoteError> {
        self.remote.put(name, body)
    }

    fn prefetch(&mut self, asks: &[Ask]) {
        self.fetch_all(asks.to_vec());
    }
}

/// An opened store, and what the open's round fetched that the open did
/// not read: the pack's head, when there is a pack.
struct Opened {
    store: Store,
    volume: u64,
    disk: String,
    fresh: bool,
    /// The packs to fetch (the boot pack, and after a crash the crash
    /// pack), each with the head the open's round fetched, if it did.
    packs: Vec<(&'static str, String, fpac_format::PackRef, Option<Vec<u8>>)>,
    rounds: Vec<Value>,
}

fn open_store(host: &str, port: u16, key: Option<[u8; 32]>) -> Result<Opened, String> {
    let t0 = Instant::now();
    let mut last = String::new();
    // Bounded by OPEN_MS_MAX.
    while t0.elapsed() < Duration::from_millis(OPEN_MS_MAX) {
        let mut r = Remote::new(host, port);
        match r.open() {
            Ok(o) => {
                let fresh = o.head.is_none();
                // the seal binds the disk's name, which the open answers
                let seal = key.map(|k| Arc::new(fpac_format::Sealer::new(&k, &o.disk)));
                let info = OpenInfo { epoch: o.epoch, volume_bytes: o.volume_bytes - o.volume_bytes % EXTENT_BYTES as u64, head: o.head, seal };
                let mut gate = Prefetching { remote: &mut r, host: host.to_string(), port, fetched: Default::default(), rounds: Vec::new() };
                match Store::open(&mut gate, &info) {
                    Ok(store) => {
                        let packs = [("pack", store.pack_to_fetch()), ("crash-pack", store.crash_pack_to_fetch())]
                            .into_iter()
                            .filter_map(|(kind, p)| p.map(|(name, pr)| (kind, name, pr)))
                            .map(|(kind, name, pr)| {
                                let head = gate.fetched.remove(&Ask::Range { name: name.clone(), offset: 0, len: fpac_store::pack_head_fetch_bytes(store.sealed()) }).flatten();
                                (kind, name, pr, head)
                            })
                            .collect();
                        let rounds = std::mem::take(&mut gate.rounds);
                        return Ok(Opened { store, volume: info.volume_bytes, disk: o.disk, fresh, packs, rounds });
                    }
                    Err(StoreError::Unavailable(e)) => last = e,
                    Err(e) => return Err(format!("the disk does not open: {e}")),
                }
            }
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(format!("the remote is unavailable: {last}"))
}

/// The pack this life opened with: its head (its header and first
/// extents, fetched in the open's round, or here when that fetch failed),
/// those extents installed at once and the rest marked as coming, so a
/// read of one waits for it rather than fetching it twice. Answers the
/// pack's name, header and how many extents the head held; None when
/// there is no pack.
fn pack_header(disk: &Disk, t0: Instant, kind: &str, name: String, pr: fpac_format::PackRef, head: Option<Vec<u8>>) -> Option<(PackView, u64)> {
    let sealed = disk.inner.lock().unwrap().store.sealed();
    let head = head.or_else(|| {
        let mut r = disk.remote();
        let mut b = vec![0u8; fpac_store::pack_head_fetch_bytes(sealed)];
        let got = r.get_range(&name, 0, &mut b);
        disk.give_back(r);
        got.ok().flatten().map(|n| {
            b.truncate(n);
            b
        })
    });
    let header = match &head {
        Some(b) => disk.inner.lock().unwrap().store.pack_header(&name, &pr, b),
        None => Err(StoreError::Unavailable("the pack's head".into())),
    };
    match (header, head) {
        (Ok(v), Some(b)) => {
            let (at, stride) = (v.images_at as usize, v.stride as usize);
            let held = (b.len().saturating_sub(at) / stride).min(v.header.entries.len());
            disk.expect_pack(v.header.entries.iter().enumerate().skip(held).map(|(i, (e, _))| (*e, i)));
            let t = Instant::now();
            let installed = {
                let mut g = disk.inner.lock().unwrap();
                let k = {
                    let disk::Inner { store, cache } = &mut *g;
                    store.pack_install(cache, &v, v.images_at, &b[at..at + held * stride]).unwrap_or(0)
                };
                let _ = disk.took(&mut g, v.header.entries.iter().take(held).map(|(e, _)| *e));
                k
            };
            let install_ms = t.elapsed().as_secs_f64() * 1000.0;
            event(t0, &format!("{kind}-header"), json!({ "extents": v.header.entries.len(), "head_extents": held, "head_installed": installed, "head_install_ms": install_ms, "sealed": sealed }));
            Some((v, held as u64))
        }
        (r, _) => {
            let why = r.err().map(|e| e.to_string()).unwrap_or_default();
            event(t0, &format!("{kind}-skipped"), json!({ "why": why }));
            None
        }
    }
}

/// Fetches the rest of the pack, from extent `from`, in ranges on
/// PACK_FETCHES connections at once, installing each range as it arrives;
/// whatever did not arrive stops being waited for at the end.
fn fetch_pack(disk: &Disk, t0: Instant, kind: &str, v: PackView, from: u64) {
    let ranges = Store::pack_ranges(&v, from, PACK_RANGE_BYTES);
    let installed = std::sync::atomic::AtomicU32::new(0);
    // the time ranges spent arriving, and installing (opening each seal
    // and writing the cache), summed over the connections (µs)
    let (fetch_us, install_us) = (AtomicU64::new(0), AtomicU64::new(0));
    let entries = &v.header.entries;
    pool(ranges, PACK_FETCHES, |(off, len)| {
        let mut r = disk.remote();
        let mut b = vec![0u8; len as usize];
        let t = Instant::now();
        let got = r.get_range(&v.name, off, &mut b);
        fetch_us.fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
        disk.give_back(r);
        let first = ((off - v.images_at) / v.stride) as usize;
        let count = (len / v.stride) as usize;
        if let Ok(Some(n)) = got {
            let t = Instant::now();
            let mut g = disk.inner.lock().unwrap();
            let k = {
                let disk::Inner { store, cache } = &mut *g;
                store.pack_install(cache, &v, off, &b[..n - n % v.stride as usize])
            };
            if let Ok(k) = k {
                installed.fetch_add(k, Ordering::Relaxed);
            }
            let _ = disk.took(&mut g, entries.iter().skip(first).take(count).map(|(e, _)| *e));
            drop(g);
            install_us.fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
        // this range's extents are no longer coming
        disk.arrived(entries.iter().skip(first).take(count).map(|(e, _)| *e));
    });
    disk.arrived(entries.iter().map(|(e, _)| *e));
    event(t0, kind, json!({ "extents": entries.len(), "from": from, "waited_at": *disk.waited.lock().unwrap(), "installed": installed.into_inner(), "bytes": entries.len() as u64 * v.stride, "fetch_ms_summed": fetch_us.into_inner() as f64 / 1000.0, "install_ms_summed": install_us.into_inner() as f64 / 1000.0 }));
}

/// A crash wake's fetch of what its replay brought back: PACK_FETCHES
/// ranged gets at once, each installed as it arrives.
fn fetch_replayed(disk: &Disk, t0: Instant, fetches: Vec<fpac_store::Fetch>) {
    let n = fetches.len();
    let installed = std::sync::atomic::AtomicU32::new(0);
    pool(fetches, PACK_FETCHES, |f| {
        let mut r = disk.remote();
        let mut window = vec![0u8; f.len];
        let got = r.get_range(&f.name, f.offset, &mut window);
        disk.give_back(r);
        if let Ok(Some(k)) = got {
            let mut g = disk.inner.lock().unwrap();
            let ok = {
                let disk::Inner { store, cache } = &mut *g;
                store.install(cache, &f, &window[..k], &mut vec![0u8; EXTENT_BYTES as usize])
            };
            if ok.is_ok() {
                installed.fetch_add(1, Ordering::Relaxed);
            }
            let _ = disk.took(&mut g, f.extents());
        }
        disk.arrived(f.extents());
    });
    event(t0, "replay-fetched", json!({ "fetches": n, "installed": installed.into_inner() }));
}

/// The daemon's CPU time so far, user and system (ms): with the bytes
/// shipped, what a sealed disk costs per GB (docs/encryption.md).
fn cpu_ms() -> serde_json::Value {
    // SAFETY: getrusage fills the struct it is given.
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) } != 0 {
        return serde_json::Value::Null;
    }
    let ms = |t: libc::timeval| t.tv_sec as f64 * 1000.0 + t.tv_usec as f64 / 1000.0;
    json!({ "user": ms(u.ru_utime), "system": ms(u.ru_stime) })
}

fn status_writer(state: PathBuf, t0: Instant) -> impl Fn(&Disk, &Progress) {
    move |disk: &Disk, p: &Progress| {
        let (dirty, shipping, epoch) = {
            let g = disk.inner.lock().unwrap();
            (g.store.dirty_extents(), g.store.shipping(), g.store.epoch())
        };
        let s = json!({
            "epoch": epoch, "up_ms": t0.elapsed().as_millis() as u64, "dirty_extents": dirty, "shipping_segments": shipping,
            "cuts": p.cuts.load(Ordering::Relaxed), "segments": p.segments.load(Ordering::Relaxed), "bytes_shipped": p.bytes.load(Ordering::Relaxed),
            "checkpoints": p.checkpoints.load(Ordering::Relaxed), "durable_ms": p.durable_ms.load(Ordering::SeqCst), "head_posted": p.head_posted.load(Ordering::SeqCst),
            "unshipped_extents": disk.inner.lock().unwrap().store.unshipped_extents(), "planned_segments": disk.inner.lock().unwrap().store.planned(), "misses": disk.stats.misses.load(Ordering::Relaxed), "pack_waits": disk.stats.pack_waits.load(Ordering::Relaxed), "page_loads": disk.stats.page_loads.load(Ordering::Relaxed), "pages_unloaded": disk.inner.lock().unwrap().store.pages_unloaded(), "evictions": disk.stats.evictions.load(Ordering::Relaxed), "write_errors": disk.stats.write_errors.load(Ordering::Relaxed), "cached_extents": disk.inner.lock().unwrap().store.cached_extents(), "fetch_retries": disk.stats.fetch_retries.load(Ordering::Relaxed),
            "read_errors": disk.stats.read_errors.load(Ordering::Relaxed), "full_waits": disk.stats.full_waits.load(Ordering::Relaxed),
            "syncs": disk.stats.syncs.load(Ordering::Relaxed), "cpu_ms": cpu_ms(),
            "lock_waits_over_1ms": disk.stats.lock_waits_over_1ms.load(Ordering::Relaxed), "lock_wait_max_us": disk.stats.lock_wait_max_us.load(Ordering::Relaxed),
            "fenced": disk.fenced.load(Ordering::SeqCst), "now_ms": flush::now_ms(),
        });
        let tmp = state.join("status.json.tmp");
        if std::fs::write(&tmp, s.to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, state.join("status.json"));
        }
    }
}

fn mount(dir: &Path, host: &str, port: u16, state: &Path, cache_path: &Path, cache_max: Option<u64>, key: Option<[u8; 32]>) -> Result<(), String> {
    let t0 = Instant::now();
    let signals = block_signals();
    let locked = lock_memory();
    let writeback = ["dirty_expire_centisecs", "dirty_writeback_centisecs"]
        .iter()
        .zip([DIRTY_EXPIRE_CENTISECS, DIRTY_WRITEBACK_CENTISECS])
        .all(|(k, v)| std::fs::write(format!("/proc/sys/vm/{k}"), v).is_ok());
    std::fs::create_dir_all(state).map_err(|e| format!("{}: {e}", state.display()))?;
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let Opened { mut store, volume, disk: disk_name, fresh, packs, rounds } = open_store(host, port, key)?;
    event(t0, "opened", json!({ "disk": disk_name, "epoch": store.epoch(), "volume_bytes": volume, "new": fresh, "replayed": store.opened().replayed, "extents_mapped": store.opened().extents_mapped, "pages": store.opened().pages, "rounds": rounds, "memory_locked": locked, "writeback_set": writeback, "sealed": store.sealed() }));
    store.record_boot();
    // a sealed disk's cache holds only ciphertext: the host's disk is not
    // trusted (docs/encryption.md)
    let cache_seal = key.map(|k| (Arc::new(fpac_format::Sealer::new(&k, &disk_name)), store.epoch()));
    let cache = FileCache::create(cache_path, volume, cache_seal).map_err(|e| format!("the cache file: {e}"))?;
    let disk = Arc::new(Disk::new(store, cache, host, port, cache_max).map_err(|e| format!("the cache file: {e}"))?);
    let progress = Arc::new(Progress::default());
    // the pack's head before the mount; the rest of it beside the mount
    // after a crash: the newest extents the replay brought back, beside
    // the mount (ext4's recovery reads them)
    let replay = disk.inner.lock().unwrap().store.replay_fetches();
    if !replay.is_empty() {
        disk.expect_pack(replay.iter().flat_map(|f| f.extents().collect::<Vec<_>>()).map(|e| (e, usize::MAX)));
        let d = disk.clone();
        std::thread::spawn(move || fetch_replayed(&d, t0, replay));
    }
    for (kind, name, pr, head) in packs {
        if let Some((v, from)) = pack_header(&disk, t0, kind, name, pr, head) {
            let d = disk.clone();
            std::thread::spawn(move || fetch_pack(&d, t0, kind, v, from));
        }
    }
    let flusher = {
        let (d, p, w) = (disk.clone(), progress.clone(), status_writer(state.to_path_buf(), t0));
        std::thread::spawn(move || flush::run(d, p, w))
    };
    {
        let d = disk.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(BOOT_RECORD_MS));
            d.inner.lock().unwrap().store.stop_boot();
        });
    }
    let fuse_dir = state.join("fuse");
    std::fs::create_dir_all(&fuse_dir).map_err(|e| e.to_string())?;
    let backing: Arc<dyn Backing> = disk.clone();
    let fuse = FuseMount::mount(&fuse_dir, File::new(volume), backing).map_err(|e| e.to_string())?;
    event(t0, "fuse", json!({ "io_flusher": fuse.io_flusher }));
    let lo = Loop::attach(&fuse.path(), true).map_err(|e| e.to_string())?;
    event(t0, "loop", json!({ "device": lo.path, "direct_io": lo.direct_io }));
    if fresh {
        linux::mkfs_ext4(&lo.path).map_err(|e| e.to_string())?;
        event(t0, "mkfs", json!({}));
    }
    linux::mount_ext4(&lo.path, dir).map_err(|e| e.to_string())?;
    event(t0, "mounted", json!({ "dir": dir, "misses": disk.stats.misses.load(Ordering::Relaxed), "pack_waits": disk.stats.pack_waits.load(Ordering::Relaxed), "waited_at": *disk.waited.lock().unwrap() }));
    // until SIGTERM or SIGINT
    let mut sig: libc::c_int = 0;
    // SAFETY: sigwait on the set this thread blocked.
    unsafe { libc::sigwait(&signals, &mut sig) };
    event(t0, "signal", json!({ "signal": sig }));
    let unmounted = linux::umount(dir, Duration::from_millis(UMOUNT_MS_MAX));
    let ok = unmounted.is_ok();
    event(t0, "ext4-unmounted", json!({ "ok": ok, "error": unmounted.err().map(|e| e.to_string()) }));
    if ok {
        let _ = lo.detach();
    } else {
        let frozen = linux::freeze(dir);
        event(t0, "ext4-frozen", json!({ "ok": frozen.is_ok(), "error": frozen.err().map(|e| e.to_string()) }));
    }
    disk.stop_flusher();
    let _ = flusher.join();
    let last = flush::last(&disk, &progress).map_err(|e| e.to_string());
    status_writer(state.to_path_buf(), t0)(&disk, &progress);
    event(t0, "flushed", json!({ "last": last.as_ref().ok(), "error": last.as_ref().err() }));
    if ok {
        let _ = fuse.unmount();
    }
    event(t0, "exit", json!({}));
    last.map(|_| ())
}
