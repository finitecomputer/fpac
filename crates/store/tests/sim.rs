//! The deterministic simulator (docs/design.md, Testing). The store runs
//! against a remote that loses answers, is unavailable for a while, and
//! answers ranged gets with the right length and the wrong bytes (as
//! Cloudflare did, docs/spike.md); against a cache that may lie; and it
//! crashes at every step. A model, kept apart from the store, says what
//! the disk must hold:
//!
//! - every read equals what was last written (never wrong bytes; an
//!   error only where the remote kept lying past the store's tries);
//! - after a crash, the next life opens to exactly the disk as of the
//!   last flush whose segments, and every earlier flush's, all reached
//!   the store; and that is never before the last flush the store
//!   acknowledged (nothing flushed is lost).
//!
//! Every random choice comes from one seeded generator, so a failure names
//! its seed and replays exactly.

use std::collections::{BTreeMap, BTreeSet};

use fpac_format::limits::{EXTENT_BYTES, FETCH_TRIES_MAX, PACK_EXTENTS_MAX};
use fpac_format::{self as format};
use fpac_remote::limits::OBJECT_BYTES_MAX;
use fpac_remote::{Put, RemoteError};
use fpac_store::head::{decide, Decision};
use fpac_store::{ship, Ask, Head, Kind, MemCache, OpenInfo, Outgoing, RemoteGate, Store, StoreError};

const EXT: usize = EXTENT_BYTES as usize;
const VOLUME: u64 = 4 * 4096 * EXTENT_BYTES as u64; // 16,384 extents: four map pages

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    /// True `per_mille` times in 1,000.
    fn chance(&mut self, per_mille: u64) -> bool {
        self.below(1000) < per_mille
    }
}

/// Faults the remote injects, each in 1,000 calls.
#[derive(Clone, Copy)]
struct Faults {
    unavailable: u64,
    lost_answer: u64,
    wrong_bytes: u64,
}

struct SimRemote {
    objects: BTreeMap<String, Vec<u8>>,
    /// Reads the store answered, and those it answered with an error.
    reads: u64,
    read_errors: u64,
    rng: Rng,
    faults: Faults,
    /// Objects whose every ranged read is wrong.
    always_wrong: BTreeSet<String>,
    gets: u64,
    wrong_served: u64,
}

impl SimRemote {
    fn new(seed: u64, faults: Faults) -> SimRemote {
        SimRemote { objects: BTreeMap::new(), reads: 0, read_errors: 0, rng: Rng(seed | 1), faults, always_wrong: BTreeSet::new(), gets: 0, wrong_served: 0 }
    }
}

impl RemoteGate for SimRemote {
    fn get_range(&mut self, name: &str, offset: u64, buf: &mut [u8]) -> Result<Option<usize>, RemoteError> {
        self.gets += 1;
        if self.rng.chance(self.faults.unavailable) {
            return Err(RemoteError::Unavailable("sim: down".into()));
        }
        let Some(obj) = self.objects.get(name) else { return Ok(None) };
        let o = offset as usize;
        if o >= obj.len() {
            return Err(RemoteError::Refused { status: 416, detail: "sim".into() });
        }
        let n = buf.len().min(obj.len() - o);
        buf[..n].copy_from_slice(&obj[o..o + n]);
        if self.always_wrong.contains(name) || self.rng.chance(self.faults.wrong_bytes) {
            // Cloudflare's shape: right for 8 KiB, then the object's bytes
            // from 28 KiB further on (or garbled where there are none).
            self.wrong_served += 1;
            for (i, b) in buf.iter_mut().enumerate().take(n).skip(8192) {
                *b = obj.get(o + i + 28672).copied().unwrap_or(*b ^ 0x5a) ^ u8::from(i == 8192);
            }
            if n <= 8192 {
                buf[0] ^= 1;
            }
        }
        Ok(Some(n))
    }

    fn get(&mut self, name: &str, sink: &mut dyn FnMut(&[u8])) -> Result<Option<u64>, RemoteError> {
        self.gets += 1;
        if self.rng.chance(self.faults.unavailable) {
            return Err(RemoteError::Unavailable("sim: down".into()));
        }
        // A whole get is checked by the real client against the stored
        // SHA-256: a wrong one arrives as Corrupt, never as bytes.
        if self.rng.chance(self.faults.wrong_bytes) {
            return Err(RemoteError::Corrupt("sim: a whole get failed its checksum".into()));
        }
        match self.objects.get(name) {
            Some(o) => {
                sink(o);
                Ok(Some(o.len() as u64))
            }
            None => Ok(None),
        }
    }

    fn put(&mut self, name: &str, body: &[u8]) -> Result<Put, RemoteError> {
        // The route's own limit (docs/contract.md): refused, never retried.
        if body.len() as u64 > OBJECT_BYTES_MAX {
            return Err(RemoteError::Refused { status: 413, detail: format!("sim: {} bytes", body.len()) });
        }
        if self.rng.chance(self.faults.unavailable) {
            return Err(RemoteError::Unavailable("sim: down".into()));
        }
        let answer = match self.objects.get(name) {
            Some(o) if o == body => Put::Same,
            Some(o) => Put::Conflict { sha256: format::sha256(o) },
            None => {
                self.objects.insert(name.to_string(), body.to_vec());
                Put::Created
            }
        };
        if self.rng.chance(self.faults.lost_answer) {
            return Err(RemoteError::Unavailable("sim: the answer was lost".into()));
        }
        Ok(answer)
    }
}

/// The platform's part: the head, moved only by `decide`.
struct Platform {
    head: Option<Head>,
    running: u64,
}

impl Platform {
    fn post(&mut self, h: &Head) -> bool {
        match decide(self.head.as_ref(), self.running, h) {
            Decision::Take => {
                self.head = Some(*h);
                true
            }
            Decision::Same => true,
            Decision::Refuse(why) => panic!("the platform refused a head: {why}"),
        }
    }
}

fn content(seed: u64) -> Vec<u8> {
    let mut r = Rng(seed | 1);
    let mut v = vec![0u8; EXT];
    for c in v.chunks_mut(8) {
        c.copy_from_slice(&r.next().to_le_bytes());
    }
    v
}

/// A flush, as the model keeps it: its segments' names, and the extents
/// it shipped with the bytes they had at the cut.
type Cut = (Vec<String>, BTreeMap<u64, Vec<u8>>);

/// The model: what each extent holds now, and per life what it opened
/// with and what each of its flushes shipped.
struct Model {
    live: BTreeMap<u64, Vec<u8>>,
    opened: BTreeMap<u64, Vec<u8>>,
    head_posted: bool,
    /// Each flush: its segments' names, and the extents it shipped with
    /// the bytes they had at the cut.
    cuts: Vec<Cut>,
    /// Segments the store was told landed.
    landed: BTreeSet<String>,
}

impl Model {
    fn get(&self, e: u64) -> Vec<u8> {
        self.live.get(&e).cloned().unwrap_or_else(|| vec![0u8; EXT])
    }

