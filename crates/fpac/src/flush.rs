//! The flusher: one thread that posts this life's head before anything
//! ships, then about once a second cuts what changed and ships it, several
//! segments at once, and now and then writes a checkpoint and moves the
//! head (docs/design.md, The sleep). A sleep is its last turn.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fpac_format::limits::CHECKPOINT_SEGMENTS;
use fpac_remote::{Head, HeadAnswer, RemoteError};
use fpac_store::{ship, Kind, Outgoing, StoreError};
use serde_json::json;

use crate::disk::{Disk, Inner};

/// How often a cut is taken while something changed: the loss window
/// for synced writes (docs/contract.md).
pub const FLUSH_MS: u64 = 1000;
/// A checkpoint at least this often while anything changed, and after
/// half of CHECKPOINT_SEGMENTS, so an open never replays more than that.
pub const CHECKPOINT_MS: u64 = 5 * 60 * 1000;
pub const CHECKPOINT_AFTER_SEGMENTS: u32 = CHECKPOINT_SEGMENTS / 2;
/// Puts in flight at once.
pub const PUTS_IN_FLIGHT: usize = 8;
/// Tries of one put before a pause; then it tries again, as long as it
/// takes: the bytes have nowhere else to go, and writers wait at the
/// dirty limit meanwhile.
const PUT_TRIES: u32 = 3;
const PAUSE_MS_MAX: u64 = 5000;

pub fn now_ms() -> u64 {
    u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).expect("after 1970").as_millis()).expect("fits")
}

/// What the flusher has done, for `fpac status`.
#[derive(Default)]
pub struct Progress {
    pub cuts: AtomicU64,
    pub segments: AtomicU64,
    pub bytes: AtomicU64,
    pub checkpoints: AtomicU64,
    /// The wall time of the last cut whose every segment has landed: any
    /// write that finished before it is in the store.
    pub durable_ms: AtomicU64,
    pub head_posted: AtomicBool,
    /// Wakes the flusher early: a stop.
    pub wake: (std::sync::Mutex<bool>, std::sync::Condvar),
}

impl Progress {
    /// Asks the flusher to stop now rather than at its next tick.
    pub fn stop(&self) {
        *self.wake.0.lock().unwrap() = true;
        self.wake.1.notify_all();
    }

    /// Waits a tick, or less if a stop is asked: whether to stop.
    fn tick(&self) -> bool {
        let g = self.wake.0.lock().unwrap();
        let (g, _) = self.wake.1.wait_timeout_while(g, Duration::from_millis(FLUSH_MS), |stop| !*stop).unwrap();
        *g
    }
}

/// Puts objects, `PUTS_IN_FLIGHT` at a time, each until it lands. A
/// conflict fences this life.
pub fn ship_all(disk: &Disk, objs: &[Outgoing]) -> Result<(), StoreError> {
    for chunk in objs.chunks(PUTS_IN_FLIGHT) {
        let results: Vec<Result<(), StoreError>> = std::thread::scope(|s| {
            let hs: Vec<_> = chunk.iter().map(|o| s.spawn(move || ship_one(disk, o))).collect();
            hs.into_iter().map(|h| h.join().expect("a put thread")).collect()
        });
        for r in results {
            r?;
        }
    }
    Ok(())
}

