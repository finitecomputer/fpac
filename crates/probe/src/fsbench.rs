//! File-system benchmarks a computer's tools lean on (docs/design.md,
//! Benchmarks to match), run in one directory: on FPAC's `/data`, and on
//! the container's own disk for comparison, in the same guest. Each is the
//! wall time of the operation as a tool would do it; "cold" reads drop the
//! page cache first (`/proc/sys/vm/drop_caches`).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::time::Instant;

use serde_json::{json, Value};

use crate::{ms, stats};

/// Section 10's line: 4 GiB and an fsync.
const SEQ_BYTES: usize = 4 << 30;
const SMALL_FILES: usize = 10_000;
const DIR_ENTRIES: usize = 30_000;
const SAVES: usize = 500;
const RAND_WRITE_MS: u128 = 10_000;
const RAND_FILE_BYTES: u64 = 256 << 20;

fn drop_caches() -> bool {
    // SAFETY: sync has no arguments.
    unsafe { libc::sync() };
    fs::write("/proc/sys/vm/drop_caches", "3").is_ok()
}

fn noise(n: usize, seed: u64) -> Vec<u8> {
    fpac_testkit::conformance::noise(n, seed)
}

/// A sequential write of 4 GiB with an fsync, then a cold read: MB/s.
fn sequential(dir: &Path) -> std::io::Result<Value> {
    let p = dir.join("seq.bin");
    let chunk = noise(1 << 20, 1);
    let t = Instant::now();
    let mut f = File::create(&p)?;
    for _ in 0..SEQ_BYTES / chunk.len() {
        f.write_all(&chunk)?;
    }
    f.sync_all()?;
    let w = t.elapsed().as_secs_f64();
    drop(f);
    let cold = drop_caches();
    let t = Instant::now();
    let mut f = File::open(&p)?;
    let mut buf = vec![0u8; 1 << 20];
    // Bounded by the file's length.
    while f.read(&mut buf)? > 0 {}
    let r = t.elapsed().as_secs_f64();
    fs::remove_file(&p)?;
    let mbs = |s: f64| (SEQ_BYTES as f64 / 1e6 / s * 10.0).round() / 10.0;
    Ok(json!({ "write_fsync_mbs": mbs(w), "read_cold_mbs": mbs(r), "page_cache_dropped": cold }))
}

/// 4 KiB writes at random offsets of a 256 MiB file for 10 s, then an
/// fsync: IOPS, the fsync counted in.
fn random_writes(dir: &Path) -> std::io::Result<Value> {
    let p = dir.join("rand.bin");
    let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).custom_flags(libc::O_CLOEXEC).open(&p)?;
    f.set_len(RAND_FILE_BYTES)?;
    let block = noise(4096, 2);
    let mut x = 0x1234_5678u64;
    let t = Instant::now();
    let mut n = 0u64;
    // Bounded by RAND_WRITE_MS.
    while t.elapsed().as_millis() < RAND_WRITE_MS {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        f.write_all_at(&block, (x % (RAND_FILE_BYTES / 4096)) * 4096)?;
        n += 1;
    }
    f.sync_all()?;
    let s = t.elapsed().as_secs_f64();
    fs::remove_file(&p)?;
    Ok(json!({ "iops": (n as f64 / s).round(), "writes": n }))
}

/// 4 KiB writes with O_SYNC, one at a time: each one's time.
fn sync_writes(dir: &Path) -> std::io::Result<Value> {
    let p = dir.join("osync.bin");
    let f = OpenOptions::new().write(true).create(true).truncate(true).custom_flags(libc::O_SYNC | libc::O_CLOEXEC).open(&p)?;
    let block = noise(4096, 3);
    let mut v = Vec::with_capacity(1000);
    for i in 0..1000u64 {
        let t = Instant::now();
        f.write_all_at(&block, (i % 256) * 4096)?;
        v.push(ms(t.elapsed()));
    }
    fs::remove_file(&p)?;
    Ok(stats(v))
}