    /// What the next life must open to.
    fn after_crash(&self, remote: &SimRemote) -> BTreeMap<u64, Vec<u8>> {
        let mut s = self.opened.clone();
        if !self.head_posted {
            return s;
        }
        let mut complete = 0;
        for (names, shipped) in &self.cuts {
            if !names.iter().all(|n| remote.objects.contains_key(n)) {
                break;
            }
            for (e, b) in shipped {
                s.insert(*e, b.clone());
            }
            complete += 1;
        }
        assert!(complete >= self.acked(), "a flush the store acknowledged is not in the store: nothing flushed may be lost");
        s
    }

    /// Flushes acknowledged: the longest run of the first ones whose every
    /// segment the store was told landed.
    fn acked(&self) -> usize {
        self.cuts.iter().take_while(|(names, _)| names.iter().all(|n| self.landed.contains(n))).count()
    }
}

/// One life of the disk, from its open to a crash or a sleep.
struct Life<'a> {
    store: Store,
    cache: MemCache,
    remote: &'a mut SimRemote,
    platform: &'a mut Platform,
    model: Model,
    steps: u64,
    crash_at: u64,
    /// Segments cut and not yet landed, from any number of flushes: they
    /// land in any order, as parallel puts do.
    in_flight: Vec<Outgoing>,
    /// In its last turn: its checkpoint is a sleep's.
    sleeping: bool,
}

#[derive(Debug)]
struct Crash;

