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

/// One cut, built, shipped and landed, with whatever cuts before it are
/// still unbuilt: the segments it shipped. A sleep's last turns.
pub fn flush_once(disk: &Disk, p: &Progress) -> Result<u32, StoreError> {
    let at = now_ms();
    let cut = disk.inner.lock().unwrap().store.cut()?;
    disk.cut.notify_all();
    let mut n = 0;
    // Bounded by the segments planned.
    loop {
        let batch: Vec<Outgoing> = {
            let mut g = disk.inner.lock().unwrap();
            let Inner { store, cache } = &mut *g;
            let mut b = Vec::new();
            while b.len() < PUTS_IN_FLIGHT {
                match store.build_next(cache)? {
                    Some(o) => b.push(o),
                    None => break,
                }
            }
            b
        };
        if batch.is_empty() {
            break;
        }
        ship_all(disk, &batch)?;
        let mut g = disk.inner.lock().unwrap();
        for o in &batch {
            let Kind::Segment(seq) = o.kind else { unreachable!("a cut is segments") };
            g.store.landed(seq);
        }
        drop(g);
        disk.cut.notify_all();
        p.segments.fetch_add(batch.len() as u64, Ordering::Relaxed);
        p.bytes.fetch_add(batch.iter().map(|o| o.body.len() as u64).sum(), Ordering::Relaxed);
        n += u32::try_from(batch.len()).expect("bounded");
    }
    if !cut.is_empty() {
        p.cuts.fetch_add(1, Ordering::Relaxed);
    }
    if n > 0 || !cut.is_empty() {
        p.durable_ms.store(at, Ordering::SeqCst);
    }
    Ok(n)
}

