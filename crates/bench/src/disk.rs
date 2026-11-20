//! The disk across lives (docs/spike.md, P4 and P5), in a place: a krun
//! microVM whose remote is the folder remote on a unix socket (a stand-in
//! for the route and for R2, named so), or a Cloudflare container whose
//! remote is the stand-in Worker and R2. Each life is a fresh guest from
//! the tiny image; the disk's objects and its head carry from one life to
//! the next as the platform's would.
//!
//! - `smoke`: a new disk gets files, a SQLite database and a git
//!   repository and sleeps; the next life finds them all, checked.
//! - `crash`: rounds of a writer killed with its guest mid-write; each next
//!   life replays its journal, passes `e2fsck -f -n`, SQLite's
//!   `integrity_check` and `git fsck`, and holds every row committed
//!   before the last flush the daemon said had landed.
//! - `wake`: disks of 1, 5 and 10 GB, each woken on fresh guests: the
//!   start, the mount, the pack, and the boot's files read back.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fpac_testkit::folder::{Folder, Listen, Running};
use serde_json::{json, Value};

use crate::{config::Config, krun, net};

/// Where a disk's lives run.
pub trait Place {
    /// Starts the disk's next life (a fresh guest): what the start took.
    fn start(&mut self) -> Result<Value, String>;
    /// A shell script in the guest: its exit code, stdout and stderr.
    fn sh(&self, script: &str) -> Result<(i64, String), String>;
    /// Ends the guest at once, as a crash would.
    fn kill(&mut self) -> Result<(), String>;
    fn what(&self) -> String;
    /// The disk's objects in the store: how many, how many bytes.
    fn objects(&self) -> Value;
    /// Deletes the disk's objects (bulk test data).
    fn cleanup(&mut self) -> Value;
    /// The disk's head as the platform stores it (never a guest's word)
    /// and its objects: name, bytes, age in ms.
    fn listing(&self) -> Result<(Option<String>, Vec<Stored>), String>;
    /// A checkpoint's bytes, and the SHA-256 the store holds for them.
    fn object(&self, name: &str) -> Result<(Vec<u8>, String), String>;
    /// Deletes the named objects at least `age_ms_min` old, beside no
    /// running life: {deleted, bytes, young, absent} or {refused}.
    fn delete(&mut self, names: &[String], age_ms_min: u64) -> Result<Value, String>;
}

pub struct Krun {
    image: String,
    dir: PathBuf,
    root: PathBuf,
    volume: u64,
    epoch: u64,
    life: Option<(String, Running)>,
    head: Option<fpac_remote::Head>,
    /// Leave the folder when done (a disk another run made).
    keep: bool,
}

impl Krun {
    pub fn new(image: &str, name: &str, volume: u64) -> Result<Krun, String> {
        let dir = crate::work_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("remote");
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        Ok(Krun { image: image.to_string(), dir, root, volume, epoch: 0, life: None, head: None, keep: false })
    }

}

impl Krun {
    /// A disk the folder in `root` already holds (`root/<disk>/e*/…`), its
    /// head taken from its newest checkpoint: to look inside a disk a run
    /// left behind. Nothing is deleted when it ends.
    pub fn existing(image: &str, root: &Path) -> Result<Krun, String> {
        let disk = root.join("disk");
        let mut newest: Option<(u64, u64)> = None;
        let mut epochs = 0;
        for e in std::fs::read_dir(&disk).map_err(|e| format!("{}: {e}", disk.display()))?.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Some(epoch) = name.strip_prefix('e').and_then(|x| x.parse::<u64>().ok()) else { continue };
            epochs = epochs.max(epoch);
            for o in std::fs::read_dir(e.path()).into_iter().flatten().flatten() {
                let o = o.file_name().to_string_lossy().to_string();
                if let Some(seq) = o.strip_prefix('c').and_then(|x| x.parse::<u64>().ok()) {
                    if newest.is_none_or(|n| (epoch, seq) > n) {
                        newest = Some((epoch, seq));
                    }
                }
            }
        }
        let (ce, cs) = newest.ok_or("no checkpoint in the folder")?;
        let body = std::fs::read(disk.join(format!("e{ce}/c{cs}"))).map_err(|e| e.to_string())?;
        let head = fpac_remote::Head { epoch: ce, checkpoint: Some(fpac_remote::CheckpointRef { epoch: ce, seq: cs, sum: fpac_remote::sha256(&body) }), clean: false };
        let dir = root.parent().ok_or("a root has a parent")?.to_path_buf();
        Ok(Krun { image: image.to_string(), dir, root: root.to_path_buf(), volume: 20_000 << 20, epoch: epochs, life: None, head: Some(head), keep: true })
    }
}