impl Life<'_> {
    /// One step; crashes when the step count reaches `crash_at`.
    fn step(&mut self) -> Result<(), Crash> {
        self.steps += 1;
        if self.steps == self.crash_at {
            Err(Crash)
        } else {
            Ok(())
        }
    }

    /// Puts objects in order, each retried until it lands; a step before
    /// and after every put, so a crash can fall between any two.
    fn ship_all(&mut self, out: &[Outgoing]) -> Result<(), Crash> {
        for o in out {
            self.step()?;
            // Bounded: an unavailable remote is retried; the faults are
            // a few in a thousand, so this ends.
            for _ in 0..1000 {
                match ship(self.remote, o, 1) {
                    Ok(()) => break,
                    Err(StoreError::Unavailable(_)) => continue,
                    Err(e) => panic!("shipping {}: {e}", o.name),
                }
            }
            self.step()?;
        }
        Ok(())
    }

    fn post_head(&mut self) -> Result<(), Crash> {
        let (out, head) = self.store.prepare_head();
        self.ship_all(&out)?;
        if !out.is_empty() {
            self.store.checkpoint_landed(&out);
        }
        self.step()?;
        assert!(self.platform.post(&head));
        self.store.head_posted();
        self.model.head_posted = true;
        Ok(())
    }

    fn read_check(&mut self, e: u64) -> Result<(), Crash> {
        self.step()?;
        let mut buf = vec![0u8; EXT];
        self.remote.reads += 1;
        let r = self.store.read(self.remote, &mut self.cache, e, &mut buf);
        if r.is_err() {
            self.remote.read_errors += 1;
        }
        match r {
            Ok(()) => assert!(buf == self.model.get(e), "extent {e}: the store handed out wrong bytes"),
            // only where the remote kept lying past every try, or was down
            Err(StoreError::Corrupt(why)) => assert!(why.contains(&format!("after {FETCH_TRIES_MAX} tries")), "{why}"),
            Err(StoreError::Unavailable(_)) => {}
            Err(other) => panic!("a read failed: {other}"),
        }
        Ok(())
    }

    fn write(&mut self, e: u64, rng: &mut Rng) -> Result<(), Crash> {
        self.step()?;
        let whole = rng.chance(500);
        let (off, len) = if whole { (0, EXT) } else { (rng.below(EXT as u64 - 100) as usize, 1 + rng.below(99) as usize) };
        let data: Vec<u8> = content(rng.next())[..len].to_vec();
        match self.store.write(self.remote, &mut self.cache, e, off as u32, &data) {
            Ok(()) => {
                let mut b = self.model.get(e);
                b[off..off + len].copy_from_slice(&data);
                self.model.live.insert(e, b);
            }
            // a partial write reads first, and that read may fail
            Err(StoreError::Corrupt(_)) | Err(StoreError::Unavailable(_)) => {}
            Err(other) => panic!("a write failed: {other}"),
        }
        Ok(())
    }

    fn punch(&mut self, e: u64) -> Result<(), Crash> {
        self.step()?;
        self.store.punch(&mut self.cache, e).expect("a punch");
        self.model.live.insert(e, vec![0u8; EXT]);
        Ok(())
    }

    /// Writes many whole extents at once: a cut of several segments.
    fn burst(&mut self, wide: &[u64], rng: &mut Rng) -> Result<(), Crash> {
        let n = 130 + rng.below(140);
        for _ in 0..n {
            let e = wide[rng.below(wide.len() as u64) as usize];
            self.steps += 1;
            if self.steps == self.crash_at {
                return Err(Crash);
            }
            let data = content(rng.next());
            self.store.write(self.remote, &mut self.cache, e, 0, &data).expect("a whole write");
            self.model.live.insert(e, data);
        }
        Ok(())
    }

    /// A cut: it only names its segments; they are built later, in
    /// order, while writes go on (`build_one`), and the model keeps what
    /// each extent held at this instant.
    fn flush(&mut self) -> Result<(), Crash> {
        self.step()?;
        let planned = self.store.cut().expect("a cut");
        if planned.is_empty() {
            return Ok(());
        }
        let mut shipped = BTreeMap::new();
        let mut names = Vec::new();
        for (seq, extents) in &planned {
            names.push(format::segment_name(format::SegId { epoch: self.store.epoch(), seq: *seq }));
            for e in extents {
                shipped.insert(*e, self.model.get(*e));
            }
        }
        if std::env::var("SIM_TRACE").is_ok() {
            eprintln!("  cut {names:?}");
        }
        self.model.cuts.push((names, shipped));
        Ok(())
    }

    /// The next planned segment is built (from the bytes its extents had
    /// at its cut) and joins those in flight.
    fn build_one(&mut self) -> Result<bool, Crash> {
        self.step()?;
        match self.store.build_next(&mut self.cache).expect("a build") {
            Some(o) => {
                self.in_flight.push(o);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// One segment in flight, chosen at random, lands: the map must still
    /// move in segment order.
    fn land_one(&mut self, rng: &mut Rng) -> Result<(), Crash> {
        if self.in_flight.is_empty() && !self.build_one()? {
            return Ok(());
        }
        let o = self.in_flight.remove(rng.below(self.in_flight.len() as u64) as usize);
        self.ship_all(std::slice::from_ref(&o))?;
        let Kind::Segment(seq) = o.kind else { unreachable!() };
        self.store.landed(seq);
        if std::env::var("SIM_TRACE").is_ok() {
            eprintln!("  landed {}", o.name);
        }
        self.model.landed.insert(o.name);
        Ok(())
    }

    fn land_all(&mut self, rng: &mut Rng) -> Result<(), Crash> {
        // Bounded by the segments planned and in flight.
        while self.build_one()? {}
        while !self.in_flight.is_empty() {
            self.land_one(rng)?;
        }
        Ok(())
    }

    /// A checkpoint of what has landed so far, whatever is still in
    /// flight, then the head.
    fn checkpoint(&mut self) -> Result<(), Crash> {
        self.step()?;
        if !self.store.checkpoint_due() || (std::env::var("SIM_NO_INFLIGHT_CKPT").is_ok() && self.store.shipping() > 0) {
            return Ok(());
        }
        // a periodic checkpoint's crash pack first (a sleep's pack has
        // already landed, and it drops the crash pack)
        if !self.sleeping {
            if let Some(p) = self.store.crash_pack(&mut self.cache).expect("a crash pack") {
                self.ship_all(std::slice::from_ref(&p))?;
                let Kind::CrashPack(pr) = p.kind else { unreachable!() };
                self.store.crash_pack_landed(pr);
            }
        }
        // the pages it rewrites that are not loaded yet
        for pr in self.store.checkpoint_needs() {
            self.step()?;
            let mut body = Vec::new();
            // Bounded: the faults are a few in a thousand calls.
            for _ in 0..1000 {
                body.clear();
                match self.remote.get(&fpac_store::page_name(&pr), &mut |b| body.extend_from_slice(b)) {
                    Ok(Some(_)) => break,
                    Ok(None) => panic!("a page the checkpoint names is gone"),
                    Err(_) => continue,
                }
            }
            self.store.install_page(pr, &body).expect("the page");
        }
        let out = self.store.checkpoint();
        self.ship_all(&out)?;
        self.store.checkpoint_landed(&out);
        let Some(Kind::Checkpoint(cr)) = out.last().map(|o| o.kind) else { unreachable!() };
        if std::env::var("SIM_TRACE").is_ok() {
            let c = format::decode_checkpoint(&out.last().unwrap().body).unwrap();
            eprintln!("  checkpoint e{}/c{} end {}/{} in flight {}", cr.epoch, cr.seq, c.end.epoch, c.end.seq, self.in_flight.len());
        }
        self.step()?;
        assert!(self.platform.post(&Head { epoch: self.store.epoch(), checkpoint: Some(cr), clean: false }));
        Ok(())
    }

    /// A sleep: everything in flight lands, the last flush, the pack, a
    /// checkpoint naming it, the head.
    fn sleep(&mut self, rng: &mut Rng) -> Result<(), Crash> {
        self.sleeping = true;
        self.flush()?;
        self.land_all(rng)?;
        self.store.stop_boot();
        if let Some(p) = self.store.pack(&mut self.cache).expect("a pack") {
            self.ship_all(std::slice::from_ref(&p))?;
            let Kind::Pack(pr) = p.kind else { unreachable!() };
            self.store.pack_landed(pr);
        }
        self.checkpoint()?;
        // the head marked clean: the next open replays nothing
        self.step()?;
        assert!(self.platform.post(&self.store.clean_head()));
        Ok(())
    }
}

/// Opens a life, retrying an unavailable or lying remote; checks it opened
/// to what the model says.
fn open<'a>(remote: &'a mut SimRemote, platform: &'a mut Platform, epoch: u64, expected: BTreeMap<u64, Vec<u8>>, crash_at: u64) -> Life<'a> {
    open_sized(remote, platform, epoch, expected, crash_at, VOLUME)
}

fn open_sized<'a>(remote: &'a mut SimRemote, platform: &'a mut Platform, epoch: u64, expected: BTreeMap<u64, Vec<u8>>, crash_at: u64, volume_bytes: u64) -> Life<'a> {
    platform.running = epoch;
    let info = OpenInfo { epoch, volume_bytes, head: platform.head };
    let mut store = None;
    // Bounded: faults are a few in a thousand calls.
    for _ in 0..1000 {
        match Store::open(remote, &info) {
            Ok(s) => {
                store = Some(s);
                break;
            }
            Err(StoreError::Unavailable(_)) => continue,
            Err(StoreError::Corrupt(why)) if why.contains("checksum") => continue,
            Err(e) => panic!("open, life {epoch}: {e}"),
        }
    }
    let mut store = store.expect("opened");
    store.record_boot();
    let model = Model { live: expected.clone(), opened: expected, head_posted: false, cuts: Vec::new(), landed: BTreeSet::new() };
    Life { store, cache: MemCache::default(), remote, platform, model, steps: 0, crash_at, in_flight: Vec::new(), sleeping: false }
}

/// One whole run: `lives` lives of random work, each crashing at a random
/// step (or sleeping cleanly), checked on every reopen.
fn run(seed: u64, lives: u64, ops: u64, faults: Faults) -> (u64, u64, u64, u64) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut remote = SimRemote::new(seed ^ 0xabcd, faults);
    let mut platform = Platform { head: None, running: 0 };
    let mut expected: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let hot: Vec<u64> = (0..48).map(|_| rng.below(VOLUME / EXTENT_BYTES as u64)).collect();
    let wide: Vec<u64> = (0..400).map(|_| rng.below(VOLUME / EXTENT_BYTES as u64)).collect();
    let mut crashes = 0;
    for epoch in 1..=lives {
        let crash_at = if rng.chance(700) { 1 + rng.below(ops * 4) } else { u64::MAX };
        let mut life = open(&mut remote, &mut platform, epoch, expected.clone(), crash_at);
        if std::env::var("SIM_TRACE").is_ok() {
            eprintln!("life {epoch} head {:?} crash_at {crash_at} expected15 {:?} replayed {}", life.platform.head.map(|h| (h.epoch, h.checkpoint.map(|c| (c.epoch, c.seq)))), expected.get(&15).map(|b| b[..4].to_vec()), life.store.opened().replayed);
        }
        // the life opened to exactly what the model says, everywhere it
        // was ever written
        let mut check: BTreeSet<u64> = hot.iter().copied().collect();
        check.extend(expected.keys().copied());
        for e in check {
            let mut buf = vec![0u8; EXT];
            if life.store.read(life.remote, &mut life.cache, e, &mut buf).is_ok() {
                assert!(buf == life.model.get(e), "seed {seed}, life {epoch}: opened with wrong bytes at {e}");
            }
        }
        let outcome: Result<(), Crash> = (|| {
            life.post_head()?;
            for _ in 0..ops {
                let e = hot[rng.below(hot.len() as u64) as usize];
                match rng.below(100) {
                    0..=29 => life.write(e, &mut rng)?,
                    30..=32 => life.burst(&wide, &mut rng)?,
                    33..=36 => life.punch(e)?,
                    37..=64 => life.read_check(e)?,
                    65..=72 => life.flush()?,
                    73..=79 => {
                        life.build_one()?;
                    }
                    80..=93 => life.land_one(&mut rng)?,
                    _ => life.checkpoint()?,
                }
            }
            life.sleep(&mut rng)
        })();
        expected = match outcome {
            Ok(()) => {
                // a clean sleep: everything written was flushed
                assert_eq!(life.store.dirty_extents(), 0);
                life.model.after_crash(life.remote)
            }
            Err(Crash) => {
                crashes += 1;
                life.model.after_crash(life.remote)
            }
        };
    }
    (crashes, remote.wrong_served, remote.reads, remote.read_errors)
}

/// Seeds per run: the full sweep in a release build (`cargo test
/// --release`, the evidence's run), fewer in a debug build, which is
/// about thirty times slower here.
const SEEDS: u64 = if cfg!(debug_assertions) { 6 } else { 60 };

const CALM: Faults = Faults { unavailable: 0, lost_answer: 0, wrong_bytes: 0 };
const STORMY: Faults = Faults { unavailable: 30, lost_answer: 30, wrong_bytes: 100 };

// Goal: with a well-behaved remote, lives that crash anywhere reopen to
// the last flush that fully landed, and no acknowledged flush is lost.
// Method: 60 seeds (6 in a debug build) of 12 lives of 150 operations.
#[test]
fn calm_remote_crashes_anywhere() {
    for seed in 1..=SEEDS {
        run(seed, 12, 150, CALM);
    }
}

// Goal: the same with a remote that is down a few times in a hundred,
// loses put answers, and answers one ranged get in ten with the right
// length and the wrong bytes: no wrong bytes ever reach a reader.
// Method: 60 seeds (6 in a debug build) of 12 lives of 150 operations.
#[test]
fn stormy_remote_never_hands_out_wrong_bytes() {
    let (mut wrong, mut crashes, mut reads, mut errors) = (0, 0, 0, 0);
    for seed in 1..=SEEDS {
        let (c, w, r, e) = run(seed, 12, 150, STORMY);
        crashes += c;
        wrong += w;
        reads += r;
        errors += e;
    }
    assert!(wrong > 10 * SEEDS, "the faults were exercised: {wrong} wrong answers");
    assert!(crashes > SEEDS * 5, "the crashes were exercised: {crashes}");
    // A fetch that comes back wrong is fetched again: with one wrong get
    // in ten and one down in thirty-three, a read fails only when every
    // try did, well under one in a hundred. Without the refetch it would
    // be about one in eight.
    assert!(errors * 100 < reads, "{errors} of {reads} reads failed: the refetch is not working");
    eprintln!("stormy: {reads} reads, {errors} failed after every try; {wrong} wrong answers served; {crashes} crashes");
}

// Goal: a crash at every single step of one fixed run is recovered,
// with flushes of several segments in flight and landing in any order.
// Method: run seed 7's first two lives once to count the steps of life
// 2, then once per step with a crash there, checking each reopen.
#[test]
fn a_crash_at_every_step() {
    let ops = 150;
    let scenario = |crash_at: u64| -> u64 {
        let mut rng = Rng(7);
        let mut remote = SimRemote::new(77, Faults { unavailable: 20, lost_answer: 20, wrong_bytes: 50 });
        let mut platform = Platform { head: None, running: 0 };
        let hot: Vec<u64> = (0..16).map(|_| rng.below(VOLUME / EXTENT_BYTES as u64)).collect();
        let wide: Vec<u64> = (0..300).map(|_| rng.below(VOLUME / EXTENT_BYTES as u64)).collect();
        let mut expected = BTreeMap::new();
        for epoch in 1..=2u64 {
            let at = if epoch == 2 { crash_at } else { u64::MAX };
            let mut life = open(&mut remote, &mut platform, epoch, expected.clone(), at);
            let out: Result<(), Crash> = (|| {
                life.post_head()?;
                for _ in 0..ops {
                    let e = hot[rng.below(hot.len() as u64) as usize];
                    match rng.below(13) {
                        12 if epoch == 2 => life.burst(&wide, &mut rng)?,
                        0..=4 => life.write(e, &mut rng)?,
                        5..=6 => life.read_check(e)?,
                        7 => life.flush()?,
                        8..=9 => life.land_one(&mut rng)?,
                        _ => life.checkpoint()?,
                    }
                }
                life.sleep(&mut rng)
            })();
            let steps = life.steps;
            expected = life.model.after_crash(life.remote);
            if epoch == 2 {
                // the third life opens to what the model says
                let mut third = open(&mut remote, &mut platform, 3, expected.clone(), u64::MAX);
                for e in hot.iter().copied().collect::<BTreeSet<_>>() {
                    let mut buf = vec![0u8; EXT];
                    if third.store.read(third.remote, &mut third.cache, e, &mut buf).is_ok() {
                        assert!(buf == third.model.get(e), "crash at step {crash_at}: extent {e}");
                    }
                }
                return if out.is_ok() { steps } else { 0 };
            }
        }
        unreachable!()
    };
    let total = scenario(u64::MAX);
    assert!(total > 100, "the run has steps to crash at: {total}");
    // every step in a release build (`cargo test --release`, the
    // evidence's run); every 97th in a debug build, which is slower
    let stride = if cfg!(debug_assertions) { 97 } else { 1 };
    for at in (1..=total).step_by(stride) {
        scenario(at);
    }
    eprintln!("crashed at {} of {total} steps", total.div_ceil(stride as u64));
}

// Goal: an extent whose every fetch comes back wrong is never handed out:
// the read fails, after its tries. Method: one segment always answers
// wrong; a fresh life reads the extent in it.
#[test]
fn a_remote_that_always_lies_gives_an_error_not_bytes() {
    let mut remote = SimRemote::new(5, CALM);
    let mut platform = Platform { head: None, running: 0 };
    let mut life = open(&mut remote, &mut platform, 1, BTreeMap::new(), u64::MAX);
    life.post_head().unwrap();
    let data = content(42);
    life.store.write(life.remote, &mut life.cache, 9, 0, &data).unwrap();
    life.model.live.insert(9, data.clone());
    life.sleep(&mut Rng(3)).unwrap();
    let expected = life.model.after_crash(life.remote);
    remote.always_wrong.insert("e1/s1".into());
    let mut life = open(&mut remote, &mut platform, 2, expected, u64::MAX);
    let mut buf = vec![0u8; EXT];
    let gets = life.remote.gets;
    // the pack would answer it from the cache; this read must be a miss
    life.cache = MemCache::default();
    let r = life.store.read(life.remote, &mut life.cache, 9, &mut buf);
    assert!(matches!(r, Err(StoreError::Corrupt(ref why)) if why.contains("tries")), "{r:?}");
    // its tries only: its map page came with the open's round (the last
    // boot touched it)
    assert_eq!(life.remote.gets - gets, u64::from(FETCH_TRIES_MAX));
}

// Goal: a cache that returns other bytes for a clean extent is caught by
// the extent's checksum, and the extent is fetched again. Method: corrupt
// the cache's copy after a fetch, then read.
#[test]
fn a_lying_cache_is_caught() {
    let mut remote = SimRemote::new(6, CALM);
    let mut platform = Platform { head: None, running: 0 };
    let mut life = open(&mut remote, &mut platform, 1, BTreeMap::new(), u64::MAX);
    life.post_head().unwrap();
    let data = content(43);
    life.store.write(life.remote, &mut life.cache, 3, 0, &data).unwrap();
    life.model.live.insert(3, data.clone());
    life.sleep(&mut Rng(3)).unwrap();
    let expected = life.model.after_crash(life.remote);
    let mut life = open(&mut remote, &mut platform, 2, expected, u64::MAX);
    let mut buf = vec![0u8; EXT];
    life.store.read(life.remote, &mut life.cache, 3, &mut buf).unwrap();
    assert_eq!(buf, data);
    life.cache.extents.get_mut(&3).unwrap()[100] ^= 1;
    let gets = life.remote.gets;
    life.store.read(life.remote, &mut life.cache, 3, &mut buf).unwrap();
    assert_eq!(buf, data, "the right bytes, fetched again");
    assert_eq!(life.remote.gets - gets, 1);
}

// Goal: after a clean sleep, the next wake's pack brings every extent the
// boot read, so the boot's reads are not misses. Method: a life reads 20
// extents and sleeps; the next opens, fetches its pack in ranges (as the
// guest does, in parallel), and reads the same 20 with no ranged get.
#[test]
fn the_pack_makes_the_boot_reads_local() {
    let mut remote = SimRemote::new(8, CALM);
    let mut platform = Platform { head: None, running: 0 };
    let mut life = open(&mut remote, &mut platform, 1, BTreeMap::new(), u64::MAX);
    life.post_head().unwrap();
    for e in 0..40u64 {
        let d = content(e + 100);
        life.store.write(life.remote, &mut life.cache, e * 3, 0, &d).unwrap();
        life.model.live.insert(e * 3, d);
    }
    life.sleep(&mut Rng(3)).unwrap();
    let expected = life.model.after_crash(life.remote);
    // the second life's boot reads 20 of them
    let mut life = open(&mut remote, &mut platform, 2, expected.clone(), u64::MAX);
    life.post_head().unwrap();
    for e in 0..20u64 {
        life.read_check(e * 3).unwrap();
    }
    life.sleep(&mut Rng(3)).unwrap();
    let expected = life.model.after_crash(life.remote);
    // the third life fetches the pack, then reads them: no misses
    let mut life = open(&mut remote, &mut platform, 3, expected, u64::MAX);
    let (name, pr) = life.store.pack_to_fetch().expect("a pack");
    let mut head = vec![0u8; fpac_store::PACK_HEADER_FETCH_BYTES];
    let n = life.remote.get_range(&name, 0, &mut head).unwrap().unwrap();
    let h = life.store.pack_header(&pr, &head[..n]).unwrap();
    assert_eq!(h.entries.len(), 20);
    let mut installed = 0;
    for (off, len) in Store::pack_ranges(&h, 0, 4 * EXTENT_BYTES as u64) {
        let mut b = vec![0u8; len as usize];
        assert_eq!(life.remote.get_range(&name, off, &mut b).unwrap(), Some(len as usize));
        installed += life.store.pack_install(&mut life.cache, &h, off, &b).unwrap();
    }
    assert_eq!(installed, 20);
    let gets = life.remote.gets;
    for e in 0..20u64 {
        life.read_check(e * 3).unwrap();
    }
    assert_eq!(life.remote.gets, gets, "the boot's reads came from the pack");
}

// Goal: a pack entry whose extent changed since is not used, and its
// bytes never reach a reader. Method: the pack holds an extent; a later
// life writes it anew and crashes before a new pack; the next life's pack
// install skips it, and the read is a miss that answers the new bytes.
#[test]
fn a_stale_pack_entry_is_ignored() {
    let mut remote = SimRemote::new(9, CALM);
    let mut platform = Platform { head: None, running: 0 };
    let mut life = open(&mut remote, &mut platform, 1, BTreeMap::new(), u64::MAX);
    life.post_head().unwrap();
    let old = content(1);
    life.store.write(life.remote, &mut life.cache, 5, 0, &old).unwrap();
    life.model.live.insert(5, old);
    life.sleep(&mut Rng(3)).unwrap();
    let exp = life.model.after_crash(life.remote);
    let mut life = open(&mut remote, &mut platform, 2, exp, u64::MAX);
    life.post_head().unwrap();
    life.read_check(5).unwrap();
    life.sleep(&mut Rng(3)).unwrap();
    let exp = life.model.after_crash(life.remote);
    let mut life = open(&mut remote, &mut platform, 3, exp, u64::MAX);
    life.post_head().unwrap();
    let new = content(2);
    life.store.write(life.remote, &mut life.cache, 5, 0, &new).unwrap();
    life.model.live.insert(5, new.clone());
    life.flush().unwrap();
    life.land_all(&mut Rng(3)).unwrap();
    // a crash here: no new pack, no checkpoint
    let exp = life.model.after_crash(life.remote);
    let mut life = open(&mut remote, &mut platform, 4, exp, u64::MAX);
    let (name, pr) = life.store.pack_to_fetch().expect("the old pack");
    let mut head = vec![0u8; fpac_store::PACK_HEADER_FETCH_BYTES];
    let n = life.remote.get_range(&name, 0, &mut head).unwrap().unwrap();
    let h = life.store.pack_header(&pr, &head[..n]).unwrap();
    let (off, len) = Store::pack_ranges(&h, 0, 1 << 20)[0];
    let mut b = vec![0u8; len as usize];
    life.remote.get_range(&name, off, &mut b).unwrap();
    assert_eq!(life.store.pack_install(&mut life.cache, &h, off, &b).unwrap(), 0, "the changed extent is not taken from the pack");
    let mut buf = vec![0u8; EXT];
    life.store.read(life.remote, &mut life.cache, 5, &mut buf).unwrap();
    assert_eq!(buf, new);
}

// Goal: a pack at its fullest is one object the route takes, its header
// and all (a 1 GB fill on krun was refused 413 at 96 MiB and 24,632
// bytes). Method: write more extents than a pack holds, sleep, and wake
// from the pack.
#[test]
fn a_full_pack_fits_one_object() {
    let mut remote = SimRemote::new(11, CALM);
    let mut platform = Platform { head: None, running: 0 };
    let mut life = open(&mut remote, &mut platform, 1, BTreeMap::new(), u64::MAX);
    life.post_head().unwrap();
    for e in 0..PACK_EXTENTS_MAX as u64 + 64 {
        let b = content(100 + e);
        life.store.write(life.remote, &mut life.cache, e, 0, &b).unwrap();
        life.model.live.insert(e, b);
    }
    life.sleep(&mut Rng(3)).unwrap();
    let packs: Vec<u64> = life.remote.objects.iter().filter(|(n, _)| n.starts_with("e1/p")).map(|(_, b)| b.len() as u64).collect();
    assert_eq!(packs.len(), 1, "one pack");
    assert!(packs[0] <= OBJECT_BYTES_MAX, "{} bytes", packs[0]);
    let exp = life.model.after_crash(life.remote);
    let life = open(&mut remote, &mut platform, 2, exp, u64::MAX);
    assert!(life.store.pack_to_fetch().is_some(), "the next life wakes from it");
}

/// A gate that keeps what an open asked for at once, and every read it
/// made before and after.
struct Recording<'a> {
    inner: &'a mut SimRemote,
    /// Each round of asks, and how many reads came before it.
    rounds: Vec<(usize, Vec<Ask>)>,
    reads: Vec<Ask>,
}

