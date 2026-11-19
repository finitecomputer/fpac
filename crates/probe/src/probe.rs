//! The probe of the faces (docs/spike.md, Acceptance 1): what the kernel
//! and the container offer, then, unless `check_only`, the face itself
//! built step by step: a FUSE file, a loop device over it with direct
//! I/O, ext4 on that, a write and a read through it, and the same on the
//! container's own disk for comparison. Each step's outcome and time is
//! recorded; the first failure ends the build, and what came before is
//! undone.
//!
//! The FUSE file's backing here is a plain local file, a stand-in for the
//! store, named so in the output: this measures the face, not the store.

use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use fpac_device::linux::{self, FuseMount, LocalFile, Loop};
use fpac_device::serve::File;
use serde_json::{json, Value};

use crate::ms;

/// The FUSE file's size for the probe: big enough for ext4's journal and
/// the sniff's file, small enough to make fast.
const VOLUME_BYTES: u64 = 2 << 30;
/// The sniff test's file: written with 1 MiB writes, then synced, then
/// read back after the page cache is dropped.
const SNIFF_BYTES: usize = 256 << 20;
const EXT4_MAGIC: u64 = 0xEF53;

/// Capabilities that matter to a face, by number (`linux/capability.h`).
const CAPS: [(&str, u32); 5] = [("CAP_SYS_ADMIN", 21), ("CAP_MKNOD", 27), ("CAP_SYS_RESOURCE", 24), ("CAP_IPC_LOCK", 14), ("CAP_SYS_RAWIO", 17)];

fn read(p: &str) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn uname() -> Value {
    // SAFETY: uname into a zeroed struct we own.
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut u) };
    let s = |f: &[libc::c_char]| f.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char).collect::<String>();
    json!({ "release": s(&u.release), "version": s(&u.version), "machine": s(&u.machine) })
}

fn caps() -> Value {
    let status = read("/proc/self/status");
    let field = |name: &str| status.lines().find_map(|l| l.strip_prefix(name)).map(|v| v.trim().to_string()).unwrap_or_default();
    let eff = u64::from_str_radix(&field("CapEff:"), 16).unwrap_or(0);
    let held: Vec<&str> = CAPS.iter().filter(|(_, n)| eff & (1 << n) != 0).map(|(c, _)| *c).collect();
    let missing: Vec<&str> = CAPS.iter().filter(|(_, n)| eff & (1 << n) == 0).map(|(c, _)| *c).collect();
    json!({ "uid": unsafe { libc::geteuid() }, "effective": field("CapEff:"), "bounding": field("CapBnd:"), "held": held, "missing": missing, "seccomp": field("Seccomp:"), "no_new_privs": field("NoNewPrivs:") })
}

/// /proc/devices, the drivers that matter: registered majors, whether or
/// not a node for them is in /dev.
fn devices() -> Value {
    let text = read("/proc/devices");
    let mut section = "";
    let (mut chars, mut blocks) = (Vec::new(), Vec::new());
    for l in text.lines() {
        if l.starts_with("Character") {
            section = "c";
        } else if l.starts_with("Block") {
            section = "b";
        } else if let Some((major, name)) = l.trim().split_once(' ') {
            let interesting = ["fuse", "loop", "nbd", "ublk", "misc", "virtblk", "device-mapper"].iter().any(|k| name.contains(k));
            if interesting {
                let e = json!({ "major": major.trim(), "name": name.trim() });
                if section == "c" {
                    chars.push(e);
                } else {
                    blocks.push(e);
                }
            }
        }
    }
    json!({ "char": chars, "block": blocks })
}

fn node(path: &str) -> Value {
    match std::fs::metadata(path) {
        Ok(m) => {
            let kind = if m.file_type().is_char_device() {
                "char"
            } else if m.file_type().is_block_device() {
                "block"
            } else {
                "other"
            };
            let open = std::fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC).open(path);
            json!({ "exists": true, "kind": kind, "major": libc::major(m.rdev()), "minor": libc::minor(m.rdev()), "opens": open.is_ok(), "open_error": open.err().map(|e| e.to_string()) })
        }
        Err(e) => json!({ "exists": false, "error": e.to_string() }),
    }
}