/// Small files: create, write 4 KiB and close 10,000; stat them warm and
/// cold; read them cold; delete them.
fn small_files(dir: &Path) -> std::io::Result<Value> {
    let d = dir.join("small");
    fs::create_dir_all(&d)?;
    let body = noise(4096, 4);
    let t = Instant::now();
    for i in 0..SMALL_FILES {
        let mut f = File::create(d.join(format!("f{i}")))?;
        f.write_all(&body)?;
    }
    let create = ms(t.elapsed());
    let t = Instant::now();
    for i in 0..SMALL_FILES {
        fs::metadata(d.join(format!("f{i}")))?;
    }
    let stat_warm = ms(t.elapsed());
    drop_caches();
    let t = Instant::now();
    for i in 0..SMALL_FILES {
        fs::metadata(d.join(format!("f{i}")))?;
    }
    let stat_cold = ms(t.elapsed());
    drop_caches();
    let t = Instant::now();
    let mut buf = Vec::with_capacity(4096);
    for i in 0..SMALL_FILES {
        buf.clear();
        File::open(d.join(format!("f{i}")))?.read_to_end(&mut buf)?;
    }
    let read_cold = ms(t.elapsed());
    let t = Instant::now();
    fs::remove_dir_all(&d)?;
    let delete = ms(t.elapsed());
    let per = |m: f64| (m / SMALL_FILES as f64 * 1000.0).round() / 1000.0;
    Ok(json!({ "files": SMALL_FILES, "create_write_close_ms_each": per(create), "stat_warm_ms_each": per(stat_warm), "stat_cold_ms_each": per(stat_cold), "read_cold_ms_each": per(read_cold), "delete_all_ms": delete }))
}

/// An office save, 500 times: a temp file, its write, an fsync, a rename
/// over the old one; and a scratch file: create, write 16 KiB, read,
/// delete.
fn saves(dir: &Path) -> std::io::Result<Value> {
    let body = noise(16384, 5);
    let (mut save, mut scratch) = (Vec::new(), Vec::new());
    for i in 0..SAVES {
        let t = Instant::now();
        let tmp = dir.join(".doc.tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
        fs::rename(&tmp, dir.join("doc"))?;
        save.push(ms(t.elapsed()));
        let t = Instant::now();
        let p = dir.join(format!("scratch{i}"));
        fs::write(&p, &body)?;
        let _ = fs::read(&p)?;
        fs::remove_file(&p)?;
        scratch.push(ms(t.elapsed()));
    }
    fs::remove_file(dir.join("doc"))?;
    Ok(json!({ "office_save_ms": stats(save), "scratch_file_ms": stats(scratch) }))
}

/// A directory of 30,000 entries: made, then listed warm and cold.
fn big_dir(dir: &Path) -> std::io::Result<Value> {
    let d = dir.join("bigdir");
    fs::create_dir_all(&d)?;
    for i in 0..DIR_ENTRIES {
        File::create(d.join(format!("entry-{i:06}")))?;
    }
    let t = Instant::now();
    let n = fs::read_dir(&d)?.count();
    let warm = ms(t.elapsed());
    drop_caches();
    let t = Instant::now();
    let _ = fs::read_dir(&d)?.count();
    let cold = ms(t.elapsed());
    fs::remove_dir_all(&d)?;
    Ok(json!({ "entries": n, "list_warm_ms": warm, "list_cold_ms": cold }))
}

pub fn run(dir: &str) -> Value {
    let d = Path::new(dir).join("fsbench");
    let _ = fs::remove_dir_all(&d);
    if let Err(e) = fs::create_dir_all(&d) {
        return json!({ "evidence": "fsbench", "error": e.to_string() });
    }
    let r = |v: std::io::Result<Value>| v.unwrap_or_else(|e| json!({ "error": e.to_string() }));
    let out = json!({
        "evidence": "fsbench", "dir": dir,
        "random_4k_write_10s": r(random_writes(&d)),
        "write_4k_osync_ms": r(sync_writes(&d)),
        "small_files": r(small_files(&d)),
        "saves": r(saves(&d)),
        "dir_30k": r(big_dir(&d)),
        // last: each of section 10's rows on its own, not behind a 4 GiB
        // burst still shipping
        "sequential_4gib": r(sequential(&d)),
    });
    let _ = fs::remove_dir_all(&d);
    out
}