impl RemoteGate for Recording<'_> {
    fn get_range(&mut self, name: &str, offset: u64, buf: &mut [u8]) -> Result<Option<usize>, RemoteError> {
        self.reads.push(Ask::Range { name: name.into(), offset, len: buf.len() });
        self.inner.get_range(name, offset, buf)
    }
    fn get(&mut self, name: &str, sink: &mut dyn FnMut(&[u8])) -> Result<Option<u64>, RemoteError> {
        self.reads.push(Ask::Whole(name.into()));
        self.inner.get(name, sink)
    }
    fn put(&mut self, name: &str, body: &[u8]) -> Result<Put, RemoteError> {
        self.inner.put(name, body)
    }
    fn prefetch(&mut self, asks: &[Ask]) {
        self.rounds.push((self.reads.len(), asks.to_vec()));
    }
}

// Goal: an open reads no map page, however many the disk has, but those
// the last boot touched (in its one round) and those its replay changed (a
// wake after a crash); any other page comes when an extent of it is first
// read. Two rounds after a sleep: the checkpoint, then the pack's head and
// the boot's pages at once. Method: a disk with pages 0, 1 and 3 whose
// boot touched 0 only, and a crashed life's segments on page 0; open it
// through a gate that records, then read an extent of page 3.
#[test]
fn an_open_reads_no_page_but_those_its_replay_changed() {
    let mut remote = SimRemote::new(12, CALM);
    let mut platform = Platform { head: None, running: 0 };
    let mut life = open(&mut remote, &mut platform, 1, BTreeMap::new(), u64::MAX);
    life.post_head().unwrap();
    // extents across the pages, as ext4's metadata is spread; the boot
    // (recording) touches page 0 only
    life.store.stop_boot();
    for e in [0u64, 1, 2, 4095, 4096, 8191, 12_288] {
        let b = content(200 + e);
        life.store.write(life.remote, &mut life.cache, e, 0, &b).unwrap();
        life.model.live.insert(e, b);
    }
    life.store.record_boot();
    life.store.read(life.remote, &mut life.cache, 0, &mut vec![0u8; EXT]).unwrap();
    life.sleep(&mut Rng(3)).unwrap();
    let exp = life.model.after_crash(life.remote);
    // after a sleep: two rounds, no page
    platform.running = 2;
    let mut gate = Recording { inner: &mut remote, rounds: Vec::new(), reads: Vec::new() };
    let s = Store::open(&mut gate, &OpenInfo { epoch: 2, volume_bytes: VOLUME, head: platform.head }).unwrap();
    assert_eq!(s.opened().pages, 3);
    assert_eq!(gate.rounds.len(), 1, "one round after the checkpoint");
    assert!(matches!(&gate.reads[..gate.rounds[0].0], [Ask::Whole(n)] if n.starts_with("e1/c")), "{:?}", gate.reads);
    let pages_asked: Vec<&Ask> = gate.rounds[0].1.iter().filter(|a| matches!(a, Ask::Whole(n) if n.contains("/m"))).collect();
    assert!(pages_asked.len() == 1 && matches!(pages_asked[0], Ask::Whole(n) if n.ends_with("-0")), "the boot's page only: {:?}", gate.rounds);
    assert_eq!(s.pages_unloaded(), 2, "the boot's page is loaded, the others not");
    assert!(gate.rounds[0].1.iter().any(|a| matches!(a, Ask::Range { name, len, .. } if name.starts_with("e1/p") && *len == fpac_store::PACK_HEAD_FETCH_BYTES)), "the pack's head");
    assert!(!gate.reads.iter().chain(gate.rounds[0].1.iter()).any(|a| matches!(a, Ask::Range { name, .. } if name.contains("/s"))), "a clean sleep: no segment asked for: {:?}", gate.rounds);
    for r in &gate.reads[gate.rounds[0].0..] {
        assert!(gate.rounds[0].1.contains(r), "{r:?} was not asked for in the round");
    }
    drop(gate);
    let mut life = open(&mut remote, &mut platform, 2, exp, u64::MAX);
    life.post_head().unwrap();
    for i in 0..3u64 {
        let b = content(300 + i);
        life.store.write(life.remote, &mut life.cache, 1, 0, &b).unwrap();
        life.model.live.insert(1, b);
        life.flush().unwrap();
        life.land_all(&mut Rng(3)).unwrap();
    }
    // a crash: no checkpoint of this life; its replay changes page 0 only
    let exp = life.model.after_crash(life.remote);
    platform.running = 3;
    let mut gate = Recording { inner: &mut remote, rounds: Vec::new(), reads: Vec::new() };
    let s = Store::open(&mut gate, &OpenInfo { epoch: 3, volume_bytes: VOLUME, head: platform.head }).unwrap();
    assert_eq!(s.opened().replayed, 3);
    let pages: Vec<&Ask> = gate.reads.iter().filter(|a| matches!(a, Ask::Whole(n) if n.contains("/m"))).collect();
    assert!(pages.len() == 1 && matches!(pages[0], Ask::Whole(n) if n.ends_with("-0")), "page 0 only: {pages:?}");
    assert_eq!(s.pages_unloaded(), 2);
    drop(gate);
    let mut life = open(&mut remote, &mut platform, 3, exp, u64::MAX);
    life.read_check(1).unwrap();
    let pages_before = life.store.pages_unloaded();
    life.read_check(12_288).unwrap();
    assert_eq!(life.store.pages_unloaded(), pages_before - 1, "the read loaded page 3");
    life.read_check(4096).unwrap();
    life.read_check(4095).unwrap();
}