impl Drop for Krun {
    fn drop(&mut self) {
        let _ = self.kill();
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// `FPAC_FOLDER_OBJECT_MS`: the folder remote's delay on each object call
/// (`Folder::object_ms`), named in every result it shapes.
fn object_ms() -> u64 {
    std::env::var("FPAC_FOLDER_OBJECT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// `FPAC_FOLDER_PUT_BYTES_PER_S`: a put's rate per connection on the folder
/// remote (`Folder::put_bytes_per_s`), named in every result it shapes.
fn put_bytes_per_s() -> u64 {
    std::env::var("FPAC_FOLDER_PUT_BYTES_PER_S").ok().and_then(|v| v.parse().ok()).unwrap_or(0)
}

impl Place for Krun {
    fn start(&mut self) -> Result<Value, String> {
        self.kill()?;
        self.epoch += 1;
        let sock = self.dir.join(format!("handler-{}.sock", self.epoch));
        let _ = std::fs::remove_file(&sock);
        let listener = std::os::unix::net::UnixListener::bind(&sock).map_err(|e| e.to_string())?;
        let mut folder = Folder::new(&self.root, "disk", self.epoch);
        folder.volume_bytes = self.volume;
        folder.object_ms = object_ms();
        folder.put_bytes_per_s = put_bytes_per_s();
        *folder.head.lock().unwrap() = self.head;
        let remote = folder.start(Listen::Unix(listener));
        let name = format!("fpac-disk-{}", self.epoch);
        let _ = krun::destroy(&name);
        let t0 = Instant::now();
        let started = krun::start(&name, &self.image, &sock)?;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        self.life = Some((name, remote));
        Ok(json!({ "start_ms": ms, "engine_ready_us": started["timings"]["readyUs"] }))
    }

    fn sh(&self, script: &str) -> Result<(i64, String), String> {
        let (name, _) = self.life.as_ref().ok_or("no life")?;
        let e = krun::exec(name, &["sh", "-c", script])?;
        Ok((e.code.unwrap_or(-1), format!("{}{}", e.stdout, e.stderr)))
    }

    fn kill(&mut self) -> Result<(), String> {
        if let Some((name, remote)) = self.life.take() {
            self.head = *remote.folder.head.lock().unwrap();
            krun::destroy(&name)?;
        }
        Ok(())
    }

    fn what(&self) -> String {
        let mut delay = match object_ms() {
            0 => String::new(),
            ms => format!(", each object call delayed {ms} ms (as R2's)"),
        };
        if put_bytes_per_s() > 0 {
            delay.push_str(&format!(", puts sharing {} MB/s (the rate a Cloudflare container sustained to R2)", put_bytes_per_s() as f64 / 1e6));
        }
        format!("a krun microVM per life (2 vCPU, 6 GiB, 20 GB); the folder remote on a unix socket on this machine{delay}: a STAND-IN for the route and for R2")
    }

    fn objects(&self) -> Value {
        count_objects(&self.root)
    }

    fn cleanup(&mut self) -> Value {
        let _ = self.kill();
        let _ = std::fs::remove_dir_all(&self.root);
        json!("the folder removed")
    }

    fn listing(&self) -> Result<(Option<String>, Vec<Stored>), String> {
        // the head the folder (the platform's stand-in) keeps
        let head = match &self.life {
            Some((_, r)) => *r.folder.head.lock().unwrap(),
            None => self.head,
        };
        let disk = self.root.join("disk");
        let now = std::time::SystemTime::now();
        let mut objects = Vec::new();
        // Bounded by the folder's files: e<epoch>/<name>.
        for life in std::fs::read_dir(&disk).into_iter().flatten().flatten() {
            for o in std::fs::read_dir(life.path()).into_iter().flatten().flatten() {
                let m = o.metadata().map_err(|e| e.to_string())?;
                let age = m.modified().ok().and_then(|t| now.duration_since(t).ok()).map_or(0, |d| d.as_millis() as u64);
                objects.push((format!("{}/{}", life.file_name().to_string_lossy(), o.file_name().to_string_lossy()), m.len(), age));
            }
        }
        objects.sort();
        Ok((head.map(|h| h.encode()), objects))
    }

    fn object(&self, name: &str) -> Result<(Vec<u8>, String), String> {
        let b = std::fs::read(self.root.join("disk").join(name)).map_err(|e| format!("{name}: {e}"))?;
        let sum = hex(&fpac_remote::sha256(&b));
        Ok((b, sum))
    }

    fn delete(&mut self, names: &[String], age_ms_min: u64) -> Result<Value, String> {
        if self.life.is_some() {
            return Ok(json!({ "refused": "a life is running" }));
        }
        let (_, objects) = self.listing()?;
        let stored: std::collections::BTreeMap<&str, (u64, u64)> = objects.iter().map(|(n, b, a)| (n.as_str(), (*b, *a))).collect();
        let (mut deleted, mut bytes, mut young, mut absent) = (0u64, 0u64, Vec::new(), Vec::new());
        for n in names {
            match stored.get(n.as_str()) {
                None => absent.push(n.clone()),
                Some((_, age)) if *age < age_ms_min => young.push(n.clone()),
                Some((b, _)) => {
                    std::fs::remove_file(self.root.join("disk").join(n)).map_err(|e| format!("{n}: {e}"))?;
                    deleted += 1;
                    bytes += b;
                }
            }
        }
        Ok(json!({ "deleted": deleted, "bytes": bytes, "young": young, "absent": absent }))
    }
}

pub struct Cloudflare {
    c: Config,
    key: net::Key,
    disk: String,
    volume: u64,
    image: &'static str,
    /// The container reaches the internet (the npm image's alone: the
    /// Worker refuses any other).
    internet: bool,
}

impl Cloudflare {
    pub fn new(disk: &str, volume: u64) -> Result<Cloudflare, String> {
        let c = Config::load()?;
        let key = net::Key::load_or_make(&c.driver_key_file);
        Ok(Cloudflare { c, key, disk: disk.to_string(), volume, image: "tiny", internet: false })
    }

    /// The npm benchmark's place (C): the npm image, with the internet.
    pub fn npm(disk: &str, volume: u64) -> Result<Cloudflare, String> {
        Ok(Cloudflare { image: "npm", internet: true, ..Cloudflare::new(disk, volume)? })
    }

}

impl Cloudflare {
    fn call(&self, path: &str, body: &Value) -> Result<Value, String> {
        net::worker(self.c.host(), &self.key, "POST", path, body, Duration::from_secs(300))
    }
}

/// A cleaner's life (docs/design.md, The cleaner): a fresh guest for the
/// disk, a new epoch, and `fpac clean` against its segments' sizes from
/// the store's listing (with the disk's key, when it is sealed). Killed
/// `kill_after` in, if given.
pub fn cleaner(p: &mut dyn Place, kill_after: Option<Duration>) -> Result<Value, String> {
    let (_, objects) = p.listing()?;
    let mut sizes = String::new();
    for (name, bytes, _) in &objects {
        let seg = name.strip_prefix('e').and_then(|r| r.split_once("/s")).and_then(|(e, q)| Some((e.parse::<u64>().ok()?, q.parse::<u64>().ok()?)));
        if let Some((e, q)) = seg {
            sizes.push_str(&format!("{e} {q} {bytes}\n"));
        }
    }
    // one exec's body is the Worker's DRIVER_BODY_BYTES_MAX (64 KiB)
    if sizes.len() > 48 << 10 {
        return Err(format!("{} bytes of sizes: past what one exec carries", sizes.len()));
    }
    let start = p.start()?;
    let (code, out) = p.sh(&format!("cat > /tmp/sizes <<'EOF'\n{sizes}EOF\necho sized"))?;
    if code != 0 || !out.contains("sized") {
        return Err(format!("the sizes file: {out}"));
    }
    let run = format!("/usr/local/bin/fpac clean --sizes /tmp/sizes{}", key_arg(p)?);
    let t0 = Instant::now();
    let r = match kill_after {
        None => {
            let (code, out) = p.sh(&run)?;
            if code != 0 {
                return Err(format!("the cleaner: {out}"));
            }
            json!({ "start": start, "segments_listed": sizes.lines().count(), "out": guest_event(&out, "cleaned")?, "exec_ms": t0.elapsed().as_millis() as u64 })
        }
        Some(after) => {
            p.sh(&format!("setsid nohup {run} > /tmp/clean.log 2>&1 < /dev/null & echo started"))?;
            std::thread::sleep(after);
            let (_, log) = p.sh("cat /tmp/clean.log")?;
            json!({ "start": start, "killed_after_ms": after.as_millis() as u64, "finished_before_the_kill": log.contains("\"cleaned\""), "log": log.chars().take(2000).collect::<String>() })
        }
    };
    p.kill()?;
    Ok(r)
}

/// The delete pass, as the platform runs it: the stored head, the
/// checkpoints kept before it (each checked against the store's SHA-256,
/// the head's against the head's), `plan_deletes`, then the store's
/// delete, which checks each object's age again and refuses beside a
/// running life.
pub fn delete_pass(p: &mut dyn Place, age_ms_min: u64) -> Result<Value, String> {
    p.kill()?;
    let t0 = Instant::now();
    let (line, objects) = p.listing()?;
    let line = line.ok_or("the disk has no head")?;
    let head = fpac_store::Head::decode(&line).ok_or("a malformed head")?;
    let names: Vec<String> = objects.iter().map(|(n, _, _)| n.clone()).collect();
    let kept_ids = fpac_store::clean::kept_checkpoints(&head, &names);
    let mut kept = Vec::new();
    for (e, q) in &kept_ids {
        let (bytes, stored_sum) = p.object(&format!("e{e}/c{q}"))?;
        let sum = fpac_remote::sha256(&bytes);
        if hex(&sum) != stored_sum {
            return Err(format!("e{e}/c{q}: not the bytes the store holds"));
        }
        if let Some(h) = head.checkpoint.filter(|h| (h.epoch, h.seq) == (*e, *q)) {
            if sum != h.sum {
                return Err(format!("e{e}/c{q}: not the checkpoint the head names"));
            }
        }
        kept.push(fpac_store::clean::read_checkpoint(&bytes).map_err(|e| e.to_string())?);
    }
    let ages: Vec<(String, u64)> = objects.iter().map(|(n, _, a)| (n.clone(), *a)).collect();
    let plan = fpac_format::plan_deletes(&kept, head.epoch, &ages, age_ms_min);
    let size: std::collections::BTreeMap<&str, u64> = objects.iter().map(|(n, b, _)| (n.as_str(), *b)).collect();
    let planned_bytes: u64 = plan.iter().map(|n| size[n.as_str()]).sum();
    let (mut deleted, mut bytes, mut young, mut absent) = (0u64, 0u64, 0usize, 0usize);
    for chunk in plan.chunks(1000) {
        let d = p.delete(chunk, age_ms_min)?;
        if let Some(why) = d["refused"].as_str() {
            return Err(format!("the delete was refused: {why}"));
        }
        deleted += d["deleted"].as_u64().unwrap_or(0);
        bytes += d["bytes"].as_u64().unwrap_or(0);
        young += d["young"].as_array().map_or(0, Vec::len);
        absent += d["absent"].as_array().map_or(0, Vec::len);
    }
    let kind = |p: char| plan.iter().filter(|n| n.split_once('/').is_some_and(|(_, r)| r.starts_with(p))).count();
    Ok(json!({
        "head": line, "objects_listed": objects.len(), "kept_checkpoints": kept_ids.iter().map(|(e, q)| format!("e{e}/c{q}")).collect::<Vec<_>>(),
        "age_ms_min": age_ms_min, "planned": plan.len(), "planned_bytes": planned_bytes,
        "planned_by_kind": { "segments": kind('s'), "pages": kind('m'), "checkpoints": kind('c'), "packs": kind('p') },
        "deleted": deleted, "deleted_bytes": bytes, "young": young, "absent": absent, "ms": t0.elapsed().as_millis() as u64,
    }))
}

/// An object of a disk as the store lists it: name, bytes, age in ms.
type Stored = (String, u64, u64);

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd hex".into());
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string())).collect()
}

/// The line of a guest's output that is the JSON event `name`.
fn guest_event(out: &str, name: &str) -> Result<Value, String> {
    out.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).find(|v| v["fpac"] == name).ok_or_else(|| format!("no {name} in: {out}"))
}

impl Place for Cloudflare {
    fn start(&mut self) -> Result<Value, String> {
        let _ = net::worker(self.c.host(), &self.key, "POST", "/destroy", &json!({}), Duration::from_secs(60));
        let epoch = crate::now_ms();
        let t0 = Instant::now();
        let s = net::worker(self.c.host(), &self.key, "POST", "/start", &json!({ "disk": self.disk, "image": self.image, "epoch": epoch, "volume": self.volume, "internet": self.internet }), Duration::from_secs(300))?;
        Ok(json!({ "start_ms": t0.elapsed().as_secs_f64() * 1000.0, "ready_ms": s["ready"]["ms"], "object_colo": s["object_colo"], "version": s["version"], "image": s["image"], "internet": s["internet"] }))
    }