/// A checkpoint of what has landed, then the head names it.
/// `clean`: a sleep's, after everything landed; its head says so, and
/// when nothing changed since the last checkpoint the head is that one,
/// marked clean.
pub fn checkpoint(disk: &Disk, p: &Progress, clean: bool) -> Result<(), StoreError> {
    // a periodic checkpoint's crash pack first: what this life rewrote,
    // for a wake after a crash (a sleep's pack has landed, and drops it)
    if !clean && disk.inner.lock().unwrap().store.checkpoint_due() {
        let crash = {
            let mut g = disk.inner.lock().unwrap();
            let Inner { store, cache } = &mut *g;
            store.crash_pack(cache)?
        };
        if let Some(o) = crash {
            ship_all(disk, std::slice::from_ref(&o))?;
            let Kind::CrashPack(pr) = o.kind else { unreachable!("a crash pack") };
            disk.inner.lock().unwrap().store.crash_pack_landed(pr);
        }
    }
    // the pages it rewrites that are not loaded yet (only this thread lands
    // segments, so none becomes dirty meanwhile)
    let needs = disk.inner.lock().unwrap().store.checkpoint_needs();
    for pr in needs {
        // Bounded: an unavailable remote is retried a few times, then the
        // checkpoint waits for the next turn.
        let mut tries = 0;
        loop {
            match disk.load_page(pr) {
                Ok(()) => break,
                Err(StoreError::Unavailable(_)) if tries < 5 => tries += 1,
                Err(e) => return Err(e),
            }
        }
    }
    let (objs, epoch) = {
        let mut g = disk.inner.lock().unwrap();
        if !g.store.checkpoint_due() {
            let head = g.store.clean_head();
            drop(g);
            if clean && p.head_posted.load(Ordering::SeqCst) {
                post_head(disk, &head)?;
            }
            return Ok(());
        }
        (g.store.checkpoint(), g.store.epoch())
    };
    let (pages, last) = objs.split_at(objs.len() - 1);
    ship_all(disk, pages)?;
    ship_all(disk, last)?;
    disk.inner.lock().unwrap().store.checkpoint_landed(&objs);
    let Some(Kind::Checkpoint(cr)) = objs.last().map(|o| o.kind) else { unreachable!("a checkpoint ends its objects") };
    post_head(disk, &Head { epoch, checkpoint: Some(cr), clean })?;
    p.checkpoints.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// A put that ended: its segment and its bytes, and how.
struct Ended {
    seq: u64,
    bytes: u64,
    result: Result<(), StoreError>,
}

/// The flusher's loop, until a stop (docs/design.md, The sleep): a cut
/// each FLUSH_MS (at once when a write waits on a full cut), its segments
/// built from the cache as there is room, PUTS_IN_FLIGHT puts at once,
/// each segment landed as its put ends, and a checkpoint now and then.
/// Writes go on meanwhile, up to UNSHIPPED_BYTES_MAX unlanded. On a stop
/// it lets the puts in flight end and leaves the rest to `last`.
pub fn run(disk: Arc<Disk>, p: Arc<Progress>, status: impl Fn(&Disk, &Progress)) {
    if let Err(e) = first_head(&disk, &p) {
        fence(&disk, &e);
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel::<Ended>();
    let mut in_flight = 0usize;
    let (mut since, mut last_checkpoint, mut last_cut, mut last_status) = (0u32, Instant::now(), Instant::now(), Instant::now());
    // each cut's last segment and its time, until it is applied
    let mut cuts: std::collections::VecDeque<(u64, u64)> = Default::default();
    let mut seen = 0u64;
    // Unbounded on purpose: the life of the mount, until a stop.
    loop {
        let (stop, events) = disk.wait_flusher(seen, Duration::from_millis(FLUSH_MS));
        seen = events;
        // the puts that ended
        let mut ended: Vec<Ended> = rx.try_iter().collect();
        if stop {
            // Bounded by the puts in flight, each of which ends (landed or
            // fenced).
            while ended.len() < in_flight {
                ended.push(rx.recv().expect("a put thread answers"));
            }
        }
        if let Err(e) = land(&disk, &p, &mut ended, &mut in_flight, &mut since) {
            fence(&disk, &e);
            return;
        }
        let applied = disk.inner.lock().unwrap().store.applied_seq();
        while let Some(&(last, at)) = cuts.front() {
            if last > applied {
                break;
            }
            p.durable_ms.store(at, Ordering::SeqCst);
            cuts.pop_front();
        }
        if stop {
            status(&disk, &p);
            return;
        }
        // a cut at once when a write waits on a full one; otherwise each
        // FLUSH_MS while shipping keeps up. When it falls behind, the
        // timed cut waits, so rewrites of the same extents coalesce
        // rather than each second's copy queueing (the loss window grows
        // with the backlog: docs/contract.md)
        let wanted = disk.cut_wanted.swap(false, Ordering::SeqCst);
        let behind = disk.inner.lock().unwrap().store.planned() > PUTS_IN_FLIGHT;
        if wanted || (last_cut.elapsed() >= Duration::from_millis(FLUSH_MS) && !behind) {
            last_cut = Instant::now();
            let at = now_ms();
            match disk.inner.lock().unwrap().store.cut() {
                Ok(planned) => {
                    if let Some((seq, _)) = planned.last() {
                        cuts.push_back((*seq, at));
                        p.cuts.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    fence(&disk, &e);
                    return;
                }
            }
            disk.cut.notify_all();
        }
        // build and put, up to PUTS_IN_FLIGHT at once
        while in_flight < PUTS_IN_FLIGHT {
            let built = {
                let mut g = disk.inner.lock().unwrap();
                let Inner { store, cache } = &mut *g;
                store.build_next(cache)
            };
            match built {
                Ok(Some(o)) => {
                    in_flight += 1;
                    let (d, tx) = (disk.clone(), tx.clone());
                    std::thread::spawn(move || {
                        let Kind::Segment(seq) = o.kind else { unreachable!("a build is a segment") };
                        let result = ship_one(&d, &o);
                        let _ = tx.send(Ended { seq, bytes: o.body.len() as u64, result });
                        d.nudge();
                    });
                }
                Ok(None) => break,
                Err(e) => {
                    fence(&disk, &e);
                    return;
                }
            }
        }
        // the store stays as of its last landing while a checkpoint ships
        // (ended puts wait in the channel)
        if since >= CHECKPOINT_AFTER_SEGMENTS || (since > 0 && last_checkpoint.elapsed() >= Duration::from_millis(CHECKPOINT_MS)) {
            if let Err(e) = checkpoint(&disk, &p, false) {
                fence(&disk, &e);
                return;
            }
            since = 0;
            last_checkpoint = Instant::now();
        }
        if last_status.elapsed() >= Duration::from_millis(FLUSH_MS) {
            status(&disk, &p);
            last_status = Instant::now();
        }
    }
}

/// Lands the segments whose puts ended; the first failure fences.
fn land(disk: &Disk, p: &Progress, ended: &mut Vec<Ended>, in_flight: &mut usize, since: &mut u32) -> Result<(), StoreError> {
    if ended.is_empty() {
        return Ok(());
    }
    for e in ended.drain(..) {
        *in_flight -= 1;
        e.result?;
        disk.inner.lock().unwrap().store.landed(e.seq);
        *since += 1;
        p.segments.fetch_add(1, Ordering::Relaxed);
        p.bytes.fetch_add(e.bytes, Ordering::Relaxed);
    }
    disk.cut.notify_all();
    Ok(())
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
    checkpoint(disk, p, true)?;
    Ok(json!({ "segments": segments, "flushed_ms": flushed_ms, "pack_bytes": pack_bytes, "ms": t0.elapsed().as_millis() }))
}