// Goal: a wake after a crash finds what its life rewrote (ext4's journal,
// a database's pages) in the last periodic checkpoint's crash pack, and
// a clean sleep drops the crash pack. Method: write one extent several
// times and others once, checkpoint, crash; then sleep and look again.
#[test]
fn a_crash_wake_finds_the_rewritten_in_its_crash_pack() {
    let mut remote = SimRemote::new(15, CALM);
    let mut platform = Platform { head: None, running: 0 };
    let mut life = open(&mut remote, &mut platform, 1, BTreeMap::new(), u64::MAX);
    life.post_head().unwrap();
    for i in 0..3u64 {
        // extent 7 rewritten each flush, as a journal is; 100+i once
        for (e, seed) in [(7u64, 600 + i), (100 + i, 700 + i)] {
            let b = content(seed);
            life.store.write(life.remote, &mut life.cache, e, 0, &b).unwrap();
            life.model.live.insert(e, b);
        }
        life.flush().unwrap();
        life.land_all(&mut Rng(9)).unwrap();
    }
    life.checkpoint().unwrap();
    // a crash: the next open's checkpoint names a crash pack
    let exp = life.model.after_crash(life.remote);
    let mut life = open(&mut remote, &mut platform, 2, exp, u64::MAX);
    let (name, pr) = life.store.crash_pack_to_fetch().expect("a crash pack");
    let mut head = vec![0u8; fpac_store::PACK_HEAD_FETCH_BYTES];
    let n = life.remote.get_range(&name, 0, &mut head).unwrap().unwrap();
    let h = life.store.pack_header(&pr, &head[..n]).unwrap();
    assert_eq!(h.entries.iter().map(|(e, _)| *e).collect::<Vec<_>>(), vec![7], "only what was rewritten");
    let installed = life.store.pack_install(&mut life.cache, &h, h.len() as u64, &head[h.len()..n - (n - h.len()) % EXT]).unwrap();
    assert_eq!(installed, 1);
    let gets = life.remote.gets;
    life.read_check(7).unwrap();
    assert_eq!(life.remote.gets, gets, "the rewritten extent came with the crash pack");
    life.read_check(101).unwrap();
    // a clean sleep: no crash pack after it
    life.post_head().unwrap();
    life.sleep(&mut Rng(9)).unwrap();
    let exp = life.model.after_crash(life.remote);
    let life = open(&mut remote, &mut platform, 3, exp, u64::MAX);
    assert!(life.store.crash_pack_to_fetch().is_none(), "a sleep drops the crash pack");
}