fn ship_one(disk: &Disk, o: &Outgoing) -> Result<(), StoreError> {
    let mut pause = 100;
    // Unbounded on purpose (see PUT_TRIES): it ends when the put lands or
    // another writer fences this life.
    loop {
        let mut r = disk.remote();
        let got = ship(&mut r, o, PUT_TRIES);
        disk.give_back(r);
        match got {
            Ok(()) => return Ok(()),
            Err(StoreError::Unavailable(why)) => {
                eprintln!("{}", json!({ "fpac": "put-retry", "name": o.name, "why": why, "pause_ms": pause }));
                std::thread::sleep(Duration::from_millis(pause));
                pause = (pause * 2).min(PAUSE_MS_MAX);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Posts `head`, as long as the remote is down; a refusal fences.
pub fn post_head(disk: &Disk, head: &Head) -> Result<(), StoreError> {
    let mut pause = 100;
    // Unbounded on purpose: this life ships nothing until it lands.
    loop {
        let mut r = disk.remote();
        let got = r.post_head(head);
        disk.give_back(r);
        match got {
            Ok(HeadAnswer::Taken) | Ok(HeadAnswer::Same) => return Ok(()),
            Err(RemoteError::Unavailable(why)) => {
                eprintln!("{}", json!({ "fpac": "head-retry", "why": why }));
                std::thread::sleep(Duration::from_millis(pause));
                pause = (pause * 2).min(PAUSE_MS_MAX);
            }
            Err(e) => return Err(StoreError::Fenced(format!("the head: {e}"))),
        }
    }
}

/// The head before anything ships: a checkpoint of what was replayed (if
/// anything), then the head.
pub fn first_head(disk: &Disk, p: &Progress) -> Result<(), StoreError> {
    let (objs, head) = disk.inner.lock().unwrap().store.prepare_head();
    ship_all(disk, &objs)?;
    if !objs.is_empty() {
        disk.inner.lock().unwrap().store.checkpoint_landed(&objs);
        p.checkpoints.fetch_add(1, Ordering::Relaxed);
    }
    post_head(disk, &head)?;
    disk.inner.lock().unwrap().store.head_posted();
    p.head_posted.store(true, Ordering::SeqCst);
    Ok(())
}

/// One cut, shipped and landed: the segments it took.
pub fn flush_once(disk: &Disk, p: &Progress) -> Result<u32, StoreError> {
    let (objs, at) = {
        let mut g = disk.inner.lock().unwrap();
        let Inner { store, cache } = &mut *g;
        let at = now_ms();
        (store.cut(cache)?, at)
    };
    disk.cut.notify_all();
    if objs.is_empty() {
        return Ok(0);
    }
    ship_all(disk, &objs)?;
    {
        let mut g = disk.inner.lock().unwrap();
        for o in &objs {
            let Kind::Segment(seq) = o.kind else { unreachable!("a cut is segments") };
            g.store.landed(seq);
        }
    }
    p.cuts.fetch_add(1, Ordering::Relaxed);
    p.segments.fetch_add(objs.len() as u64, Ordering::Relaxed);
    p.bytes.fetch_add(objs.iter().map(|o| o.body.len() as u64).sum(), Ordering::Relaxed);
    p.durable_ms.store(at, Ordering::SeqCst);
    Ok(u32::try_from(objs.len()).expect("bounded"))
}

/// A checkpoint of what has landed, then the head names it.
pub fn checkpoint(disk: &Disk, p: &Progress) -> Result<(), StoreError> {
    let (objs, epoch) = {
        let mut g = disk.inner.lock().unwrap();
        if !g.store.checkpoint_due() {
            return Ok(());
        }
        (g.store.checkpoint(), g.store.epoch())
    };
    let (pages, last) = objs.split_at(objs.len() - 1);
    ship_all(disk, pages)?;
    ship_all(disk, last)?;
    disk.inner.lock().unwrap().store.checkpoint_landed(&objs);
    let Some(Kind::Checkpoint(cr)) = objs.last().map(|o| o.kind) else { unreachable!("a checkpoint ends its objects") };
    post_head(disk, &Head { epoch, checkpoint: Some(cr) })?;
    p.checkpoints.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// The flusher's loop, until `stop`.
pub fn run(disk: Arc<Disk>, p: Arc<Progress>, status: impl Fn(&Disk, &Progress)) {
    if let Err(e) = first_head(&disk, &p) {
        fence(&disk, &e);
        return;
    }
    let (mut since, mut last_checkpoint) = (0u32, Instant::now());
    // Unbounded on purpose: the life of the mount, until a stop.
    while !p.tick() {
        match flush_once(&disk, &p) {
            Ok(n) => since += n,
            Err(e) => {
                fence(&disk, &e);
                return;
            }
        }
        let due = since >= CHECKPOINT_AFTER_SEGMENTS || (since > 0 && last_checkpoint.elapsed() >= Duration::from_millis(CHECKPOINT_MS));
        if due {
            if let Err(e) = checkpoint(&disk, &p) {
                fence(&disk, &e);
                return;
            }
            since = 0;
            last_checkpoint = Instant::now();
        }
        status(&disk, &p);
    }
}

fn fence(disk: &Disk, e: &StoreError) {
    eprintln!("{}", json!({ "fpac": "fenced", "why": e.to_string() }));
    disk.fenced.store(true, Ordering::SeqCst);
    disk.inner.lock().unwrap().store.fence();
}

/// A sleep's last turn: everything cut and landed, the pack, a checkpoint
/// naming it, the head.
pub fn last(disk: &Disk, p: &Progress) -> Result<serde_json::Value, StoreError> {
    let t0 = Instant::now();
    if !p.head_posted.load(Ordering::SeqCst) {
        first_head(disk, p)?;
    }
    let mut segments = 0;
    // Bounded: after the unmount nothing writes, so a cut or two empties it.
    for _ in 0..4 {
        let n = flush_once(disk, p)?;
        segments += n;
        if n == 0 {
            break;
        }
    }
    let flushed_ms = t0.elapsed().as_millis();
    let pack = {
        let mut g = disk.inner.lock().unwrap();
        let Inner { store, cache } = &mut *g;
        store.stop_boot();
        store.pack(cache)?
    };
    let pack_bytes = pack.as_ref().map(|o| o.body.len()).unwrap_or(0);
    if let Some(o) = pack {
        ship_all(disk, std::slice::from_ref(&o))?;
        let Kind::Pack(pr) = o.kind else { unreachable!("a pack") };
        disk.inner.lock().unwrap().store.pack_landed(pr);
    }
    checkpoint(disk, p)?;
    Ok(json!({ "segments": segments, "flushed_ms": flushed_ms, "pack_bytes": pack_bytes, "ms": t0.elapsed().as_millis() }))
}
