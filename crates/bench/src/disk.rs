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
    fn what(&self) -> &'static str;
    /// The disk's objects in the store: how many, how many bytes.
    fn objects(&self) -> Value;
    /// Deletes the disk's objects (bulk test data).
    fn cleanup(&mut self) -> Value;
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
        let head = fpac_remote::Head { epoch: ce, checkpoint: Some(fpac_remote::CheckpointRef { epoch: ce, seq: cs, sum: fpac_remote::sha256(&body) }) };
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

impl Place for Krun {
    fn start(&mut self) -> Result<Value, String> {
        self.kill()?;
        self.epoch += 1;
        let sock = self.dir.join(format!("handler-{}.sock", self.epoch));
        let _ = std::fs::remove_file(&sock);
        let listener = std::os::unix::net::UnixListener::bind(&sock).map_err(|e| e.to_string())?;
        let mut folder = Folder::new(&self.root, "disk", self.epoch);
        folder.volume_bytes = self.volume;
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

    fn what(&self) -> &'static str {
        "a krun microVM per life (2 vCPU, 6 GiB, 20 GB); the folder remote on a unix socket on this machine: a STAND-IN for the route and for R2"
    }

    fn objects(&self) -> Value {
        count_objects(&self.root)
    }

    fn cleanup(&mut self) -> Value {
        let _ = self.kill();
        let _ = std::fs::remove_dir_all(&self.root);
        json!("the folder removed")
    }
}

pub struct Cloudflare {
    c: Config,
    key: net::Key,
    disk: String,
    volume: u64,
}

impl Cloudflare {
    pub fn new(disk: &str, volume: u64) -> Result<Cloudflare, String> {
        let c = Config::load()?;
        let key = net::Key::load_or_make(&c.driver_key_file);
        Ok(Cloudflare { c, key, disk: disk.to_string(), volume })
    }

}

impl Place for Cloudflare {
    fn start(&mut self) -> Result<Value, String> {
        let _ = net::worker(self.c.host(), &self.key, "POST", "/destroy", &json!({}), Duration::from_secs(60));
        let epoch = crate::now_ms();
        let t0 = Instant::now();
        let s = net::worker(self.c.host(), &self.key, "POST", "/start", &json!({ "disk": self.disk, "image": "tiny", "epoch": epoch, "volume": self.volume }), Duration::from_secs(300))?;
        Ok(json!({ "start_ms": t0.elapsed().as_secs_f64() * 1000.0, "ready_ms": s["ready"]["ms"], "object_colo": s["object_colo"], "version": s["version"] }))
    }

    fn sh(&self, script: &str) -> Result<(i64, String), String> {
        let r = net::worker(self.c.host(), &self.key, "POST", "/exec", &json!({ "argv": ["sh", "-c", script], "ms": 300_000 }), Duration::from_secs(360))?;
        Ok((r["exitCode"].as_i64().unwrap_or(-1), format!("{}{}", r["stdout"].as_str().unwrap_or(""), r["stderr"].as_str().unwrap_or(""))))
    }

    fn kill(&mut self) -> Result<(), String> {
        net::worker(self.c.host(), &self.key, "POST", "/destroy", &json!({}), Duration::from_secs(60)).map(|_| ())
    }

    fn what(&self) -> &'static str {
        "a Cloudflare container per life (2 vCPU, 6 GiB, 20 GB); the stand-in Worker's handler on its machine and R2"
    }

    fn objects(&self) -> Value {
        net::worker(self.c.host(), &self.key, "POST", "/objects/usage", &json!({ "prefix": format!("disks/{}/", self.disk) }), Duration::from_secs(300)).unwrap_or(Value::Null)
    }

    fn cleanup(&mut self) -> Value {
        let _ = self.kill();
        net::worker(self.c.host(), &self.key, "POST", "/objects/delete", &json!({ "prefix": format!("disks/{}/", self.disk) }), Duration::from_secs(600)).unwrap_or(Value::Null)
    }
}

/// Starts the daemon and waits for its mount: its events, by name, in ms.
pub fn mount(p: &dyn Place) -> Result<Value, String> {
    let (code, out) = p.sh("mkdir -p /run/fpac && setsid nohup /usr/local/bin/fpac mount --dir /data > /run/fpac/log 2>&1 < /dev/null & echo started")?;
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
pub fn wake(p: &mut dyn Place, gb: u64, wakes: u32) -> Result<Value, String> {
    let st = p.start()?;
    let m = mount(p)?;
    let (code, out) = p.sh(BOOT_SET)?;
    if code != 0 {
        return Err(format!("the boot's files: {out}"));
    }
    let t = Instant::now();
    // the rest of the disk: 1 GiB files of noise (urandom is fast enough)
    let (code, out) = p.sh(&format!("set -e; mkdir -p /data/fill; for i in $(seq 1 {gb}); do head -c 1073741824 /dev/urandom > /data/fill/g$i; done; sync; df -B1 /data | tail -1"))?;
    if code != 0 {
        return Err(format!("the fill: {out}"));
    }
    let fill_ms = t.elapsed().as_millis() as u64;
    let s = sleep(p)?;
    p.kill()?;
    let mut lives = Vec::new();
    for _ in 0..wakes {
        let t0 = Instant::now();
        let st = p.start()?;
        let m = mount(p)?;
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
            "pack": m["pack"], "boot_read_ok": code == 0, "boot_read_ms": boot_read, "misses_after_boot_read": status["misses"],
            "wake_to_boot_read_ms": wake_ms, "far_1mib_read_ms": far, "sleep_ms": sl["sleep_ms"],
        }));
    }
    Ok(json!({ "what": p.what(), "gb": gb, "first": { "start": st, "mount": m, "fill_ms": fill_ms, "fill_df": out.trim(), "sleep": s }, "wakes": lives }))
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
t=$(now); sqlite3 b.sqlite "select count(*) from (with recursive c(x) as (select 1 union all select x+1 from c where x < 100000) select (select v from big where id = 1 + abs(random()) % 1000000) from c);" > /dev/null; echo "sqlite_100k_point_reads_warm $(ms $t)"
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