// Goal: a wake after a crash fetches what its replay brought back (the
// crashed life's writes since its checkpoint) in a few ranged gets, and
// reads them without a miss. Method: write extents in two flushes, crash,
// open, install each of replay_fetches, read with the remote counted.
#[test]
fn a_crash_wake_fetches_what_its_replay_brought_back() {
    let mut remote = SimRemote::new(17, CALM);
    let mut platform = Platform { head: None, running: 0 };
    let mut life = open(&mut remote, &mut platform, 1, BTreeMap::new(), u64::MAX);
    life.post_head().unwrap();
    for (i, e) in [30u64, 31, 32, 900].into_iter().enumerate() {
        let b = content(950 + i as u64);
        life.store.write(life.remote, &mut life.cache, e, 0, &b).unwrap();
        life.model.live.insert(e, b);
        if i == 1 {
            life.flush().unwrap();
            life.land_all(&mut Rng(2)).unwrap();
        }
    }
    life.flush().unwrap();
    life.land_all(&mut Rng(2)).unwrap();
    // a crash: no checkpoint since the head's
    let exp = life.model.after_crash(life.remote);
    let mut life = open(&mut remote, &mut platform, 2, exp, u64::MAX);
    assert_eq!(life.store.opened().replayed, 2);
    let fetches = life.store.replay_fetches();
    let named: BTreeSet<u64> = fetches.iter().flat_map(|f| f.extents().collect::<Vec<_>>()).collect();
    assert_eq!(named, [30u64, 31, 32, 900].into_iter().collect(), "every extent the replay brought back");
    assert!(fetches.len() <= 3, "in runs, not one by one: {}", fetches.len());
    for f in &fetches {
        let mut window = vec![0u8; f.len];
        let n = life.remote.get_range(&f.name, f.offset, &mut window).unwrap().unwrap();
        life.store.install(&mut life.cache, f, &window[..n], &mut vec![0u8; EXT]).unwrap();
    }
    let gets = life.remote.gets;
    for e in [30u64, 31, 32, 900] {
        life.read_check(e).unwrap();
    }
    assert_eq!(life.remote.gets, gets, "no miss after the replay's fetch");
    // a clean sleep's next wake fetches nothing so
    life.post_head().unwrap();
    life.sleep(&mut Rng(2)).unwrap();
    let exp = life.model.after_crash(life.remote);
    let life = open(&mut remote, &mut platform, 3, exp, u64::MAX);
    assert!(life.store.replay_fetches().is_empty());
}