fn io_flusher() -> Value {
    // SAFETY: prctl on the calling thread; undone at once.
    let set = unsafe { libc::prctl(57, 1, 0, 0, 0) };
    let err = (set != 0).then(|| std::io::Error::last_os_error().to_string());
    if set == 0 {
        unsafe { libc::prctl(57, 0, 0, 0, 0) };
    }
    json!({ "ok": set == 0, "error": err })
}

fn mlockall() -> Value {
    // SAFETY: locks this process's pages, then unlocks them.
    let r = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
    let err = (r != 0).then(|| std::io::Error::last_os_error().to_string());
    if r == 0 {
        unsafe { libc::munlockall() };
    }
    let limit = read("/proc/self/limits").lines().find(|l| l.starts_with("Max locked memory")).map(|l| l.split_whitespace().skip(3).take(2).collect::<Vec<_>>().join(" ")).unwrap_or_default();
    json!({ "ok": r == 0, "error": err, "rlimit_memlock": limit })
}

fn statfs(path: &str) -> Value {
    // SAFETY: statfs into a zeroed struct we own.
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    let c = std::ffi::CString::new(path).unwrap();
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return json!({ "error": std::io::Error::last_os_error().to_string() });
    }
    json!({ "type": format!("{:#x}", s.f_type), "bytes": s.f_blocks as u64 * s.f_bsize as u64, "free_bytes": s.f_bavail as u64 * s.f_bsize as u64 })
}

struct Steps {
    list: Vec<Value>,
    failed: bool,
}

impl Steps {
    /// Runs one step unless an earlier one failed: its value, recorded.
    fn step<T>(&mut self, name: &str, f: impl FnOnce() -> Result<(T, Value), String>) -> Option<T> {
        if self.failed {
            return None;
        }
        let t = Instant::now();
        let r = f();
        let took = ms(t.elapsed());
        match r {
            Ok((v, detail)) => {
                self.list.push(json!({ "step": name, "ok": true, "ms": took, "detail": detail }));
                Some(v)
            }
            Err(e) => {
                self.list.push(json!({ "step": name, "ok": false, "ms": took, "error": e }));
                self.failed = true;
                None
            }
        }
    }

    /// An undo step: always runs, recorded, never stops the others.
    fn undo(&mut self, name: &str, f: impl FnOnce() -> Result<(), String>) {
        let t = Instant::now();
        let r = f();
        self.list.push(json!({ "step": name, "ok": r.is_ok(), "ms": ms(t.elapsed()), "error": r.err() }));
    }
}

fn drop_caches() -> bool {
    // SAFETY: sync has no arguments.
    unsafe { libc::sync() };
    std::fs::write("/proc/sys/vm/drop_caches", "3").is_ok()
}

/// Writes `SNIFF_BYTES` to `path` with 1 MiB writes and an fsync, then
/// reads it back after dropping the page cache: MB/s each way.
fn sniff(path: &Path) -> Result<Value, String> {
    let chunk: Vec<u8> = fpac_testkit::conformance::noise(1 << 20, 9);
    let t = Instant::now();
    let mut f = std::fs::File::create(path).map_err(|e| e.to_string())?;
    for _ in 0..SNIFF_BYTES / chunk.len() {
        f.write_all(&chunk).map_err(|e| e.to_string())?;
    }
    f.sync_all().map_err(|e| e.to_string())?;
    let write = t.elapsed();
    drop(f);
    let dropped = drop_caches();
    let t = Instant::now();
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0usize;
    let mut same = true;
    // Bounded by the file's length.
    loop {
        let k = f.read(&mut buf).map_err(|e| e.to_string())?;
        if k == 0 {
            break;
        }
        same &= buf[..k] == chunk[..k];
        n += k;
    }
    let read = t.elapsed();
    let _ = std::fs::remove_file(path);
    let mbs = |d: std::time::Duration| (SNIFF_BYTES as f64 / 1e6 / d.as_secs_f64() * 10.0).round() / 10.0;
    if n != SNIFF_BYTES || !same {
        return Err(format!("read back {n} bytes, same: {same}"));
    }
    Ok(json!({ "bytes": SNIFF_BYTES, "write_mbs": mbs(write), "read_mbs": mbs(read), "page_cache_dropped": dropped }))
}