    fn sh(&self, script: &str) -> Result<(i64, String), String> {
        let r = net::worker(self.c.host(), &self.key, "POST", "/exec", &json!({ "argv": ["sh", "-c", script], "ms": 300_000 }), Duration::from_secs(360))?;
        Ok((r["exitCode"].as_i64().unwrap_or(-1), format!("{}{}", r["stdout"].as_str().unwrap_or(""), r["stderr"].as_str().unwrap_or(""))))
    }

    fn kill(&mut self) -> Result<(), String> {
        net::worker(self.c.host(), &self.key, "POST", "/destroy", &json!({}), Duration::from_secs(60)).map(|_| ())
    }

    fn what(&self) -> String {
        "a Cloudflare container per life (2 vCPU, 6 GiB, 20 GB); the stand-in Worker's handler on its machine and R2".into()
    }

    fn objects(&self) -> Value {
        net::worker(self.c.host(), &self.key, "POST", "/objects/usage", &json!({ "prefix": format!("disks/{}/", self.disk) }), Duration::from_secs(300)).unwrap_or(Value::Null)
    }

    fn cleanup(&mut self) -> Value {
        let _ = self.kill();
        net::worker(self.c.host(), &self.key, "POST", "/objects/delete", &json!({ "prefix": format!("disks/{}/", self.disk) }), Duration::from_secs(600)).unwrap_or(Value::Null)
    }

    fn listing(&self) -> Result<(Option<String>, Vec<Stored>), String> {
        let l = self.call("/clean/list", &json!({ "disk": self.disk }))?;
        let objects = l["objects"]
            .as_array()
            .ok_or("no objects listed")?
            .iter()
            .map(|o| Some((o[0].as_str()?.to_string(), o[1].as_u64()?, o[2].as_u64()?)))
            .collect::<Option<Vec<_>>>()
            .ok_or("a malformed listing")?;
        Ok((l["head"].as_str().map(str::to_string), objects))
    }

    fn object(&self, name: &str) -> Result<(Vec<u8>, String), String> {
        let c = self.call("/clean/checkpoint", &json!({ "disk": self.disk, "name": name }))?;
        Ok((unhex(c["hex"].as_str().ok_or_else(|| format!("{name}: absent"))?)?, c["sha256"].as_str().unwrap_or("").to_string()))
    }

    fn delete(&mut self, names: &[String], age_ms_min: u64) -> Result<Value, String> {
        self.call("/clean/delete", &json!({ "disk": self.disk, "names": names, "age_ms_min": age_ms_min }))
    }
}

/// Whether this run's disks are sealed (`FPAC_ENCRYPT` set): the test
/// data key the local config names, as hex.
pub fn sealed_key() -> Result<Option<String>, String> {
    if std::env::var_os("FPAC_ENCRYPT").is_none() {
        return Ok(None);
    }
    let c = crate::config::Config::load()?;
    let f = c.test_data_key_file.ok_or("FPAC_ENCRYPT is set and the local config names no test_data_key_file")?;
    let hex = std::fs::read_to_string(&f).map_err(|e| format!("the test key file: {e}"))?.trim().to_string();
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        return Err("the test key file holds no key (64 lowercase hex digits)".into());
    }
    Ok(Some(hex))
}

/// Starts the daemon and waits for its mount: its events, by name, in ms.
pub fn mount(p: &dyn Place) -> Result<Value, String> {
    mount_with(p, "")
}

/// The daemon's key argument for this run (` --key-file …`, the test key
/// put in the guest's tmpfs, mode 600), or nothing when the disk is plain.
fn key_arg(p: &dyn Place) -> Result<String, String> {
    let Some(hex) = sealed_key()? else { return Ok(String::new()) };
    let (code, out) = p.sh(&format!("mkdir -p /run/fpac && (umask 077; printf '%s\\n' {hex} > /run/fpac/key) && echo keyed"))?;
    if code != 0 || !out.contains("keyed") {
        return Err(format!("the key file: {out}"));
    }
    Ok(" --key-file /run/fpac/key".into())
}