// Goal: a new disk's first wake has its boot's files in its pack: the
// first life's boot wrote them, and a big fill after them does not push
// them out. Method: write the boot's files, then more than a pack holds,
// sleep, and look at the pack's first entries.
#[test]
fn a_new_disks_first_pack_holds_its_boots_files() {
    let mut remote = SimRemote::new(16, CALM);
    let mut platform = Platform { head: None, running: 0 };
    let mut life = open(&mut remote, &mut platform, 1, BTreeMap::new(), u64::MAX);
    life.store.record_boot();
    life.post_head().unwrap();
    let boot_files = [11u64, 12, 13];
    for e in boot_files {
        let b = content(800 + e);
        life.store.write(life.remote, &mut life.cache, e, 0, &b).unwrap();
        life.model.live.insert(e, b);
    }
    for e in 2000..2000 + fpac_format::limits::PACK_EXTENTS_MAX as u64 + 50 {
        let b = content(e);
        life.store.write(life.remote, &mut life.cache, e, 0, &b).unwrap();
        life.model.live.insert(e, b);
        if e % 1000 == 0 {
            life.flush().unwrap();
            life.land_all(&mut Rng(4)).unwrap();
        }
    }
    life.sleep(&mut Rng(4)).unwrap();
    let exp = life.model.after_crash(life.remote);
    let life = open(&mut remote, &mut platform, 2, exp, u64::MAX);
    let (name, pr) = life.store.pack_to_fetch().expect("a pack");
    let mut head = vec![0u8; fpac_store::PACK_HEAD_FETCH_BYTES];
    let n = life.remote.get_range(&name, 0, &mut head).unwrap().unwrap();
    let h = life.store.pack_header(&pr, &head[..n]).unwrap();
    assert_eq!(h.entries.iter().take(3).map(|(e, _)| *e).collect::<Vec<_>>(), boot_files.to_vec(), "the boot's files first");
}

// Goal: a bounded cache drops only what the store holds: a clean extent
// leaves and reads back right (fetched again); a dirty one stays, since
// the cache is its only copy. Method: write, flush and land one extent,
// write another, evict both.
#[test]
fn only_clean_extents_leave_the_cache() {
    let mut remote = SimRemote::new(13, CALM);
    let mut platform = Platform { head: None, running: 0 };
    let mut life = open(&mut remote, &mut platform, 1, BTreeMap::new(), u64::MAX);
    life.post_head().unwrap();
    let (a, b) = (content(400), content(401));
    life.store.write(life.remote, &mut life.cache, 7, 0, &a).unwrap();
    life.model.live.insert(7, a);
    life.flush().unwrap();
    life.land_all(&mut Rng(3)).unwrap();
    life.store.write(life.remote, &mut life.cache, 8, 0, &b).unwrap();
    life.model.live.insert(8, b);
    let gets = life.remote.gets;
    assert!(life.store.evict(&mut life.cache, 7).unwrap(), "clean: dropped");
    assert!(!life.store.is_cached(7));
    assert!(!life.store.evict(&mut life.cache, 8).unwrap(), "dirty: kept");
    assert!(life.store.is_cached(8));
    life.read_check(7).unwrap();
    assert!(life.remote.gets > gets, "the dropped extent was fetched again");
    life.read_check(8).unwrap();
}