pub fn run(check_only: bool, dir: &str) -> Value {
    let modules: serde_json::Map<String, Value> = ["loop", "fuse", "nbd", "ublk_drv"].iter().map(|m| (m.to_string(), json!(Path::new(&format!("/sys/module/{m}")).exists()))).collect();
    let nodes: serde_json::Map<String, Value> = ["/dev/fuse", "/dev/loop-control", "/dev/loop0", "/dev/ublk-control", "/dev/nbd0"].iter().map(|p| (p.to_string(), node(p))).collect();
    let mut out = json!({
        "evidence": "probe",
        "mode": if check_only { "check-only" } else { "full" },
        "kernel": uname(),
        "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "mem_total": read("/proc/meminfo").lines().next().unwrap_or("").to_string(),
        "caps": caps(),
        "devices": devices(),
        "filesystems": read("/proc/filesystems").lines().filter(|l| ["ext4", "fuse", "overlay", "virtiofs", "tmpfs"].iter().any(|k| l.contains(k))).map(|l| l.trim().to_string()).collect::<Vec<_>>(),
        "modules": modules,
        "nodes": nodes,
        "io_flusher": io_flusher(),
        "mlockall": mlockall(),
        "root_fs": statfs("/"),
        "dirty_expire_centisecs": read("/proc/sys/vm/dirty_expire_centisecs").trim(),
    });
    if check_only {
        return out;
    }
    let dir = PathBuf::from(dir);
    let _ = std::fs::create_dir_all(&dir);
    out["scratch_fs"] = statfs(&dir.to_string_lossy());
    let mut s = Steps { list: Vec::new(), failed: false };
    // Nodes a container's /dev may lack while the kernel has the driver.
    s.step("node /dev/fuse (char 10:229)", || linux::ensure_node(Path::new("/dev/fuse"), false, 10, 229).map(|m| ((), json!({ "made": m }))).map_err(|e| e.to_string()));
    s.step("node /dev/loop-control (char 10:237)", || linux::ensure_node(Path::new("/dev/loop-control"), false, 10, 237).map(|m| ((), json!({ "made": m }))).map_err(|e| e.to_string()));
    let local = s.step("sniff: the container's own disk", || sniff(&dir.join("sniff.bin")).map(|v| ((), v)));
    let _ = local;
    let backing_path = dir.join("backing.img");
    let fuse_dir = dir.join("fuse");
    let ext4_dir = dir.join("ext4");
    let _ = std::fs::create_dir_all(&fuse_dir);
    let _ = std::fs::create_dir_all(&ext4_dir);
    let backing = s.step("backing: a sparse local file (the stand-in for the store)", || LocalFile::create(&backing_path, VOLUME_BYTES).map(|b| (b, json!({ "bytes": VOLUME_BYTES }))).map_err(|e| e.to_string()));
    let mount = backing.and_then(|b| {
        s.step("fuse: mount one file over /dev/fuse", || {
            FuseMount::mount(&fuse_dir, File::new(VOLUME_BYTES), Arc::new(b)).map(|m| {
                let d = json!({ "threads": m.io_flusher.len(), "io_flusher": m.io_flusher });
                (m, d)
            }).map_err(|e| e.to_string())
        })
    });
    let file_ok = mount.as_ref().and_then(|m| s.step("fuse: the file is there", || std::fs::metadata(m.path()).map(|md| ((), json!({ "size": md.len() }))).map_err(|e| e.to_string())));
    let lo = file_ok.and_then(|_| {
        let p = mount.as_ref().unwrap().path();
        s.step("loop: attach over the FUSE file, direct I/O, 4 KiB blocks", || Loop::attach(&p, true).map(|l| {
            let d = json!({ "device": l.path, "direct_io": l.direct_io });
            (l, d)
        }).map_err(|e| e.to_string()))
    });
    let made = lo.as_ref().and_then(|l| s.step("ext4: mke2fs", || linux::mkfs_ext4(&l.path).map(|_| ((), json!({}))).map_err(|e| e.to_string())));
    let mounted = made.and_then(|_| s.step("ext4: mount (noatime, commit=1, discard)", || linux::mount_ext4(&lo.as_ref().unwrap().path, &ext4_dir).map(|_| ((), json!({}))).map_err(|e| e.to_string())));
    let typed = mounted.and_then(|_| s.step("ext4: statfs reads ext4", || {
        let t = linux::fs_type(&ext4_dir).map_err(|e| e.to_string())?;
        if t == EXT4_MAGIC {
            Ok(((), json!({ "type": format!("{t:#x}") })))
        } else {
            Err(format!("type {t:#x}"))
        }
    }));
    let _ = typed.and_then(|_| s.step("sniff: ext4 on the loop device on the FUSE file", || sniff(&ext4_dir.join("sniff.bin")).map(|v| ((), v))));
    let _ = s.step("ext4: a file survives an unmount and a mount", || {
        std::fs::write(ext4_dir.join("kept"), b"kept").map_err(|e| e.to_string())?;
        linux::umount(&ext4_dir, std::time::Duration::from_secs(5)).map_err(|e| e.to_string())?;
        linux::mount_ext4(&lo.as_ref().unwrap().path, &ext4_dir).map_err(|e| e.to_string())?;
        let got = std::fs::read(ext4_dir.join("kept")).map_err(|e| e.to_string())?;
        if got == b"kept" {
            Ok(((), json!({})))
        } else {
            Err("read back something else".into())
        }
    });
    let failed = s.failed;
    // Undo whatever was built, in reverse.
    if mounted.is_some() {
        s.undo("ext4: unmount", || linux::umount(&ext4_dir, std::time::Duration::from_secs(5)).map_err(|e| e.to_string()));
    }
    if let Some(l) = lo {
        let p = l.path.clone();
        s.undo("ext4: e2fsck -f -n through the loop device", || linux::fsck_ext4(&p).map(|_| ()).map_err(|e| e.to_string()));
        s.undo("loop: detach", || l.detach().map_err(|e| e.to_string()));
    }
    if let Some(m) = mount {
        s.undo("fuse: unmount", || m.unmount().map_err(|e| e.to_string()));
    }
    let _ = std::fs::remove_file(&backing_path);
    let steps = s.list;
    let ok = |name: &str| steps.iter().any(|v| v["step"].as_str().is_some_and(|s| s.starts_with(name)) && v["ok"] == true);
    let dio = steps.iter().find(|v| v["step"].as_str().is_some_and(|s| s.starts_with("loop: attach"))).map(|v| v["detail"]["direct_io"] == true).unwrap_or(false);
    out["faces"] = json!({
        "fuse_file": ok("fuse: the file is there"),
        "loop_over_fuse": ok("loop: attach"),
        "loop_direct_io": dio,
        "ext4_on_loop": ok("ext4: statfs reads ext4"),
        "ublk_control": Path::new("/dev/ublk-control").exists(),
        "nbd": Path::new("/dev/nbd0").exists() || read("/proc/devices").lines().any(|l| l.ends_with(" nbd")),
        "all_steps_ok": !failed,
    });
    out["steps"] = json!(steps);
    out
}