/// `fpac mount` with more arguments (`--cache-max <bytes>`, say).
pub fn mount_with(p: &dyn Place, extra: &str) -> Result<Value, String> {
    let extra = format!("{extra}{}", key_arg(p)?);
    let (code, out) = p.sh(&format!("mkdir -p /run/fpac && setsid nohup /usr/local/bin/fpac mount --dir /data {extra} > /run/fpac/log 2>&1 < /dev/null & echo started"))?;
    if code != 0 || !out.contains("started") {
        return Err(format!("the daemon did not start: {out}"));
    }
    let t0 = Instant::now();
    // Bounded: 120 s.
    while t0.elapsed() < Duration::from_secs(120) {
        let (_, log) = p.sh("cat /run/fpac/log")?;
        if log.contains("\"fpac\":\"mounted\"") {
            return Ok(events(&log));
        }
        if log.contains("\"fpac\":\"error\"") {
            return Err(format!("the daemon failed: {log}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let (_, log) = p.sh("cat /run/fpac/log")?;
    Err(format!("no mount in 120 s: {log}"))
}

fn events(log: &str) -> Value {
    let mut m = serde_json::Map::new();
    for line in log.lines() {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if let Some(name) = v["fpac"].as_str() {
                m.entry(name.to_string()).or_insert(v.clone());
            }
        }
    }
    Value::Object(m)
}

/// A sleep: SIGTERM, then the daemon's last flush; its events.
pub fn sleep(p: &dyn Place) -> Result<Value, String> {
    p.sh("kill -TERM $(cat /proc/[0-9]*/stat 2>/dev/null | awk '$2==\"(fpac)\" {print $1}')")?;
    let t0 = Instant::now();
    // Bounded: 600 s (a big disk's last flush).
    while t0.elapsed() < Duration::from_secs(600) {
        let (_, log) = p.sh("cat /run/fpac/log")?;
        if log.contains("\"fpac\":\"exit\"") {
            let mut e = events(&log);
            e["sleep_ms"] = json!(t0.elapsed().as_millis() as u64);
            if log.contains("retry") {
                e["retries"] = json!(log.lines().filter(|l| l.contains("retry")).take(10).collect::<Vec<_>>());
            }
            return Ok(e);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let (_, log) = p.sh("tail -n 30 /run/fpac/log")?;
    Err(format!("no exit in 600 s after SIGTERM: {log}"))
}

pub fn status(p: &dyn Place) -> Value {
    p.sh("cat /run/fpac/status.json").ok().and_then(|(_, s)| serde_json::from_str(s.trim()).ok()).unwrap_or(Value::Null)
}

const WRITE_A: &str = r#"set -e
cd /data
mkdir -p files
for i in $(seq 1 100); do head -c $((4096 + i * 997)) /dev/urandom > files/f$i; done
sha256sum files/* > /data/files.sha256
sqlite3 db.sqlite "pragma journal_mode=wal; create table t(id integer primary key, v text); with recursive c(x) as (select 1 union all select x+1 from c where x < 5000) insert into t(v) select hex(randomblob(64)) from c;"
git init -q repo && cd repo && git config user.email t@t && git config user.name t && git config core.fsync all
for i in $(seq 1 50); do head -c 2000 /dev/urandom | base64 > file$i; done
git add . && git commit -qm one && git rev-parse HEAD > /data/repo.head
sync
echo written"#;

const CHECK_A: &str = r#"set -e
cd /data
sha256sum -c --quiet files.sha256 && echo files-ok
test "$(sqlite3 db.sqlite 'pragma integrity_check')" = ok && echo sqlite-ok
test "$(sqlite3 db.sqlite 'select count(*) from t')" = 5000 && echo rows-ok
echo "journal_mode: $(sqlite3 db.sqlite 'pragma journal_mode')"
git -C repo fsck --strict --no-progress && echo git-ok
test "$(git -C repo rev-parse HEAD)" = "$(cat repo.head)" && echo head-ok
grep ' /data ' /proc/self/mountinfo"#;

/// A new disk written, put to sleep, and found whole by the next life.
pub fn smoke(p: &mut dyn Place) -> Result<Value, String> {
    let st1 = p.start()?;
    let m1 = mount(p)?;
    let (code, out) = p.sh(WRITE_A)?;
    if code != 0 {
        return Err(format!("writing: {out}"));
    }
    let s1 = sleep(p)?;
    p.kill()?;
    let st2 = p.start()?;
    let m2 = mount(p)?;
    let (code, check) = p.sh(CHECK_A)?;
    let s2 = sleep(p)?;
    p.kill()?;
    Ok(json!({
        "what": p.what(),
        "life1": { "start": st1, "mount": m1, "sleep": s1 }, "life2": { "start": st2, "mount": m2, "check": check, "check_code": code, "sleep": s2 },
    }))
}

fn count_objects(root: &Path) -> Value {
    let mut n = 0u64;
    let mut bytes = 0u64;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    // Bounded by the folder's files.
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if let Ok(m) = e.metadata() {
                if m.is_dir() {
                    stack.push(e.path());
                } else {
                    n += 1;
                    bytes += m.len();
                }
            }
        }
    }
    json!({ "objects": n, "bytes": bytes })
}

/// The writer of the crash test: rows committed one at a time with
/// synchronous=FULL, each logged with its commit time after the commit
/// returns; a file and a git commit every 20 rows.
///
/// A file only written, never synced, may be lost by a crash within the
/// loss window, as on any disk: `git init` syncs nothing, and git syncs
/// no loose object by default. So the repository is made with
/// `core.fsync=all` and synced before the writer says it is ready, and the
/// kill is timed from then.
const WRITER: &str = r#"cd /data
sqlite3 db.sqlite "pragma journal_mode=wal; create table if not exists w(id integer primary key, at integer)" > /dev/null
start=$(sqlite3 db.sqlite "select coalesce(max(id),0) from w")
mkdir -p repo && cd repo && { [ -d .git ] || { git init -q . && git config user.email t@t && git config user.name t && git config core.fsync all; }; } && cd ..
sync
echo ready > /tmp/writer.ready
i=$start
while true; do
  i=$((i+1))
  sqlite3 db.sqlite "pragma synchronous=FULL; insert into w(id, at) values($i, $(date +%s%3N))" || break
  echo "$i $(date +%s%3N)" >> /tmp/acked.log
  if [ $((i % 20)) -eq 0 ]; then
    head -c 30000 /dev/urandom > repo/f$((i % 200))
    git -C repo add -A && git -C repo commit -qm "row $i" || true
  fi
done"#;

const VERIFY: &str = r#"set -e
dev=$(awk '$2=="/data" {print $1}' /proc/mounts)
umount /data
e2fsck -f -n $dev > /tmp/fsck.log 2>&1 && echo fsck-ok || { echo fsck-failed; cat /tmp/fsck.log; }
mount -t ext4 -o noatime,commit=1,discard $dev /data
test "$(sqlite3 /data/db.sqlite 'pragma integrity_check')" = ok && echo sqlite-ok
echo "max: $(sqlite3 /data/db.sqlite 'select coalesce(max(id),0) from w')"
echo "count: $(sqlite3 /data/db.sqlite 'select count(*) from w')"
if [ -d /data/repo/.git ]; then git -C /data/repo fsck --no-progress > /tmp/gitfsck.log 2>&1 && echo git-ok || { echo git-failed; tail -5 /tmp/gitfsck.log; }; else echo git-ok; fi"#;

/// Life 1 of the cleaner's run: 512 MiB in 4 MiB files.
const WRITE_BIG: &str = r#"set -e
mkdir -p /data/big
for i in $(seq 1 128); do head -c 4194304 /dev/urandom > /data/big/b$i; done
sync
echo written"#;

/// Life 2: three of every four 256 KiB of each file written again in
/// place, so each of life 1's segments keeps about a quarter of its
/// extents live: what compaction moves. (Files written again whole leave
/// segments all dead or all live, and nothing to move.)
const REWRITE_BIG: &str = r#"set -e
for i in $(seq 1 128); do
  for k in $(seq 1 15); do
    [ $((k % 4)) -eq 0 ] || dd if=/dev/urandom of=/data/big/b$i bs=256K seek=$k count=1 conv=notrunc status=none
  done
done
sha256sum /data/big/* > /data/big.sha256
sync
echo rewritten"#;

/// Everything the cleaner's run wrote, checked, then ext4 itself.
const CHECK_CLEAN: &str = r#"cd /data
sha256sum -c --quiet big.sha256 && echo big-ok
sha256sum -c --quiet files.sha256 && echo files-ok
test "$(sqlite3 db.sqlite 'pragma integrity_check')" = ok && echo sqlite-ok
test "$(sqlite3 db.sqlite 'select count(*) from t')" = 5000 && echo rows-ok
git -C repo fsck --strict --no-progress && echo git-ok
test "$(git -C repo rev-parse HEAD)" = "$(cat repo.head)" && echo head-ok
cd /
dev=$(awk '$2=="/data" {print $1}' /proc/mounts)
umount /data
e2fsck -f -n $dev > /tmp/fsck.log 2>&1 && echo fsck-ok || { echo fsck-failed; tail -5 /tmp/fsck.log; }
mount -t ext4 -o noatime,commit=1,discard $dev /data"#;
const CHECK_CLEAN_OK: [&str; 7] = ["big-ok", "files-ok", "sqlite-ok", "rows-ok", "git-ok", "head-ok", "fsck-ok"];

/// The cleaner on Cloudflare (docs/design.md, The cleaner): a disk with
/// garbage, a cleaner killed partway and the disk checked, a cleaner to
/// its end, two lives so the kept checkpoints move past the old segments,
/// the delete pass, and the disk checked again; the store's objects
/// before and after each.
pub fn clean(p: &mut dyn Place, kill_after: Duration) -> Result<Value, String> {
    let life = |p: &mut dyn Place, script: &str, ok: &[&str]| -> Result<Value, String> {
        let st = p.start()?;
        let m = mount(p)?;
        let (code, out) = p.sh(script)?;
        let s = sleep(p)?;
        p.kill()?;
        if let Some(missing) = ok.iter().find(|w| !out.contains(*w)) {
            return Err(format!("{missing} missing (exit {code}): {out}"));
        }
        Ok(json!({ "start": st, "opened": m["opened"], "mounted_ms": m["mounted"]["ms"], "out": out.chars().take(1000).collect::<String>(), "sleep_ms": s["sleep_ms"] }))
    };
    let mut r = json!({ "what": p.what() });
    r["life1"] = life(p, &format!("{WRITE_A}\n{WRITE_BIG}"), &["written"])?;
    r["life2"] = life(p, REWRITE_BIG, &["rewritten"])?;
    r["objects_before"] = p.objects();
    r["cleaner_killed"] = cleaner(p, Some(kill_after))?;
    r["check_after_the_kill"] = life(p, CHECK_CLEAN, &CHECK_CLEAN_OK)?;
    r["objects_after_the_kill"] = p.objects();
    r["cleaner"] = cleaner(p, None)?;
    r["objects_after_compaction"] = p.objects();
    for i in 1..=2 {
        r[format!("life_after_{i}")] = life(p, &format!("echo {i} > /data/note && sync && echo noted"), &["noted"])?;
    }
    r["objects_before_deletes"] = p.objects();
    r["deletes"] = delete_pass(p, 0)?;
    r["objects_after_deletes"] = p.objects();
    r["check_after_deletes"] = life(p, CHECK_CLEAN, &CHECK_CLEAN_OK)?;
    Ok(r)
}

/// The free room, in bytes, of the guest's disk that holds the cache.
fn guest_free(p: &dyn Place) -> Result<u64, String> {
    let (_, out) = p.sh("df -B1 --output=avail /var/lib/fpac | tail -n 1")?;
    out.trim().parse().map_err(|e| format!("df: {e}: {out}"))
}

/// Giving room back (docs/design.md, The cache): the cache full at its
/// bound (`ceiling` bytes, `--cache-max`) with every extent shipped, then
/// a writer beside the daemon takes all but a GiB of its disk's free room
/// and writes 2 GiB more, while a writer on the disk syncs 256 MiB of new
/// files. The writer beside it must succeed (only the cache's giving room
/// back makes that room), the daemon must answer no write with an error,
/// and the next life must find every file the disk took.
pub fn room(p: &mut dyn Place, ceiling: u64) -> Result<Value, String> {
    let st = p.start()?;
    let m = mount_with(p, &format!("--cache-max {ceiling}"))?;
    let files = ceiling / (64 << 20);
    let t0 = Instant::now();
    // in the background: past the unshipped budget it goes at the
    // shipping's pace, longer than one exec may take
    let (code, out) = p.sh(&format!("setsid nohup sh -c 'set -e; mkdir -p /data/fill; for i in $(seq 1 {files}); do head -c 67108864 /dev/urandom > /data/fill/f$i; done; sync; sha256sum /data/fill/* > /data/fill.sha256; sync; echo filled > /tmp/fill.done' > /tmp/fill.log 2>&1 < /dev/null & echo started"))?;
    if code != 0 || !out.contains("started") {
        return Err(format!("the fill: {out}"));
    }
    // Bounded: 1800 s.
    loop {
        let (_, d) = p.sh("cat /tmp/fill.done 2>/dev/null; cat /tmp/fill.log")?;
        if d.contains("filled") {
            break;
        }
        if t0.elapsed() > Duration::from_secs(1800) {
            return Err(format!("the fill: not done in 1800 s: {d} {}", status(p)));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    let fill_ms = t0.elapsed().as_millis() as u64;
    // Bounded: 900 s. The cache full of shipped (clean) extents.
    let full = loop {
        let s = status(p);
        if s["unshipped_extents"] == 0 && s["dirty_extents"] == 0 {
            break s;
        }
        if t0.elapsed() > Duration::from_secs(900) {
            return Err(format!("not shipped in 900 s: {s}"));
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    let free_full = guest_free(p)?;
    let (code, out) = p.sh("setsid nohup sh -c 'set -e; mkdir -p /data/during; for i in $(seq 1 64); do head -c 4194304 /dev/urandom > /data/during/d$i; sync; done; sha256sum /data/during/* > /data/during.sha256; sync; echo done > /tmp/during.done' > /tmp/during.log 2>&1 < /dev/null & echo started")?;
    if code != 0 || !out.contains("started") {
        return Err(format!("the writer on the disk: {out}"));
    }
    let ballast = free_full.saturating_sub(1 << 30);
    let t1 = Instant::now();
    let (local_code, local) = p.sh(&format!("fallocate -l {ballast} /var/lib/ballast && dd if=/dev/urandom of=/var/lib/local.fill bs=1M count=2048 conv=fsync status=none && echo local-ok; df -B1 --output=avail /var/lib/fpac | tail -n 1"))?;
    let local_ms = t1.elapsed().as_millis() as u64;
    // Bounded: 300 s.
    let during = loop {
        let (_, d) = p.sh("cat /tmp/during.done 2>/dev/null; cat /tmp/during.log")?;
        if d.contains("done") || t1.elapsed() > Duration::from_secs(300) {
            break d;
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    let after = status(p);
    let (_, log) = p.sh("grep cache-room /run/fpac/log | head -n 20")?;
    p.sh("rm -f /var/lib/ballast /var/lib/local.fill")?;
    let s1 = sleep(p)?;
    p.kill()?;
    let st2 = p.start()?;
    let m2 = mount(p)?;
    let (_, check) = p.sh("sha256sum -c --quiet /data/fill.sha256 && echo fill-ok; sha256sum -c --quiet /data/during.sha256 && echo during-ok")?;
    let s2 = sleep(p)?;
    p.kill()?;
    let ok = local.contains("local-ok") && during.contains("done") && check.contains("fill-ok") && check.contains("during-ok") && after["write_errors"] == 0 && after["fenced"] == false;
    Ok(json!({
        "what": p.what(), "ceiling_bytes": ceiling, "ok": ok,
        "life1": { "start": st, "mount": m, "fill_bytes": files << 26, "fill_ms": fill_ms, "full": full, "free_when_full_bytes": free_full,
            "local": { "ballast_bytes": ballast, "then_written_bytes": 2u64 << 30, "exit": local_code, "out": local, "ms": local_ms },
            "during": during, "after": after, "room_events": log, "sleep": s1 },
        "life2": { "start": st2, "mount": m2, "check": check, "sleep": s2 },
    }))
}

/// A full disk (docs/design.md, The cache): a life syncs 256 MiB of files
/// (each acknowledged: its fsync returned; some still unshipped), then a
/// writer beside the daemon takes every byte left on the cache's disk,
/// past the cache's floor, and the life writes new data. That write must
/// fail loudly: the program sees "No space left on device", and the
/// daemon counts and logs a typed no-space error, unfenced. Then the room
/// comes back, the sleep ships everything, and the next life finds every
/// acknowledged file.
pub fn full(p: &mut dyn Place) -> Result<Value, String> {
    let st = p.start()?;
    let m = mount(p)?;
    let free = guest_free(p)?;
    // a computer's own disk only, never one this run should not fill
    if free > 25 << 30 {
        return Err(format!("the guest's disk has {free} bytes free: not a computer's 20 GB disk, so not filled"));
    }
    let (code, out) = p.sh("set -e; mkdir -p /data/acked; for i in $(seq 1 64); do head -c 4194304 /dev/urandom > /data/acked/a$i; sync /data/acked/a$i; done; sha256sum /data/acked/* > /data/acked.sha256; sync; echo acked")?;
    if code != 0 || !out.contains("acked") {
        return Err(format!("the acknowledged files: {out}"));
    }
    let acked = status(p);
    // every byte: what an unprivileged writer may take, then the root's
    // reserve, until nothing is left (three rounds, two seconds apart, so
    // whatever the cache gives back is taken too)
    let (_, fill) = p.sh("fallocate -l $(df -B1 --output=avail /var/lib/fpac | tail -n 1) /var/lib/ballast; for i in 1 2 3; do dd if=/dev/zero of=/var/lib/ballast-$i bs=1M status=none 2>&1 | tail -n 1; sleep 2; done; echo free-blocks $(stat -f -c %f /var/lib/fpac)")?;
    let (_, new) = p.sh("dd if=/dev/urandom of=/data/new bs=1M count=64 conv=fsync status=none 2>&1; echo exit=$?")?;
    std::thread::sleep(Duration::from_secs(2));
    let after = status(p);
    let (_, log) = p.sh("grep -E 'no-space|\"error\"' /run/fpac/log | head -n 5")?;
    let (_, reread) = p.sh("sha256sum -c --quiet /data/acked.sha256 && echo acked-readable")?;
    p.sh("rm -f /var/lib/ballast /var/lib/ballast-*")?;
    let s1 = sleep(p)?;
    p.kill()?;
    let st2 = p.start()?;
    let m2 = mount(p)?;
    let (_, check) = p.sh("sha256sum -c --quiet /data/acked.sha256 && echo acked-ok")?;
    let s2 = sleep(p)?;
    p.kill()?;
    let loud = !new.contains("exit=0") && new.contains("No space left on device");
    let typed = after["no_space_errors"].as_u64().unwrap_or(0) > 0 && log.contains("no-space");
    let ok = loud && typed && after["fenced"] == false && s1["flushed"]["error"].is_null() && check.contains("acked-ok");
    Ok(json!({
        "what": p.what(), "ok": ok, "failed_loudly": loud, "typed": typed,
        "life1": { "start": st, "mount": m, "free_bytes": free, "acked": acked, "fill": fill, "new_write": new, "after": after, "log": log, "reread_while_full": reread, "sleep": s1 },
        "life2": { "start": st2, "mount": m2, "check": check, "sleep": s2 },
    }))
}

/// Rounds of a guest killed mid-write, each next life checked.
pub fn crash(p: &mut dyn Place, rounds: u32) -> Result<Value, String> {
    let mut required: u64 = 0;
    let mut out = Vec::new();
    let mut seed = 0x2545_f491u64;
    let mut failures = 0;
    for round in 0..=rounds {
        p.start()?;
        let m = mount(p)?;
        // the last round only checks
        let (code, verify) = if round == 0 { (0, "new disk".to_string()) } else { p.sh(VERIFY)? };
        let max: u64 = verify.lines().find_map(|x| x.strip_prefix("max: ")).and_then(|x| x.trim().parse().ok()).unwrap_or(0);
        let count: u64 = verify.lines().find_map(|x| x.strip_prefix("count: ")).and_then(|x| x.trim().parse().ok()).unwrap_or(0);
        let ok = round == 0 || (code == 0 && verify.contains("fsck-ok") && verify.contains("sqlite-ok") && verify.contains("git-ok") && max >= required && count == max);
        if !ok {
            failures += 1;
        }
        let mut row = json!({ "round": round, "mount_ms": m["mounted"]["ms"], "opened_ms": m["opened"]["ms"], "loop_ms": m["loop"]["ms"], "replayed": m["opened"]["replayed"], "verify_ok": ok, "rows_required": required, "rows_found": max, "verify": if ok { Value::Null } else { json!(verify) } });
        if round == rounds {
            out.push(row);
            p.kill()?;
            break;
        }
        p.sh(&format!("rm -f /tmp/acked.log /tmp/writer.ready; setsid nohup sh -c '{}' > /tmp/writer.log 2>&1 < /dev/null & echo started", WRITER.replace('\'', "'\\''")))?;
        // Bounded: 30 s for the writer's setup.
        let ready = Instant::now();
        while ready.elapsed() < Duration::from_secs(30) && !p.sh("test -f /tmp/writer.ready")?.0.eq(&0) {
            std::thread::sleep(Duration::from_millis(50));
        }
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let after = 2500 + seed % 4000;
        std::thread::sleep(Duration::from_millis(after));
        // what the daemon says has landed, and what the writer committed
        let (_, state) = p.sh("cat /run/fpac/status.json; echo; tail -n 400 /tmp/acked.log")?;
        p.kill()?;
        let status: Value = state.lines().next().and_then(|x| serde_json::from_str(x).ok()).unwrap_or(Value::Null);
        let durable = status["durable_ms"].as_u64().unwrap_or(0);
        let acked: Vec<(u64, u64)> = state.lines().skip(1).filter_map(|x| x.split_once(' ')).filter_map(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))).collect();
        required = acked.iter().filter(|(_, at)| *at < durable).map(|(i, _)| *i).max().unwrap_or(required).max(required);
        row["killed_after_ms"] = json!(after);
        row["rows_acked"] = json!(acked.last().map(|x| x.0).unwrap_or(0));
        row["rows_durable_at_kill"] = json!(required);
        row["cuts"] = status["cuts"].clone();
        out.push(row);
    }
    Ok(json!({ "what": p.what(), "rounds": rounds, "failures": failures, "rounds_detail": out }))
}

/// The boot's files: 200 small files the next wake reads first, about
/// 24 MB, written and read back within the boot's first minute so the
/// pack holds them.
const BOOT_SET: &str = r#"set -e
mkdir -p /data/boot && cd /data/boot
for i in $(seq 1 200); do head -c $((60000 + i * 1000)) /dev/urandom > b$i; done
sha256sum b* > /data/boot.sha256
cat b* > /dev/null
echo boot-written"#;

/// Reads the boot's files back and checks them: its time in ms.
const BOOT_READ: &str = r#"set -e
t0=$(date +%s%N)
cd /data/boot && sha256sum -c --quiet /data/boot.sha256
t1=$(date +%s%N)
echo "boot_read_ms: $(( (t1 - t0) / 1000000 ))""#;

/// Disks of each size: written once (the boot's files, then `gb` of
/// others), put to sleep, then woken `wakes` times on fresh guests.
/// `asleep_s`: how long the disk sleeps, its guest gone, before the first
/// wake (the later wakes come at once), so the store's caches have time
/// to forget it.
pub fn wake(p: &mut dyn Place, gb: u64, wakes: u32, asleep_s: u64, mount_args: &str) -> Result<Value, String> {
    let st = p.start()?;
    let m = mount_with(p, mount_args)?;
    let (code, out) = p.sh(BOOT_SET)?;
    if code != 0 {
        return Err(format!("the boot's files: {out}"));
    }
    let t = Instant::now();
    // the rest of the disk: 1 GiB files of noise (urandom is fast enough)
    let (code, out) = sh_long(p, &format!("set -e; mkdir -p /data/fill; for i in $(seq 1 {gb}); do head -c 1073741824 /dev/urandom > /data/fill/g$i; done; sync; df -B1 /data | tail -1"), "", "fill")?;
    if code != 0 {
        return Err(format!("the fill: {out}"));
    }
    let fill_ms = t.elapsed().as_millis() as u64;
    let s = sleep(p)?;
    p.kill()?;
    // Bounded by the caller's flag.
    std::thread::sleep(Duration::from_secs(asleep_s));
    let mut lives = Vec::new();
    for _ in 0..wakes {
        let t0 = Instant::now();
        let st = p.start()?;
        let m = mount_with(p, mount_args)?;
        let (code, out) = p.sh(BOOT_READ)?;
        let wake_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let boot_read: u64 = out.lines().find_map(|x| x.strip_prefix("boot_read_ms: ")).and_then(|x| x.trim().parse().ok()).unwrap_or(0);
        // the daemon writes its status each second
        std::thread::sleep(Duration::from_millis(1200));
        let status = status(p);
        let far = p.sh("t0=$(date +%s%N); head -c 1048576 /data/fill/g1 > /dev/null; t1=$(date +%s%N); echo $(( (t1 - t0) / 1000000 ))").map(|(_, o)| o.trim().to_string()).unwrap_or_default();
        let sl = sleep(p)?;
        p.kill()?;
        lives.push(json!({
            "start": st, "mount_events_ms": { "opened": m["opened"]["ms"], "fuse": m["fuse"]["ms"], "loop": m["loop"]["ms"], "mounted": m["mounted"]["ms"], "pack": m["pack"]["ms"] },
            "opened": m["opened"], "mounted": m["mounted"], "pack_header": m["pack-header"],
            "pack": m["pack"], "boot_read_ok": code == 0, "boot_read_ms": boot_read, "misses_after_boot_read": status["misses"],
            "wake_to_boot_read_ms": wake_ms, "far_1mib_read_ms": far, "sleep_ms": sl["sleep_ms"],
        }));
    }
    Ok(json!({ "what": p.what(), "gb": gb, "asleep_s_before_the_first_wake": asleep_s, "first": { "start": st, "mount": m, "fill_ms": fill_ms, "fill_df": out.trim(), "sleep": s }, "wakes": lives }))
}

/// A build's target directory, as a job: `$T/target` filled with
/// $BIG files of 4 MiB (from a random blob in memory, so the source is not
/// the bottleneck) and 10,000 small files of 16 KiB, synced, the page cache
/// dropped, and every file read back. Each step `name ms`.
const BUILD: &str = r#"set -e
now() { date +%s%N; }
ms() { echo $(( ($(now) - $1) / 1000000 )); }
drop() { sync; echo 3 > /proc/sys/vm/drop_caches; }
[ -f /dev/shm/blob ] || head -c 67108864 /dev/urandom > /dev/shm/blob
rm -rf $T/target; mkdir -p $T/target
t=$(now)
i=0; while [ $i -lt $BIG ]; do d=$T/target/deps$((i / 64)); mkdir -p $d; dd if=/dev/shm/blob of=$d/lib$i.rlib bs=4M count=1 skip=$((i % 16)) status=none; i=$((i + 1)); done
i=0; while [ $i -lt 100 ]; do d=$T/target/incremental/s$i; mkdir -p $d; j=0; while [ $j -lt 100 ]; do dd if=/dev/shm/blob of=$d/w$j.o bs=16K count=1 skip=$(( (i * 100 + j) % 4096 )) status=none; j=$((j + 1)); done; i=$((i + 1)); done
echo "write_ms $(ms $t)"
t=$(now); sync; echo "sync_ms $(ms $t)"
echo 3 > /proc/sys/vm/drop_caches
t=$(now); find $T/target -type f -exec cat {} + > /dev/null; echo "read_back_ms $(ms $t)"
echo "bytes $(du -sb $T/target | cut -f1)""#;

/// A job longer than one exec may wait (the engine's socket gives up at
/// 600 s): the script written to the guest, run detached, its exit code
/// polled every 5 s, at most three hours; then its output.
fn sh_long(p: &dyn Place, script: &str, env: &str, tag: &str) -> Result<(i64, String), String> {
    p.sh(&format!("cat > /tmp/{tag}.sh <<'FPAC_EOF'\n{script}\nFPAC_EOF\nrm -f /tmp/{tag}.out /tmp/{tag}.exit; echo written"))?;
    let (code, out) = p.sh(&format!("setsid nohup sh -c \"{env} sh /tmp/{tag}.sh > /tmp/{tag}.out 2>&1; echo \\$? > /tmp/{tag}.exit\" > /dev/null 2>&1 < /dev/null & echo started"))?;
    if code != 0 || !out.contains("started") {
        return Err(format!("the job did not start: {out}"));
    }
    // Bounded: three hours of polls.
    for _ in 0..2160 {
        std::thread::sleep(Duration::from_secs(5));
        if let Ok((0, exit)) = p.sh(&format!("cat /tmp/{tag}.exit")) {
            let code: i64 = exit.trim().parse().unwrap_or(-1);
            let (_, out) = p.sh(&format!("cat /tmp/{tag}.out"))?;
            return Ok((code, out));
        }
    }
    Err(format!("{tag}: no end in three hours"))
}

/// Samples the daemon's status each second into /tmp/status.log.
const SAMPLER: &str = "rm -f /tmp/status.log; setsid nohup sh -c 'while true; do cat /run/fpac/status.json 2>/dev/null; echo; sleep 1; done' > /tmp/status.log 2>&1 < /dev/null & echo $! > /tmp/sampler.pid; echo started";

/// The thrash test (docs/costs.md): a build's target directory larger than
/// the cache. The job on the container's own disk; on FPAC with the cache
/// bounded at `cache_bytes` (or not), its status sampled each second; a
/// sleep; then a wake and the same files read back cold.
pub fn thrash(p: &mut dyn Place, big_files: u64, cache_bytes: Option<u64>, wake: bool) -> Result<Value, String> {
    let st = p.start()?;
    let (code, out) = sh_long(p, BUILD, &format!("T=/var/tmp/local BIG={big_files}"), "local")?;
    let local = if code == 0 { rows(&out) } else { json!({ "error": out }) };
    p.sh("rm -rf /var/tmp/local/target")?;
    let extra = cache_bytes.map(|b| format!("--cache-max {b}")).unwrap_or_default();
    let m = mount_with(p, &extra)?;
    p.sh(SAMPLER)?;
    let (code, out) = sh_long(p, BUILD, &format!("T=/data BIG={big_files}"), "fpac")?;
    let fpac = if code == 0 { rows(&out) } else { json!({ "error": out }) };
    let after_job = status(p);
    let (_, kernel) = p.sh("dmesg | grep -iE 'ext4|i/o error|loop0' | tail -n 20")?;
    let _ = p.sh("kill $(cat /tmp/sampler.pid) 2>/dev/null; true");
    let (_, log) = p.sh("cat /tmp/status.log")?;
    let timeline: Vec<Value> = log.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).map(|s| json!({ "up_ms": s["up_ms"], "dirty": s["dirty_extents"], "shipping": s["shipping_segments"], "shipped": s["bytes_shipped"], "full_waits": s["full_waits"], "misses": s["misses"], "evictions": s["evictions"], "cached": s["cached_extents"], "write_errors": s["write_errors"] })).collect();
    let sl = sleep(p)?;
    p.kill()?;
    if !wake {
        return Ok(json!({ "what": p.what(), "big_files": big_files, "cache_max_bytes": cache_bytes, "local": local, "fpac": { "start": st, "mount": m["mounted"], "job": fpac, "status_after_job": after_job, "kernel": kernel, "timeline": timeline, "sleep": sl } }));
    }
    // a wake: the cache died with the container; the same files read cold
    let st2 = p.start()?;
    let m2 = mount_with(p, &extra)?;
    let (code, out) = sh_long(p, "now() { date +%s%N; }; t=$(now); find /data/target -type f -exec cat {} + > /dev/null; echo \"read_back_after_a_wake_ms $(( ($(now) - t) / 1000000 ))\"", "", "cold")?;
    let cold = if code == 0 { rows(&out) } else { json!({ "error": out }) };
    let after_wake = status(p);
    let sl2 = sleep(p)?;
    p.kill()?;
    Ok(json!({
        "what": p.what(), "big_files": big_files, "cache_max_bytes": cache_bytes,
        "local": local,
        "fpac": { "start": st, "mount": m["mounted"], "job": fpac, "status_after_job": after_job, "kernel": kernel, "timeline": timeline, "sleep": sl },
        "after_a_wake": { "start": st2, "mount": m2["mounted"], "job": cold, "status": after_wake, "sleep": sl2 },
    }))
}

/// The git source the clone reads: 7,260 files in 121 directories (react's
/// size), two commits, the second touching every file; made once per
/// guest on its own disk.
const GIT_SOURCE: &str = r#"set -e
rm -rf /var/tmp/gitsrc && mkdir -p /var/tmp/gitsrc && cd /var/tmp/gitsrc && git init -q . && git config user.email t@t && git config user.name t
awk 'BEGIN { srand(1); for (d = 1; d <= 121; d++) { system("mkdir -p d" d); for (f = 1; f <= 60; f++) { file = "d" d "/f" f; n = 20 + (d * f) % 80; for (l = 0; l < n; l++) print rand() " " rand() " " rand() > file; close(file) } } }'
git add -A && git commit -qm one
awk 'BEGIN { for (d = 1; d <= 121; d++) for (f = 1; f <= 60; f++) { file = "d" d "/f" f; print "two" >> file; close(file) } }'
git commit -qam two
echo source-ready"#;

/// The tools' rows in `$T`: git, SQLite; each `name ms`.
const TOOLS: &str = r#"set -e
cd $T
now() { date +%s%N; }
ms() { echo $(( ($(now) - $1) / 1000000 )); }
drop() { sync; echo 3 > /proc/sys/vm/drop_caches; }
rm -rf clone b.sqlite*
t=$(now); git clone -q --no-local /var/tmp/gitsrc clone; echo "git_clone_7260_files $(ms $t)"
t=$(now); git -C clone status --porcelain > /dev/null; echo "git_status_warm $(ms $t)"
drop; t=$(now); git -C clone status --porcelain > /dev/null; echo "git_status_cold $(ms $t)"
t=$(now); git -C clone checkout -q HEAD~1; echo "git_checkout_7260_changed $(ms $t)"
drop; t=$(now); grep -rl zzzz clone > /dev/null || true; echo "grep_rl_cold $(ms $t)"
t=$(now); grep -rl zzzz clone > /dev/null || true; echo "grep_rl_warm $(ms $t)"
drop; t=$(now); find clone | wc -l > /dev/null; echo "find_cold $(ms $t)"
t=$(now); rm -rf clone; echo "rm_rf_checkout $(ms $t)"
sqlite3 b.sqlite "pragma journal_mode=wal; create table f(id integer primary key, v text); create table n(id integer primary key, v text); create table big(id integer primary key, v text);" > /dev/null
{ echo "pragma synchronous=FULL;"; awk 'BEGIN { q = sprintf("%c", 39); for (i = 1; i <= 10000; i++) print "insert into f(v) values(" q "row " i q ");" }'; } > /tmp/full.sql
{ echo "pragma synchronous=NORMAL;"; awk 'BEGIN { q = sprintf("%c", 39); for (i = 1; i <= 10000; i++) print "insert into n(v) values(" q "row " i q ");" }'; } > /tmp/normal.sql
t=$(now); sqlite3 b.sqlite < /tmp/full.sql; echo "sqlite_10k_txns_full $(ms $t)"
t=$(now); sqlite3 b.sqlite < /tmp/normal.sql; echo "sqlite_10k_txns_normal $(ms $t)"
t=$(now); sqlite3 b.sqlite "with recursive c(x) as (select 1 union all select x+1 from c where x < 1000000) insert into big(v) select hex(randomblob(32)) from c;"; echo "sqlite_1m_rows_one_txn $(ms $t)"
t=$(now); sqlite3 b.sqlite "select sum(length(v)) from (with recursive c(x) as (select 1 union all select x+1 from c where x < 100000) select (select v from big where id = 1 + abs(random()) % 1000000) as v from c);" > /dev/null; echo "sqlite_100k_point_reads_warm $(ms $t)"
echo "journal_mode $(sqlite3 b.sqlite 'pragma journal_mode')"
rm -f b.sqlite*"#;

fn rows(out: &str) -> Value {
    let mut m = serde_json::Map::new();
    for l in out.lines() {
        if let Some((k, v)) = l.split_once(' ') {
            if k.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                m.insert(k.to_string(), v.trim().parse::<u64>().map(Value::from).unwrap_or_else(|_| json!(v.trim())));
            }
        }
    }
    Value::Object(m)
}

/// The tools' benchmarks on FPAC's /data and on the container's own disk,
/// in one guest: git, SQLite, and the file-system rows (`fpac-probe
/// fsbench`).
pub fn tools(p: &mut dyn Place) -> Result<Value, String> {
    let st = p.start()?;
    let m = mount(p)?;
    let (code, out) = p.sh(GIT_SOURCE)?;
    if code != 0 {
        return Err(format!("the git source: {out}"));
    }
    let mut by = serde_json::Map::new();
    for (label, dir) in [("fpac", "/data"), ("local", "/var/tmp/local")] {
        let (code, out) = p.sh(&format!("mkdir -p {dir} && T={dir} sh -c '{}'", TOOLS.replace('\'', "'\\''")))?;
        let tools = if code == 0 { rows(&out) } else { json!({ "error": out }) };
        let (_, fs) = p.sh(&format!("/usr/local/bin/fpac-probe fsbench --dir {dir}"))?;
        let fs: Value = fs.lines().rev().find_map(|l| serde_json::from_str(l).ok()).unwrap_or_else(|| json!({ "error": fs }));
        by.insert(label.to_string(), json!({ "dir": dir, "tools": tools, "fs": fs }));
    }
    let status = status(p);
    let sl = sleep(p)?;
    p.kill()?;
    Ok(json!({ "what": p.what(), "start": st, "mount": m["mounted"], "results": by, "daemon": status, "sleep_ms": sl["sleep_ms"], "npm": "not run: the guests have no internet (enableInternet is off), and npm ci needs its registry" }))
}

/// The npm benchmark's workload (docs/results.md, C): cloudflare/sandbox-sdk
/// at a pinned commit, installed from its own lockfile.
const NPM_REPO: &str = "https://github.com/cloudflare/sandbox-sdk";
const NPM_COMMIT: &str = "61cac81b53334b314e4a4daf04486b9cee9b65c2";
/// Every top-level package of the tree required once, its time and how
/// many loaded: a cold start of a program that requires much of the tree.
const NPM_REQUIRE_ALL: &str = r#"const fs = require('fs'); const t0 = process.hrtime.bigint();
const lock = JSON.parse(fs.readFileSync('node_modules/.package-lock.json'));
let loaded = 0, failed = 0;
for (const k of Object.keys(lock.packages)) { const m = /^node_modules\/((?:@[^/]+\/)?[^/]+)$/.exec(k); if (!m) continue; try { require(m[1]); loaded++; } catch (e) { failed++; } }
console.log(JSON.stringify({ loaded, failed, ms: Number(process.hrtime.bigint() - t0) / 1e6 })); process.exit(0);"#;

/// A shell step's wall time in the guest (ms), its exit code and its last
/// output, run detached and polled (an install outlasts one exec).
fn timed(p: &dyn Place, script: &str, tag: &str) -> Result<Value, String> {
    let wrapped = format!("t0=$(date +%s%N)\n{script}\nrc=$?\nt1=$(date +%s%N)\necho \"FPAC_MS $(( (t1 - t0) / 1000000 ))\"\nexit $rc");
    let (code, out) = sh_long(p, &wrapped, "", tag)?;
    let ms: Option<u64> = out.lines().rev().find_map(|l| l.strip_prefix("FPAC_MS ")).and_then(|v| v.trim().parse().ok());
    let tail: String = out.lines().filter(|l| !l.starts_with("FPAC_MS ")).collect::<Vec<_>>().iter().rev().take(6).rev().cloned().collect::<Vec<_>>().join("\n");
    Ok(json!({ "code": code, "ms": ms, "tail": tail }))
}

/// One run of the npm benchmark (C), in a disposable Cloudflare container
/// with the internet, `variant`: `own` (the container's own disk), `shipped`
/// (FPAC, node_modules shipped with the rest) or `local` (FPAC, node_modules
/// on the container's own disk, so a wake installs it again). Never run on
/// this machine: an install runs its packages' own scripts.
pub fn npm(p: &mut dyn Place, variant: &str, workload: &str) -> Result<Value, String> {
    assert!(["own", "shipped", "local"].contains(&variant));
    assert!(["sandbox-sdk", "heavy"].contains(&workload));
    let fpac = variant != "own";
    let w = if fpac { "/data/work" } else { "/root/work" };
    let sb = format!("{w}/sb");
    let local_nm = |p: &dyn Place| -> Result<(), String> {
        // the bind mount survives `npm ci`, which empties node_modules
        let (c, o) = p.sh(&format!("mkdir -p /scratch/nm {sb}/node_modules && mount --bind /scratch/nm {sb}/node_modules && echo bound"))?;
        if c != 0 || !o.contains("bound") {
            return Err(format!("node_modules local: {o}"));
        }
        Ok(())
    };
    // life 1: fetch, install cold, install again warm, sleep at once
    let start1 = p.start()?;
    let mount1 = if fpac { mount(p)? } else { Value::Null };
    // the heavy workload's package.json and lockfile are in the image
    // (crates/bench/npm-heavy, copied to /opt/npm-heavy)
    let get = if workload == "heavy" {
        format!("mkdir -p {sb} && cp /opt/npm-heavy/package.json /opt/npm-heavy/package-lock.json {sb}/")
    } else {
        format!("mkdir -p {sb} && cd {sb} && git init -q && git fetch -q --depth 1 {NPM_REPO} {NPM_COMMIT} && git checkout -q FETCH_HEAD")
    };
    let fetch = timed(p, &get, "fetch")?;
    if variant == "local" {
        local_nm(p)?;
    }
    if fpac {
        p.sh(SAMPLER)?;
    }
    let before = status(p);
    let install = timed(p, &format!("cd {sb} && npm ci --no-audit --no-fund --loglevel=error"), "install1")?;
    let after = status(p);
    let (_, tree) = p.sh(&format!("cd {sb} && echo files $(find node_modules -type f | wc -l) bytes $(du -sb node_modules | cut -f1) packages $(ls node_modules | wc -l)"))?;
    let again = timed(p, &format!("cd {sb} && npm ci --no-audit --no-fund --loglevel=error"), "install2")?;
    let after_again = status(p);
    let timeline: Vec<Value> = if fpac {
        let (_, log) = p.sh("kill $(cat /tmp/sampler.pid) 2>/dev/null; cat /tmp/status.log")?;
        log.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).map(|s| json!({ "up_ms": s["up_ms"], "unshipped": s["unshipped_extents"], "full_waits": s["full_waits"], "lock_waits_over_1ms": s["lock_waits_over_1ms"], "shipped": s["bytes_shipped"] })).collect()
    } else {
        Vec::new()
    };
    let sleep1 = if fpac { sleep(p)? } else { Value::Null };
    let shipped = status(p);
    // life 2: a wake; what it must install again; a cold start, then warm
    let start2 = p.start()?;
    let mount2 = if fpac { mount(p)? } else { Value::Null };
    let reinstall = match variant {
        "own" => Some(timed(p, &format!("{get} && cd {sb} && npm ci --no-audit --no-fund --loglevel=error"), "reinstall")?),
        "local" => {
            local_nm(p)?;
            Some(timed(p, &format!("cd {sb} && npm ci --no-audit --no-fund --loglevel=error"), "reinstall")?)
        }
        _ => None,
    };
    p.sh(&format!("cat > {sb}/fpac-require-all.cjs <<'FPAC_EOF'\n{NPM_REQUIRE_ALL}\nFPAC_EOF\necho ok"))?;
    // the daemon's first status comes a second after its mount: before it,
    // the mount's own count
    let misses0 = status(p)["misses"].as_u64().or_else(|| mount2["mounted"]["misses"].as_u64());
    let node_cold = timed(p, &format!("cd {sb} && timeout 600 node fpac-require-all.cjs"), "nodecold")?;
    let misses1 = status(p)["misses"].as_u64();
    let node_warm = timed(p, &format!("cd {sb} && timeout 600 node fpac-require-all.cjs"), "nodewarm")?;
    let sleep2 = if fpac { sleep(p)? } else { Value::Null };
    let _ = p.kill();
    let bytes = shipped["bytes_shipped"].as_u64();
    Ok(json!({
        "evidence": "npm", "variant": variant,
        "workload": if workload == "heavy" { json!({ "name": "heavy", "definition": "crates/bench/npm-heavy (package.json, package-lock.json)", "install": "npm ci --no-audit --no-fund", "tree": tree.trim() }) } else { json!({ "name": "sandbox-sdk", "repo": NPM_REPO, "commit": NPM_COMMIT, "install": "npm ci --no-audit --no-fund (its lockfile, lockfileVersion 3)", "tree": tree.trim() }) },
        "life1": { "start": start1, "mount": mount1, "fetch": fetch, "install_cold": install, "install_warm_disk": again, "status_before": before, "status_after_install": after, "status_after_second": after_again, "timeline": timeline, "sleep": sleep1 },
        "shipped_bytes": bytes, "egress_usd_at_0_026_per_gb": bytes.map(|b| (b as f64 / 1e9 * 0.026 * 10000.0).round() / 10000.0),
        "life2": { "start": start2, "mount": mount2, "reinstall": reinstall, "node_cold": node_cold, "node_warm": node_warm, "misses_during_cold_start": misses0.zip(misses1).map(|(a, b)| b.saturating_sub(a)), "sleep": sleep2 },
        "what": "a Cloudflare container of the spike's Worker (2 vCPU, 6 GiB, 20 GB), the npm image (node:22-bookworm, pinned by digest) with the internet on for its starts alone; the stand-in Worker's handler and R2",
    }))
}

/// The heavy workload's lockfile (C), made once in a disposable container
/// with the internet: `npm install --package-lock-only` of
/// crates/bench/npm-heavy/package.json, its tree resolved as of the date
/// the top-level pins were chosen before, no package's scripts run. Answers
/// the lockfile's text.
pub fn npm_lock(p: &mut dyn Place, package_json: &str, before: &str) -> Result<String, String> {
    assert!(package_json.len() < 32 << 10 && !package_json.contains("FPAC_EOF"));
    p.start()?;
    let (code, out) = p.sh(&format!("mkdir -p /root/lock && cat > /root/lock/package.json <<'FPAC_EOF'\n{package_json}\nFPAC_EOF\ncd /root/lock && npm install --package-lock-only --ignore-scripts --no-audit --no-fund --loglevel=error --before={before} && wc -c < package-lock.json"))?;
    if code != 0 {
        return Err(format!("the lockfile: {out}"));
    }
    let n: usize = out.lines().rev().find_map(|l| l.trim().parse().ok()).ok_or("the lockfile's size")?;
    // in pieces under the exec's output limit
    let piece = 3 << 20;
    let mut lock = String::new();
    for at in (0..n).step_by(piece) {
        let (code, part) = p.sh(&format!("tail -c +{} /root/lock/package-lock.json | head -c {piece}", at + 1))?;
        if code != 0 {
            return Err(format!("the lockfile at {at}"));
        }
        lock.push_str(&part);
    }
    let _ = p.kill();
    if lock.len() != n {
        return Err(format!("the lockfile came back as {} of its {n} bytes", lock.len()));
    }
    Ok(lock)
}