// Goal: a put that meets another writer's bytes fences the life: it
// ships nothing more. Method: plant other bytes at the name of the
// life's next segment, then flush.
#[test]
fn another_writer_fences_the_life() {
    let mut remote = SimRemote::new(10, CALM);
    let mut platform = Platform { head: None, running: 0 };
    let mut life = open(&mut remote, &mut platform, 1, BTreeMap::new(), u64::MAX);
    life.post_head().unwrap();
    life.store.write(life.remote, &mut life.cache, 1, 0, &content(3)).unwrap();
    life.remote.objects.insert("e1/s1".into(), b"another writer".to_vec());
    life.store.cut().unwrap();
    let out = life.store.build_next(&mut life.cache).unwrap().expect("a segment");
    let r = ship(life.remote, &out, 3);
    assert!(matches!(r, Err(StoreError::Fenced(_))), "{r:?}");
    life.store.fence();
    assert!(matches!(life.store.write(life.remote, &mut life.cache, 1, 0, &content(4)), Err(StoreError::Fenced(_))));
    assert!(matches!(life.store.cut(), Err(StoreError::Fenced(_))));
}

// Goal: a cut is the disk at its instant, however long its segment waits
// to be built: a write after the cut, whole or partial, or a discard,
// reaches the next cut and not that one. Method: write, cut, write again
// before the build, crash with only the first cut landed, and reopen.
#[test]
fn a_write_after_its_cut_stays_out_of_that_cut() {
    let mut remote = SimRemote::new(14, CALM);
    let mut platform = Platform { head: None, running: 0 };
    let mut life = open(&mut remote, &mut platform, 1, BTreeMap::new(), u64::MAX);
    life.post_head().unwrap();
    let (a, b) = (content(500), content(501));
    for (e, bytes) in [(3u64, &a), (4, &a), (5, &a)] {
        life.store.write(life.remote, &mut life.cache, e, 0, bytes).unwrap();
        life.model.live.insert(e, bytes.clone());
    }
    life.flush().unwrap();
    // before the cut's segment is built: a whole write, a partial write, a discard
    life.store.write(life.remote, &mut life.cache, 3, 0, &b).unwrap();
    life.store.write(life.remote, &mut life.cache, 4, 100, &b[..50]).unwrap();
    life.store.punch(&mut life.cache, 5).unwrap();
    let mut four = a.clone();
    four[100..150].copy_from_slice(&b[..50]);
    // reads see the newest bytes meanwhile
    let mut buf = vec![0u8; EXT];
    life.store.read(life.remote, &mut life.cache, 3, &mut buf).unwrap();
    assert_eq!(buf, b);
    life.land_all(&mut Rng(5)).unwrap();
    assert!(life.cache.shadows.is_empty(), "every copy kept apart was used");
    // a crash now: the second cut never happened
    let exp = life.model.after_crash(life.remote);
    assert_eq!(exp.get(&3), Some(&a));
    let mut life = open(&mut remote, &mut platform, 2, exp, u64::MAX);
    for e in [3, 4, 5] {
        life.read_check(e).unwrap();
    }
    // and the writes after the cut, cut and landed in the next life's
    // terms, are what a later open sees
    life.post_head().unwrap();
    life.store.write(life.remote, &mut life.cache, 3, 0, &b).unwrap();
    life.model.live.insert(3, b.clone());
    life.store.write(life.remote, &mut life.cache, 4, 0, &four).unwrap();
    life.model.live.insert(4, four);
    life.flush().unwrap();
    life.land_all(&mut Rng(5)).unwrap();
    let exp = life.model.after_crash(life.remote);
    let mut life = open(&mut remote, &mut platform, 3, exp, u64::MAX);
    for e in [3, 4, 5] {
        life.read_check(e).unwrap();
    }
}

// Goal: the head's rules, each case: a new disk, a new life, a later
// checkpoint, the same post again, an older or conflicting one, a post
// from a life that isn't running. Method: one assertion per case.
#[test]
fn the_heads_rules() {
    use fpac_store::CheckpointRef;
    let ck = |epoch, seq, b| Some(CheckpointRef { epoch, seq, sum: [b; 32] });
    let h = |epoch, checkpoint| Head { epoch, checkpoint, clean: false };
    let c = |epoch, checkpoint| Head { epoch, checkpoint, clean: true };
    assert_eq!(decide(None, 1, &h(1, None)), Decision::Take);
    assert_eq!(decide(Some(&h(1, None)), 2, &h(2, None)), Decision::Take, "a new life of a new disk");
    assert_eq!(decide(Some(&h(1, ck(1, 4, 1))), 2, &h(2, ck(1, 4, 1))), Decision::Take, "a new life, nothing replayed");
    assert_eq!(decide(Some(&h(2, ck(1, 4, 1))), 2, &h(2, ck(2, 3, 2))), Decision::Take, "its own later checkpoint");
    assert_eq!(decide(Some(&h(2, ck(2, 3, 2))), 2, &h(2, ck(2, 3, 2))), Decision::Same, "the same again");
    assert!(matches!(decide(Some(&h(2, ck(2, 3, 2))), 2, &h(2, ck(2, 3, 9))), Decision::Refuse(_)), "the same checkpoint, another body");
    assert!(matches!(decide(Some(&h(2, ck(2, 5, 2))), 2, &h(2, ck(2, 3, 2))), Decision::Refuse(_)), "an older checkpoint");
    assert!(matches!(decide(Some(&h(3, None)), 3, &h(2, None)), Decision::Refuse(_)), "a life that isn't running");
    assert!(matches!(decide(Some(&h(3, None)), 2, &h(2, None)), Decision::Refuse(_)), "an earlier life's");
    assert!(matches!(decide(Some(&h(2, ck(2, 3, 2))), 2, &h(2, None)), Decision::Refuse(_)), "losing the checkpoint");
    assert!(matches!(decide(None, 2, &h(2, ck(3, 1, 1))), Decision::Refuse(_)), "a later life's checkpoint");
    assert_eq!(decide(Some(&h(2, ck(2, 3, 2))), 2, &c(2, ck(2, 3, 2))), Decision::Take, "a sleep marks its checkpoint clean");
    assert_eq!(decide(Some(&c(2, ck(2, 3, 2))), 2, &c(2, ck(2, 3, 2))), Decision::Same, "the clean post again");
    assert!(matches!(decide(Some(&c(2, ck(2, 3, 2))), 2, &h(2, ck(2, 3, 2))), Decision::Refuse(_)), "a clean head loses its mark");
    assert_eq!(decide(Some(&h(2, None)), 2, &c(2, None)), Decision::Take, "a life that shipped nothing sleeps cleanly");
    assert_eq!(decide(Some(&c(2, ck(2, 3, 2))), 3, &h(3, ck(2, 3, 2))), Decision::Take, "a new life after a clean sleep");
}

#[test]
#[ignore = "a debugging aid: SIM_SEED=<n> SIM_TRACE=1"]
fn one_seed() {
    let seed: u64 = std::env::var("SIM_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    run(seed, 12, 150, CALM);
}
