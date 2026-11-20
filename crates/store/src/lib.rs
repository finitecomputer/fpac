//! `fpac-store`: the disk's state and every rule that changes it
//! (docs/design.md). It knows extents, segments, checkpoints and packs; it
//! knows nothing of FUSE, threads or HTTP. The world comes in through two
//! gates (`RemoteGate`, `CacheGate`) and a clock the caller reads.
//!
//! **The life of a disk.** A life opens from the head the platform kept
//! (`open`): it loads the head's checkpoint, replays the head life's
//! segments after it, and then, before it ships anything of its own,
//! writes a checkpoint if it replayed anything and posts its head
//! (`prepare_head`, `head_posted`). Reads come from the cache, or are
//! fetched from the store with read-ahead and checked extent by extent;
//! writes go to the cache. About once a second the caller cuts what
//! changed into segments (`cut`), ships them, several at once, and tells
//! the store as each lands (`landed`); the map moves only in segment
//! order, so what the store holds is always a real moment of the disk. Now
//! and then, and at a sleep, a checkpoint (`checkpoint`) bounds the next
//! open's replay, and a pack of the boot's reads (`pack`) makes the next
//! wake one fetch.
//!
//! **What is checked.** Every extent's bytes against their sum64, after
//! every fetch and every read of a clean extent from the cache: a wrong
//! one is fetched again, and if it stays wrong the read fails rather than
//! hand ext4 wrong bytes. Every object's header against its own SHA-256.
//! Every segment against the chain (its life, its number, and the header
//! it follows). A contradiction in our own state is corruption: the store
//! refuses to go on.

pub mod clean;
pub mod evict;
pub mod gates;
pub mod head;

use std::collections::{BTreeMap, BTreeSet};

use fpac_format::limits::*;
use fpac_format::{self as format, Checkpoint, Image, Link, Loc, PackRef, PageRef, Place, SegId, Sealer, Sum, GENESIS, SEALED_EXTENT_BYTES};
use fpac_remote::{Put, RemoteError};

pub use gates::{Ask, CacheGate, MemCache, RemoteGate};
pub use head::{CheckpointRef, Head};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The remote is slow, absent or failing: the caller tries later.
    #[error("the remote is unavailable: {0}")]
    Unavailable(String),
    /// Another writer put other bytes at one of this life's names. This
    /// life is fenced: it ships nothing more.
    #[error("fenced: {0} holds another writer's bytes")]
    Fenced(String),
    /// Bytes that fail their checksums after every try, or objects that
    /// contradict each other: the disk as stored cannot be trusted here.
    #[error("corrupt: {0}")]
    Corrupt(String),
    #[error("the cache file: {0}")]
    Cache(#[from] std::io::Error),
    /// DIRTY_EXTENTS_MAX are written and not yet cut: the write waits for
    /// the next cut (an operating error, retried).
    #[error("{0} extents wait to be cut: the write waits")]
    Full(u32),
    /// The remote refused a call it should take (a bug or a fence).
    #[error("refused: {0}")]
    Refused(String),
}

fn from_remote(e: RemoteError) -> StoreError {
    match e {
        RemoteError::Unavailable(s) => StoreError::Unavailable(s),
        RemoteError::Corrupt(s) => StoreError::Corrupt(s),
        RemoteError::Refused { status, detail } => StoreError::Refused(format!("{status}: {detail}")),
        RemoteError::Protocol(s) => StoreError::Refused(s),
    }
}

/// What the platform tells a life when it opens (docs/contract.md, open).
#[derive(Debug, Clone)]
pub struct OpenInfo {
    pub epoch: u64,
    pub volume_bytes: u64,
    pub head: Option<Head>,
    /// The disk's data key, when it is sealed (docs/encryption.md): every
    /// object this life reads must open with it, and every one it writes
    /// is sealed. A disk is sealed or plain for good: the other kind's
    /// objects are refused by their magic.
    pub seal: Option<std::sync::Arc<Sealer>>,
}

/// An object for the caller to put.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub name: String,
    pub body: Vec<u8>,
    pub kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Segment(u64),
    Page,
    Checkpoint(CheckpointRef),
    Pack(PackRef),
    CrashPack(PackRef),
}

/// Extents a wake after a crash fetches beside its mount, at most: the
/// newest its replay brought back, 64 MiB (ext4's recovery read 55
/// extents as misses after a crash on Cloudflare, 2.5 to 11.8 s).
pub const REPLAY_FETCH_EXTENTS: usize = 1024;

/// A periodic checkpoint's crash pack, at most: the extents a life
/// rewrote, newest first. ext4's journal, which a wake after a crash
/// replays, is rewritten commit by commit; 8 MiB every five minutes of
/// activity is about $0.0024 an hour of container egress (docs/costs.md).
pub const CRASH_PACK_EXTENTS: usize = 128;

/// What a read needs: nothing more, or a fetch the caller makes.
#[derive(Debug)]
pub enum Read {
    Done,
    Fetch(Fetch),
    /// The map page that places the extent is not loaded yet: fetch it
    /// (`page_name`), `install_page` it, and read again.
    Page(PageRef),
}

/// What a write needs first.
#[derive(Debug)]
pub enum Need {
    /// The extent's bytes (a partial write to an extent the cache does
    /// not hold).
    Fetch(Fetch),
    /// The map page that places it.
    Page(PageRef),
}

/// One ranged get of a segment: the extent asked for and the read-ahead
/// after it.
#[derive(Debug, Clone)]
pub struct Fetch {
    pub name: String,
    pub offset: u64,
    pub len: usize,
    extent: u64,
    run: Vec<(u64, Loc)>,
    /// An image's bytes in the object: 64 KiB, and its tag when sealed.
    stride: u32,
}

impl Fetch {
    /// The extents this fetch may install.
    pub fn extents(&self) -> impl Iterator<Item = u64> + '_ {
        self.run.iter().map(|(e, _)| *e)
    }

    /// The same fetch for the extent asked for alone.
    pub fn single(&self) -> Fetch {
        let first = self.run[0];
        Fetch { name: self.name.clone(), offset: self.offset, len: self.stride as usize, extent: self.extent, run: vec![first], stride: self.stride }
    }
}

/// An extent's state in the cache file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Local {
    /// Its bytes are the map's (checked against its sum when read).
    Clean,
    /// Written since the last cut: the cache is its truth.
    Dirty,
    /// Named by this life's cut segment `seq`, not yet built: its bytes as
    /// of the cut are the cache's until a write changes them, which keeps
    /// them apart first (`CacheGate::shadow_save`).
    Frozen(u64),
    /// Built into this life's segment `seq`, not yet landed; a write makes
    /// it Dirty again.
    Shipping(u64),
}

/// A segment a cut named and nobody has built yet.
#[derive(Debug)]
struct Planned {
    cut: u64,
    last_of_cut: bool,
    extents: Vec<u64>,
}

/// A segment taken to be built (`Store::build_take`): its extents' bytes
/// are read and encoded outside the disk's lock, between `build_take` and
/// `build_resolve`, and between that and `build_done`.
#[derive(Debug)]
pub struct Building {
    id: SegId,
    base: Link,
    cut: u64,
    last_of_cut: bool,
    extents: Vec<u64>,
    seal: Option<std::sync::Arc<Sealer>>,
}

impl Building {
    /// The extents whose bytes to read, in the segment's order.
    pub fn extents(&self) -> &[u64] {
        &self.extents
    }

    /// The segment's object, from its extents' bytes as of the cut (an
    /// extent of zeros ships as a zeroed entry, with no image). Pure, so
    /// a disk runs it outside its lock.
    pub fn encode(&self, bytes: &[Vec<u8>]) -> Vec<u8> {
        assert_eq!(bytes.len(), self.extents.len());
        let images: Vec<Image<'_>> = self.extents.iter().zip(bytes).map(|(e, b)| Image { extent: *e, bytes: if b.iter().all(|&x| x == 0) { None } else { Some(b.as_slice()) } }).collect();
        match &self.seal {
            Some(s) => format::encode_segment_sealed(s, self.id, self.base, self.cut, self.last_of_cut, &images),
            None => format::encode_segment(self.id, self.base, self.cut, self.last_of_cut, &images),
        }
    }
}

#[derive(Debug)]
struct Shipment {
    base: Link,
    link: Link,
    last_of_cut: bool,
    entries: Vec<(u64, Loc)>,
    landed: bool,
}

/// What an open did, for the caller's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub replayed: u32,
    pub extents_mapped: u64,
    /// Map pages the checkpoint named.
    pub pages: u32,
    pub pack: Option<PackRef>,
    pub crash_pack: Option<PackRef>,
}

pub struct Store {
    volume_bytes: u64,
    epoch: u64,
    seal: Option<std::sync::Arc<Sealer>>,
    /// Where each written extent's bytes are, as the store holds them.
    map: BTreeMap<u64, Loc>,
    /// Every segment the map may name, with its header's seal.
    segments: BTreeMap<SegId, Sum>,
    local: BTreeMap<u64, Local>,
    /// How many of `local` are Dirty (kept with every change of state).
    dirty: u32,
    /// Segments cut and not yet built, in order, and the extents they name.
    planned: BTreeMap<u64, Planned>,
    planned_extents: u64,
    /// The segment taken from `planned` and being built outside the
    /// disk's lock (`build_take`), at most one.
    building: Option<u64>,
    /// Extents in segments built and not yet landed.
    shipping_extents: u64,
    /// A frozen extent's bytes kept apart, by (extent, segment).
    shadows: BTreeSet<(u64, u64)>,
    /// What this life's next segment follows.
    chain_end: Link,
    /// What the map reflects: the last segment applied.
    applied_end: Link,
    next_seq: u64,
    next_cut: u64,
    /// This life's next checkpoint's and pack's numbers.
    next_checkpoint: u64,
    next_pack: u64,
    shipping: BTreeMap<u64, Shipment>,
    checkpoint: Option<Checkpoint>,
    checkpoint_ref: Option<CheckpointRef>,
    pages_dirty: BTreeSet<u32>,
    /// The checkpoint's map pages not loaded yet (docs/design.md, lazy
    /// pages): an extent of one is placed only once its page is fetched,
    /// on its first read, a partial write to it, or a checkpoint that
    /// rewrites the page. The map holds every other placement.
    unloaded: BTreeMap<u32, PageRef>,
    /// Segments applied since the last checkpoint (the live list's growth).
    segments_landed_since: usize,
    pack: Option<PackRef>,
    crash_pack: Option<PackRef>,
    head_posted: bool,
    fenced: bool,
    boot: Vec<u64>,
    boot_seen: BTreeSet<u64>,
    /// The extents a replay brought back, in the order its segments wrote
    /// them: a wake after a crash fetches the newest of them beside its
    /// mount (`replay_fetches`).
    replayed: Vec<u64>,
    /// The map pages this boot touched (at most BOOT_PAGES_MAX): its
    /// checkpoints name them for the next open's round.
    boot_pages: BTreeSet<u32>,
    /// Extents this life's cuts carried, the most recent last (each once):
    /// what the next boot is likely to read that this boot did not (a
    /// database's pages, a log's tail).
    written: std::collections::VecDeque<u64>,
    /// Extents this life's cuts carried more than once, the most recent
    /// last (each once): the crash pack's.
    rewritten: std::collections::VecDeque<u64>,
    /// A new disk's first life: the boot's writes are noted for the next
    /// wake's pack too (the files its first boot made are what its next
    /// boot reads).
    fresh: bool,
    recording: bool,
    opened: Opened,
}

/// A map page's object name.
pub fn page_name(pr: &PageRef) -> String {
    format::page_name(pr.epoch, pr.seq, pr.page)
}

fn page_of(extent: u64) -> u32 {
    u32::try_from(extent / PAGE_EXTENTS as u64).expect("bounded by the volume")
}


impl Store {
    /// Opens a life (docs/design.md, The wake): the head's checkpoint, then
    /// the head life's segments after it, in order, each one following the
    /// last, until one is absent. A cut whose last segment is absent is
    /// left out whole.
    pub fn open(remote: &mut dyn RemoteGate, info: &OpenInfo) -> Result<Store, StoreError> {
        assert!(info.epoch > 0);
        assert!(info.volume_bytes > 0 && info.volume_bytes <= VOLUME_BYTES_MAX && info.volume_bytes.is_multiple_of(EXTENT_BYTES as u64));
        let mut s = Store {
            volume_bytes: info.volume_bytes,
            epoch: info.epoch,
            seal: info.seal.clone(),
            map: BTreeMap::new(),
            segments: BTreeMap::new(),
            local: BTreeMap::new(),
            dirty: 0,
            planned: BTreeMap::new(),
            planned_extents: 0,
            building: None,
            shipping_extents: 0,
            shadows: BTreeSet::new(),
            chain_end: GENESIS,
            applied_end: GENESIS,
            next_seq: 1,
            next_cut: 1,
            next_checkpoint: 1,
            next_pack: 1,
            shipping: BTreeMap::new(),
            checkpoint: None,
            checkpoint_ref: None,
            pages_dirty: BTreeSet::new(),
            unloaded: BTreeMap::new(),
            segments_landed_since: 0,
            pack: None,
            crash_pack: None,
            head_posted: false,
            fenced: false,
            boot: Vec::new(),
            boot_seen: BTreeSet::new(),
            boot_pages: BTreeSet::new(),
            replayed: Vec::new(),
            written: std::collections::VecDeque::new(),
            rewritten: std::collections::VecDeque::new(),
            fresh: info.head.is_none(),
            recording: false,
            opened: Opened { replayed: 0, extents_mapped: 0, pages: 0, pack: None, crash_pack: None },
        };
        let Some(head) = info.head else {
            return Ok(s);
        };
        assert!(head.epoch < info.epoch, "a new life is later than the head's");
        let mut first_seq = 1;
        if let Some(cr) = head.checkpoint {
            let c = s.fetch_checkpoint(remote, cr)?;
            if c.volume_bytes > info.volume_bytes {
                return Err(StoreError::Corrupt(format!("the disk is {} bytes; this life was given {}", c.volume_bytes, info.volume_bytes)));
            }
            // the head life's segments after the chain's end the
            // checkpoint covers (none of them, if it ends in another life)
            if c.end.epoch == head.epoch {
                first_seq = c.end.seq + 1;
            }
            remote.prefetch(&Self::open_asks(&c, head.epoch, if head.clean { None } else { Some(first_seq) }, s.seal.is_some()));
            s.unloaded = c.pages.iter().map(|p| (p.page, *p)).collect();
            s.segments = c.segments.iter().copied().collect();
            s.chain_end = c.end;
            s.applied_end = c.end;
            s.pack = c.pack;
            s.crash_pack = c.crash_pack;
            s.checkpoint = Some(c);
            s.checkpoint_ref = Some(cr);
        }
        // a clean sleep's checkpoint covers everything its life shipped
        if !head.clean {
            s.replay(remote, head.epoch, first_seq)?;
        }
        // the pages a replay changed are loaded now: the head's preparation
        // checkpoints them (a wake after a crash)
        let need = s.open_needs();
        if !need.is_empty() {
            remote.prefetch(&need.iter().map(|p| Ask::Whole(page_name(p))).collect::<Vec<_>>());
            for pr in need {
                s.load_page(remote, pr)?;
            }
        }
        // the pages the last boot touched, fetched with the round
        let boot: Vec<PageRef> = s.checkpoint.as_ref().map(|c| c.boot_pages.iter().filter_map(|p| s.unloaded.get(p).copied()).collect()).unwrap_or_default();
        for pr in boot {
            s.load_page(remote, pr)?;
        }
        let pages = s.checkpoint.as_ref().map_or(0, |c| c.pages.len() as u32);
        s.opened = Opened { replayed: s.opened.replayed, extents_mapped: s.map.len() as u64, pages, pack: s.pack, crash_pack: s.crash_pack };
        assert_eq!(s.chain_end, s.applied_end, "nothing of this life is shipping yet");
        Ok(s)
    }

    fn fetch_checkpoint(&mut self, remote: &mut dyn RemoteGate, cr: CheckpointRef) -> Result<Checkpoint, StoreError> {
        let name = format::checkpoint_name(cr.epoch, cr.seq);
        let body = whole(remote, &name, self.whole_max(CHECKPOINT_BYTES_MAX))?;
        if format::sha256(&body) != cr.sum {
            return Err(StoreError::Corrupt(format!("{name} is not the checkpoint the head names")));
        }
        let c = format::decode_checkpoint(&self.verified_checkpoint(&name, &body)?).map_err(|e| StoreError::Corrupt(format!("{name}: {e}")))?;
        if (c.epoch, c.seq) != (cr.epoch, cr.seq) {
            return Err(StoreError::Corrupt(format!("{name} names itself otherwise")));
        }
        Ok(c)
    }

    /// What an open reads after its checkpoint, asked for at once: the
    /// first OPEN_SEGMENT_HEADERS segment headers a replay may read, and
    /// the pack's head (its header and first extents,
    /// `PACK_HEAD_FETCH_BYTES`). No map page: each comes when first needed.
    pub fn open_asks(c: &Checkpoint, epoch: u64, first_seq: Option<u64>, sealed: bool) -> Vec<Ask> {
        let mut asks: Vec<Ask> = Vec::new();
        let mut heads: Vec<Ask> = [c.pack, c.crash_pack].into_iter().flatten().map(|pr| Ask::Range { name: format::pack_name(pr.epoch, pr.seq), offset: 0, len: pack_head_fetch_bytes(sealed) }).collect();
        // and the pages the last boot touched
        heads.extend(c.pages.iter().filter(|p| c.boot_pages.contains(&p.page)).map(|p| Ask::Whole(page_name(p))));
        let Some(first_seq) = first_seq else { return heads };
        asks.extend((first_seq..first_seq + u64::from(OPEN_SEGMENT_HEADERS)).map(|seq| Ask::Range { name: format::segment_name(SegId { epoch, seq }), offset: 0, len: segment_header_fetch_bytes(sealed) }));
        asks.extend(heads);
        asks
    }

    /// A map page fetched (`Read::Page`, `Need::Page`, `checkpoint_needs`):
    /// its placements join the map where the map has none (anything there
    /// is newer: a replay, a landing, the pack). A second install of the
    /// same page is a no-op.
    pub fn install_page(&mut self, pr: PageRef, body: &[u8]) -> Result<(), StoreError> {
        if self.unloaded.get(&pr.page) != Some(&pr) {
            return Ok(());
        }
        let pname = page_name(&pr);
        if format::sha256(body) != pr.sum {
            return Err(StoreError::Corrupt(format!("{pname} is not the page its checkpoint names")));
        }
        let (page, entries) = format::decode_page(&self.opened_object(&pname, body)?).map_err(|e| StoreError::Corrupt(format!("{pname}: {e}")))?;
        if page != pr.page {
            return Err(StoreError::Corrupt(format!("{pname} is page {page}")));
        }
        for (i, loc) in entries {
            if !loc.is_zero() && !self.segments.contains_key(&loc.seg) {
                return Err(StoreError::Corrupt(format!("{pname} names a segment its checkpoint does not list")));
            }
            let extent = u64::from(page) * PAGE_EXTENTS as u64 + u64::from(i);
            if extent >= self.extents() {
                return Err(StoreError::Corrupt(format!("{pname} maps an extent past the volume")));
            }
            self.map.entry(extent).or_insert(loc);
        }
        self.unloaded.remove(&pr.page);
        Ok(())
    }

    /// Fetches and installs a page.
    fn load_page(&mut self, remote: &mut dyn RemoteGate, pr: PageRef) -> Result<(), StoreError> {
        let body = whole(remote, &page_name(&pr), self.whole_max(PAGE_BYTES_MAX))?;
        self.install_page(pr, &body)
    }

    /// After a crash, the newest extents the replay brought back (the
    /// crashed life's last writes: ext4's journal and metadata, which the
    /// mount's recovery reads), up to REPLAY_FETCH_EXTENTS, as fetches of
    /// runs within a segment: fetch them beside the mount and `install`
    /// each. Empty after a clean sleep.
    pub fn replay_fetches(&self) -> Vec<Fetch> {
        let mut seen = BTreeSet::new();
        let mut want: Vec<u64> = Vec::new();
        for &e in self.replayed.iter().rev() {
            if want.len() == REPLAY_FETCH_EXTENTS || !seen.insert(e) {
                continue;
            }
            want.push(e);
        }
        self.fetches_for(want, READ_AHEAD_EXTENTS as usize)
    }

    /// A sequential read's next extents (docs/design.md, Read-ahead): those
    /// in `from..to` the cache does not hold and the map places, as
    /// fetches of runs within a segment of at most `run_max` extents, for
    /// the caller to make in parallel and `install` (each extent once,
    /// whoever asks). An extent whose page is not loaded is left to its
    /// read.
    pub fn ahead(&self, from: u64, to: u64, run_max: usize) -> Vec<Fetch> {
        assert!(to >= from && to - from <= u64::from(AHEAD_EXTENTS_MAX), "a read-ahead window within its limit");
        self.fetches_for((from..to.min(self.extents())).collect(), run_max)
    }

    /// The fetches for `extents` the cache does not hold and the map
    /// places: runs of consecutive places in one segment, at most
    /// `run_max` extents each.
    fn fetches_for(&self, extents: Vec<u64>, run_max: usize) -> Vec<Fetch> {
        assert!(run_max >= 1 && run_max <= SEGMENT_EXTENTS_MAX as usize);
        let mut want: Vec<(u64, Loc)> = extents.into_iter().filter(|e| !self.local.contains_key(e)).filter_map(|e| self.map.get(&e).copied().filter(|l| !l.is_zero()).map(|l| (e, l))).collect();
        want.sort_by_key(|(_, l)| (l.seg, l.offset));
        let mut out: Vec<Fetch> = Vec::new();
        for (e, l) in want {
            if let Some(f) = out.last_mut() {
                let (_, last) = *f.run.last().expect("non-empty");
                if last.seg == l.seg && last.offset + self.stride() == l.offset && f.run.len() < run_max {
                    f.run.push((e, l));
                    f.len += self.stride() as usize;
                    continue;
                }
            }
            out.push(Fetch { name: format::segment_name(l.seg), offset: u64::from(l.offset), len: self.stride() as usize, extent: e, run: vec![(e, l)], stride: self.stride() });
        }
        out
    }

    /// One ranged get of segment `seg` covering each of `extents` the
    /// cache does not hold and the map places there, from the first to
    /// the last (the dead images between come along, unused): how
    /// compaction reads a mostly dead segment in one round trip rather
    /// than one per run of live extents. None when none needs fetching.
    pub fn segment_fetch(&self, seg: SegId, extents: &[u64]) -> Option<Fetch> {
        let mut run: Vec<(u64, Loc)> = extents.iter().filter(|e| !self.local.contains_key(e)).filter_map(|e| self.map.get(e).copied().filter(|l| !l.is_zero() && l.seg == seg).map(|l| (*e, l))).collect();
        run.sort_by_key(|(_, l)| l.offset);
        let (first, last) = (run.first()?.1, run.last()?.1);
        let len = u64::from(last.offset - first.offset + self.stride());
        assert!(len <= SEGMENT_BYTES_MAX, "a fetch within one segment");
        Some(Fetch { name: format::segment_name(seg), offset: u64::from(first.offset), len: len as usize, extent: run[0].0, run, stride: self.stride() })
    }

    /// The pages an open loads before its head: those its replay changed,
    /// which the head's checkpoint rewrites. Every page only when that
    /// checkpoint's list (the last one's and the segments replayed) could
    /// pass LIVE_SEGMENTS_MAX: past half of it, `checkpoint_needs` loads
    /// every page at the life's first checkpoint instead, in the flusher,
    /// so the open stays the same size however much the disk holds.
    fn open_needs(&self) -> Vec<PageRef> {
        self.needs(self.opened.replayed as usize, LIVE_SEGMENTS_MAX)
    }

    /// The pages a checkpoint would rewrite that are not loaded: fetch and
    /// install each before `checkpoint`.
    /// When the live list (a superset while pages are unloaded) passes
    /// half its limit, every page: the checkpoint then lists exactly the
    /// segments the map names. A sleep's checkpoint loads every page only
    /// when the list would pass the limit itself: a sleep has 4 s, and at
    /// 100 GB every page is about 70 MB.
    pub fn checkpoint_needs(&self, sleep: bool) -> Vec<PageRef> {
        self.needs(self.segments_landed_since, if sleep { LIVE_SEGMENTS_MAX } else { LIVE_SEGMENTS_MAX / 2 })
    }

    /// The pages not loaded that a checkpoint now rewrites; every page not
    /// loaded when the last checkpoint's list and `since` more segments
    /// pass `limit`.
    fn needs(&self, since: usize, limit: u32) -> Vec<PageRef> {
        let superset = self.checkpoint.as_ref().map_or(0, |c| c.segments.len()) + since;
        if !self.unloaded.is_empty() && superset > limit as usize {
            return self.unloaded.values().copied().collect();
        }
        self.pages_dirty.iter().filter_map(|p| self.unloaded.get(p).copied()).collect()
    }

    /// The head a sleep posts when its last checkpoint covers everything
    /// it shipped (nothing new to checkpoint): the same, marked clean.
    pub fn clean_head(&self) -> Head {
        Head { epoch: self.epoch, checkpoint: self.checkpoint_ref, clean: true }
    }

    /// Map pages not loaded yet.
    pub fn pages_unloaded(&self) -> usize {
        self.unloaded.len()
    }

    /// Applies life `epoch`'s segments from `first`, each following the
    /// chain's end, until one is absent. Bounded by REPLAY_SEGMENTS_MAX:
    /// a checkpoint is written every CHECKPOINT_SEGMENTS, so one more is
    /// corruption.
    fn replay(&mut self, remote: &mut dyn RemoteGate, epoch: u64, first: u64) -> Result<(), StoreError> {
        let mut group: Vec<format::SegmentHeader> = Vec::new();
        let mut seq = first;
        let mut buf = vec![0u8; segment_header_fetch_bytes(self.seal.is_some())];
        loop {
            if seq - first >= u64::from(REPLAY_SEGMENTS_MAX) + u64::from(CUT_SEGMENTS_MAX) {
                return Err(StoreError::Corrupt(format!("more than {REPLAY_SEGMENTS_MAX} segments after a checkpoint")));
            }
            let name = format::segment_name(SegId { epoch, seq });
            let Some(h) = header(remote, self.seal.as_deref(), SegId { epoch, seq }, &mut buf)? else { break };
            let expected_base = group.last().map(|g| g.link()).unwrap_or(self.chain_end);
            if h.id != (SegId { epoch, seq }) || h.base != expected_base {
                return Err(StoreError::Corrupt(format!("{name} does not follow the chain")));
            }
            let last = h.last_of_cut;
            group.push(h);
            if last {
                for g in group.drain(..) {
                    self.apply(&g);
                    self.opened.replayed += 1;
                }
            }
            seq += 1;
        }
        // a cut whose last segment never landed is left out whole
        Ok(())
    }

    fn apply(&mut self, h: &format::SegmentHeader) {
        let link = h.link();
        assert_eq!(h.base, self.applied_end, "segments are applied in chain order");
        for e in &h.entries {
            assert!(e.extent < self.extents(), "a segment maps an extent past the volume");
            self.map.insert(e.extent, Loc { seg: h.id, offset: e.offset, len: e.len, sum: e.sum });
            self.pages_dirty.insert(page_of(e.extent));
            if e.len > 0 {
                self.replayed.push(e.extent);
            }
        }
        self.segments.insert(h.id, link.sum);
        self.applied_end = link;
        self.chain_end = link;
    }

    pub fn extents(&self) -> u64 {
        self.volume_bytes / EXTENT_BYTES as u64
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn opened(&self) -> &Opened {
        &self.opened
    }

    pub fn dirty_extents(&self) -> usize {
        debug_assert_eq!(self.dirty as usize, self.local.values().filter(|l| **l == Local::Dirty).count());
        self.dirty as usize
    }

    /// Changes an extent's local state, keeping the dirty count.
    fn set_local(&mut self, extent: u64, to: Option<Local>) {
        let was = match to {
            Some(l) => self.local.insert(extent, l),
            None => self.local.remove(&extent),
        };
        if was == Some(Local::Dirty) {
            self.dirty -= 1;
        }
        if to == Some(Local::Dirty) {
            self.dirty += 1;
        }
        assert!(self.dirty <= DIRTY_EXTENTS_MAX, "a write past the dirty limit was let in");
    }

    pub fn shipping(&self) -> usize {
        self.shipping.len()
    }

    /// Whether this disk's objects are sealed.
    pub fn sealed(&self) -> bool {
        self.seal.is_some()
    }

    /// A whole object's limit as stored: its plain limit, and its seal's
    /// magic and tag on a sealed disk.
    fn whole_max(&self, plain: u64) -> u64 {
        plain + if self.seal.is_some() { format::seal::SEALED_OBJECT_EXTRA_BYTES } else { 0 }
    }

    /// An image's bytes in a segment: 64 KiB, and its tag when sealed.
    fn stride(&self) -> u32 {
        if self.seal.is_some() { SEALED_EXTENT_BYTES } else { EXTENT_BYTES }
    }

    /// A whole object (a page, a checkpoint) as stored: sealed under its
    /// name on a sealed disk.
    fn sealed_object(&self, name: &str, plain: Vec<u8>) -> Vec<u8> {
        match &self.seal {
            Some(s) => s.seal_object(name, &plain),
            None => plain,
        }
    }

    /// A checkpoint's plain bytes, from those stored: on a sealed disk,
    /// checked against its tag.
    fn verified_checkpoint(&self, name: &str, body: &[u8]) -> Result<Vec<u8>, StoreError> {
        match &self.seal {
            Some(s) => s.verify_object(name, body).map_err(|e| StoreError::Corrupt(format!("{name}: {e}"))),
            None => Ok(body.to_vec()),
        }
    }

    /// A whole object's plain bytes, from those stored.
    fn opened_object(&self, name: &str, body: &[u8]) -> Result<Vec<u8>, StoreError> {
        match &self.seal {
            Some(s) => s.open_object(name, body).map_err(|e| StoreError::Corrupt(format!("{name}: {e}"))),
            None => Ok(body.to_vec()),
        }
    }

    /// Segments cut and not yet built.
    pub fn planned(&self) -> usize {
        self.planned.len()
    }

    /// Images written and not yet landed: dirty, planned or in flight
    /// (an extent written again after its cut counts twice). Writes wait
    /// past UNSHIPPED_EXTENTS_MAX.
    pub fn unshipped_extents(&self) -> u64 {
        u64::from(self.dirty) + self.planned_extents + self.shipping_extents
    }

    /// The last segment the map reflects: every cut up to it has landed.
    pub fn applied_seq(&self) -> u64 {
        if self.applied_end.epoch == self.epoch { self.applied_end.seq } else { 0 }
    }

    /// Whether a write that is not to a dirty extent must wait.
    fn full(&self) -> bool {
        self.dirty >= DIRTY_EXTENTS_MAX || self.unshipped_extents() >= UNSHIPPED_EXTENTS_MAX
    }

    /// Before the cache's bytes for `extent` change: a frozen extent's
    /// bytes as of its cut are kept apart for its segment.
    fn keep_frozen(&mut self, cache: &mut dyn CacheGate, extent: u64) -> Result<(), StoreError> {
        if let Some(Local::Frozen(seq)) = self.local.get(&extent).copied() {
            cache.shadow_save(extent, seq)?;
            assert!(self.shadows.insert((extent, seq)), "a frozen extent is kept apart once");
        }
        Ok(())
    }

    /// Extents the cache holds (clean, dirty or shipping).
    pub fn cached_extents(&self) -> usize {
        self.local.len()
    }

    /// Whether the cache holds `extent` as the store's bytes (clean) and it
    /// may leave: not one the boot read (or, on a new disk, wrote), which
    /// the next pack takes from the cache (at most PACK_EXTENTS_MAX, so
    /// 100 MB stays).
    pub fn may_leave(&self, extent: u64) -> bool {
        self.local.get(&extent) == Some(&Local::Clean) && !self.boot_seen.contains(&extent)
    }

    /// Whether the cache holds `extent`.
    pub fn is_cached(&self, extent: u64) -> bool {
        self.local.contains_key(&extent)
    }

    /// Drops a clean extent from the cache: its bytes are the store's, and
    /// a later read fetches them again. A dirty or shipping extent stays
    /// (the cache is its only copy). Answers whether it was dropped.
    pub fn evict(&mut self, cache: &mut dyn CacheGate, extent: u64) -> Result<bool, StoreError> {
        if self.local.get(&extent) != Some(&Local::Clean) {
            return Ok(false);
        }
        cache.punch(extent)?;
        self.set_local(extent, None);
        Ok(true)
    }

    /// What must land before this life may ship: a checkpoint of what it
    /// replayed (if anything), then its head. Answers the objects to put,
    /// in order (pages before their checkpoint), and the head to post
    /// once they have all landed.
    pub fn prepare_head(&mut self) -> (Vec<Outgoing>, Head) {
        assert!(!self.head_posted);
        if self.opened.replayed > 0 {
            let out = self.checkpoint_objects();
            let cr = match out.last().map(|o| o.kind) {
                Some(Kind::Checkpoint(cr)) => cr,
                _ => unreachable!("a replay leaves pages to write"),
            };
            return (out, Head { epoch: self.epoch, checkpoint: Some(cr), clean: false });
        }
        (Vec::new(), Head { epoch: self.epoch, checkpoint: self.checkpoint_ref, clean: false })
    }

    /// The head is posted: this life may ship.
    pub fn head_posted(&mut self) {
        self.head_posted = true;
    }

    /// Starts noting the extents read, in order, for the next wake's pack.
    pub fn record_boot(&mut self) {
        self.recording = true;
    }

    pub fn stop_boot(&mut self) {
        self.recording = false;
    }

    fn note(&mut self, extent: u64) {
        if self.recording && self.boot.len() < PACK_EXTENTS_MAX as usize && self.boot_seen.insert(extent) {
            self.boot.push(extent);
        }
        self.note_page(extent);
    }

    /// The boot touched `extent`'s map page.
    fn note_page(&mut self, extent: u64) {
        if self.recording && self.boot_pages.len() < BOOT_PAGES_MAX as usize {
            self.boot_pages.insert(page_of(extent));
        }
    }

    /// Reads extent `extent` into `buf` (one extent long): from the cache
    /// when it holds it, else from the store, with read-ahead, each extent
    /// checked, a wrong one fetched again (FETCH_TRIES_MAX tries in all).
    /// This composes `read_local`, the remote's ranged get and `install`,
    /// which a caller that must not hold its lock across the network calls
    /// apart.
    pub fn read(&mut self, remote: &mut dyn RemoteGate, cache: &mut dyn CacheGate, extent: u64, buf: &mut [u8]) -> Result<(), StoreError> {
        // Bounded: a page, then the extent.
        for _ in 0..3 {
            match self.read_local(cache, extent, buf)? {
                Read::Done => return Ok(()),
                Read::Fetch(f) => return self.fetch(remote, cache, f, buf),
                Read::Page(pr) => self.load_page(remote, pr)?,
            }
        }
        unreachable!("a loaded page places the extent")
    }

    fn fetch(&mut self, remote: &mut dyn RemoteGate, cache: &mut dyn CacheGate, f: Fetch, buf: &mut [u8]) -> Result<(), StoreError> {
        let mut f = f;
        let mut window = vec![0u8; f.len];
        let mut last = String::new();
        for _try in 0..FETCH_TRIES_MAX {
            window.resize(f.len, 0);
            match remote.get_range(&f.name, f.offset, &mut window) {
                Ok(Some(n)) => {
                    if self.install(cache, &f, &window[..n], buf)? {
                        return Ok(());
                    }
                    last = format!("{} at {}: {n} bytes, the extent's checksum does not match", f.name, f.offset);
                }
                Ok(None) => return Err(StoreError::Corrupt(format!("{} is gone, and the map names it", f.name))),
                Err(RemoteError::Unavailable(s)) => last = s,
                Err(e) => return Err(from_remote(e)),
            }
            // the next try fetches only the extent asked for
            f = f.single();
        }
        Err(StoreError::Corrupt(format!("extent {}, after {FETCH_TRIES_MAX} tries: {last}", f.extent)))
    }

    /// The local half of a read: answered from the cache (or as zeros),
    /// or a fetch for the caller to make.
    pub fn read_local(&mut self, cache: &mut dyn CacheGate, extent: u64, buf: &mut [u8]) -> Result<Read, StoreError> {
        assert_eq!(buf.len(), EXTENT_BYTES as usize);
        assert!(extent < self.extents());
        self.note(extent);
        match self.local.get(&extent).copied() {
            Some(Local::Dirty) | Some(Local::Frozen(_)) | Some(Local::Shipping(_)) => {
                cache.read(extent, buf)?;
                return Ok(Read::Done);
            }
            Some(Local::Clean) => {
                // a sealed disk's cache is sealed too: its seal checks the
                // bytes (the map's sum is the store's tag, not theirs), and
                // one that does not open is a lie like a wrong sum
                let opened = match cache.read(extent, buf) {
                    Ok(()) => true,
                    Err(_) if self.seal.is_some() => false,
                    Err(e) => return Err(e.into()),
                };
                let right = opened
                    && match self.map.get(&extent) {
                        Some(l) if !l.is_zero() => self.seal.is_some() || format::sum64(buf) == l.sum,
                        _ => buf.iter().all(|&b| b == 0),
                    };
                if right {
                    return Ok(Read::Done);
                }
                // A cache that lies: dropped, and fetched again.
                self.set_local(extent, None);
            }
            None => {}
        }
        match self.map.get(&extent).copied() {
            Some(l) if !l.is_zero() => Ok(Read::Fetch(self.plan(extent, l))),
            None if self.unloaded.contains_key(&page_of(extent)) => Ok(Read::Page(self.unloaded[&page_of(extent)])),
            _ => {
                buf.fill(0);
                Ok(Read::Done)
            }
        }
    }

    /// A fetch of `extent` and the extents after it in the same segment,
    /// up to READ_AHEAD_EXTENTS, that the cache does not hold: one ranged
    /// get.
    fn plan(&self, extent: u64, loc: Loc) -> Fetch {
        let mut run = vec![(extent, loc)];
        // Bounded: READ_AHEAD_EXTENTS.
        while run.len() < READ_AHEAD_EXTENTS as usize {
            let (pe, pl) = *run.last().expect("non-empty");
            let next = pe + 1;
            if next >= self.extents() || self.local.contains_key(&next) {
                break;
            }
            match self.map.get(&next) {
                Some(l) if !l.is_zero() && l.seg == pl.seg && l.offset == pl.offset + self.stride() => run.push((next, *l)),
                _ => break,
            }
        }
        Fetch { name: format::segment_name(loc.seg), offset: u64::from(loc.offset), len: run.len() * self.stride() as usize, extent, run, stride: self.stride() }
    }

    /// Installs what a fetch brought (`window`, possibly short): each
    /// extent whose bytes match its sum64, and that the map still places
    /// there and the cache does not hold by now, goes to the cache as
    /// clean. Answers whether the extent asked for is in `buf`; false when
    /// its bytes were wrong (the caller fetches it again). Each extent is
    /// at its own place in the window (a segment's fetch has gaps).
    pub fn install(&mut self, cache: &mut dyn CacheGate, f: &Fetch, window: &[u8], buf: &mut [u8]) -> Result<bool, StoreError> {
        let stride = f.stride as usize;
        let mut asked = false;
        let mut plain = vec![0u8; EXTENT_BYTES as usize];
        for (e, l) in &f.run {
            let at = usize::try_from(u64::from(l.offset) - f.offset).expect("within a segment");
            let Some(got) = window.get(at..at + stride) else { break };
            let placed = self.map.get(e) == Some(l);
            let held = self.local.contains_key(e);
            if *e == f.extent && held {
                // written or fetched meanwhile: the cache is its truth now
                cache.read(*e, buf)?;
                asked = true;
                continue;
            }
            let image: &[u8] = match &self.seal {
                Some(s) => match s.open_extent(Place::Image { seg: l.seg, extent: *e, offset: l.offset }, got, l.sum, &mut plain) {
                    Ok(()) => &plain,
                    Err(_) => continue,
                },
                None if format::sum64(got) == l.sum => got,
                None => continue,
            };
            if placed && !held {
                cache.write(*e, image)?;
                self.set_local(*e, Some(Local::Clean));
                if *e == f.extent {
                    buf.copy_from_slice(image);
                    asked = true;
                }
            }
        }
        if !asked && self.map.get(&f.extent) != f.run.first().map(|(_, l)| l) {
            // the map moved meanwhile (a landed cut): read it anew
            return Err(StoreError::Unavailable("the extent moved while it was fetched; read again".into()));
        }
        Ok(asked)
    }

    /// Writes `data` at `offset` within extent `extent`. A partial write
    /// to an extent the cache does not hold reads it first.
    pub fn write(&mut self, remote: &mut dyn RemoteGate, cache: &mut dyn CacheGate, extent: u64, offset: u32, data: &[u8]) -> Result<(), StoreError> {
        // Bounded: a write needs at most a page and a fetch, then lands.
        for _ in 0..3 {
            match self.write_local(cache, extent, offset, data)? {
                None => return Ok(()),
                Some(Need::Fetch(f)) => {
                    let mut buf = vec![0u8; EXTENT_BYTES as usize];
                    self.fetch(remote, cache, f, &mut buf)?;
                }
                Some(Need::Page(pr)) => self.load_page(remote, pr)?,
            }
        }
        unreachable!("a fetched extent is held by the cache")
    }

    /// The local half of a write: done, or the fetch it needs first (a
    /// partial write to an extent the cache does not hold); after that
    /// fetch is installed, call it again.
    pub fn write_local(&mut self, cache: &mut dyn CacheGate, extent: u64, offset: u32, data: &[u8]) -> Result<Option<Need>, StoreError> {
        assert!(extent < self.extents());
        assert!(offset as usize + data.len() <= EXTENT_BYTES as usize && !data.is_empty());
        if self.fenced {
            return Err(StoreError::Fenced("this life".into()));
        }
        let was = self.local.get(&extent).copied();
        if was != Some(Local::Dirty) && self.full() {
            return Err(StoreError::Full(self.dirty));
        }
        if self.fresh {
            self.note(extent);
        } else if was.is_none() {
            // a write to an extent the cache does not hold may need its page
            self.note_page(extent);
        }
        let whole_extent = offset == 0 && data.len() == EXTENT_BYTES as usize;
        if whole_extent {
            self.keep_frozen(cache, extent)?;
            cache.write(extent, data)?;
        } else {
            let mut buf = vec![0u8; EXTENT_BYTES as usize];
            match (was, self.map.get(&extent).copied()) {
                (Some(_), _) => cache.read(extent, &mut buf)?,
                (None, Some(l)) if !l.is_zero() => return Ok(Some(Need::Fetch(self.plan(extent, l)))),
                (None, None) if self.unloaded.contains_key(&page_of(extent)) => return Ok(Some(Need::Page(self.unloaded[&page_of(extent)]))),
                (None, _) => {}
            }
            buf[offset as usize..offset as usize + data.len()].copy_from_slice(data);
            self.keep_frozen(cache, extent)?;
            cache.write(extent, &buf)?;
        }
        self.set_local(extent, Some(Local::Dirty));
        Ok(None)
    }

    /// The extent reads as zeros from now on (a discard).
    pub fn punch(&mut self, cache: &mut dyn CacheGate, extent: u64) -> Result<(), StoreError> {
        assert!(extent < self.extents());
        if self.fenced {
            return Err(StoreError::Fenced("this life".into()));
        }
        if self.local.get(&extent) != Some(&Local::Dirty) && self.full() {
            return Err(StoreError::Full(self.dirty));
        }
        self.keep_frozen(cache, extent)?;
        cache.punch(extent)?;
        self.set_local(extent, Some(Local::Dirty));
        Ok(())
    }

    /// Cuts everything written since the last cut at this instant
    /// (docs/design.md, The cut): one cut, as many segments as it needs
    /// (at most CUT_SEGMENTS_MAX), the last one marked. It only names
    /// them: each segment is built from the cache by `build_next`, in
    /// order, as there is room to put it, and an extent written again
    /// before then keeps its bytes as of the cut apart. Answers each
    /// segment's number and the extents it names.
    pub fn cut(&mut self) -> Result<Vec<(u64, Vec<u64>)>, StoreError> {
        assert!(self.head_posted, "a life ships only after its head is posted");
        if self.fenced {
            return Err(StoreError::Fenced("this life".into()));
        }
        let dirty: Vec<u64> = self.local.iter().filter(|(_, l)| **l == Local::Dirty).map(|(e, _)| *e).collect();
        assert!(dirty.len() <= DIRTY_EXTENTS_MAX as usize && dirty.len() == self.dirty as usize);
        if dirty.is_empty() {
            return Ok(Vec::new());
        }
        let cut = self.next_cut;
        self.next_cut += 1;
        let groups: Vec<&[u64]> = dirty.chunks(SEGMENT_EXTENTS_TARGET as usize).collect();
        assert!(groups.len() <= CUT_SEGMENTS_MAX as usize);
        let mut out = Vec::with_capacity(groups.len());
        for (gi, g) in groups.iter().enumerate() {
            let seq = self.next_seq;
            self.next_seq += 1;
            for e in g.iter() {
                self.set_local(*e, Some(Local::Frozen(seq)));
            }
            self.planned.insert(seq, Planned { cut, last_of_cut: gi + 1 == groups.len(), extents: g.to_vec() });
            self.planned_extents += g.len() as u64;
            out.push((seq, g.to_vec()));
        }
        Ok(out)
    }

    /// The next planned segment, built here from the cache: `build_take`,
    /// the reads, `build_resolve`, `Building::encode` and `build_done` in
    /// one go. A disk does the reads and the encoding outside its lock
    /// instead, so writes are not held behind a segment's 8 MiB.
    pub fn build_next(&mut self, cache: &mut dyn CacheGate) -> Result<Option<Outgoing>, StoreError> {
        let Some(b) = self.build_take()? else { return Ok(None) };
        let mut bytes = vec![vec![0u8; EXTENT_BYTES as usize]; b.extents.len()];
        for (&e, buf) in b.extents.iter().zip(bytes.iter_mut()) {
            cache.read(e, buf)?;
        }
        self.build_resolve(cache, &b, &mut bytes)?;
        let body = b.encode(&bytes);
        Ok(Some(self.build_done(b, body)))
    }

    /// Takes the next planned segment to build, in order. Its extents stay
    /// frozen: a write to one meanwhile keeps the cut's bytes apart first,
    /// so the cache's bytes may be read without the lock and be torn.
    pub fn build_take(&mut self) -> Result<Option<Building>, StoreError> {
        if self.fenced {
            return Err(StoreError::Fenced("this life".into()));
        }
        assert!(self.building.is_none(), "one segment is built at a time");
        let Some((&seq, _)) = self.planned.first_key_value() else { return Ok(None) };
        let p = self.planned.remove(&seq).expect("present");
        self.building = Some(seq);
        Ok(Some(Building { id: SegId { epoch: self.epoch, seq }, base: self.chain_end, cut: p.cut, last_of_cut: p.last_of_cut, extents: p.extents, seal: self.seal.clone() }))
    }

    /// The segment's bytes as of its cut, from what was read of the cache:
    /// an extent written since then had its bytes kept apart before that
    /// write, and that copy replaces whatever the read saw; any other was
    /// not written, so the read saw its bytes as of the cut. From here on
    /// they ship. Answers, for each, whether its kept copy replaced it.
    pub fn build_resolve(&mut self, cache: &mut dyn CacheGate, b: &Building, bytes: &mut [Vec<u8>]) -> Result<Vec<bool>, StoreError> {
        if self.fenced {
            return Err(StoreError::Fenced("this life".into()));
        }
        let seq = b.id.seq;
        assert_eq!(self.building, Some(seq), "the segment taken");
        assert_eq!(bytes.len(), b.extents.len());
        let mut replaced = Vec::with_capacity(bytes.len());
        for (&e, buf) in b.extents.iter().zip(bytes.iter_mut()) {
            if self.shadows.remove(&(e, seq)) {
                cache.shadow_take(e, seq, buf)?;
                replaced.push(true);
            } else {
                assert_eq!(self.local.get(&e), Some(&Local::Frozen(seq)), "an extent not written since its cut is still frozen");
                self.set_local(e, Some(Local::Shipping(seq)));
                replaced.push(false);
            }
        }
        Ok(replaced)
    }

    /// The built segment joins those in flight.
    pub fn build_done(&mut self, b: Building, body: Vec<u8>) -> Outgoing {
        assert_eq!(self.building.take(), Some(b.id.seq), "the segment taken");
        assert_eq!(b.base, self.chain_end, "segments are built in order, one at a time");
        let id = b.id;
        let h = match &self.seal {
            Some(s) => format::decode_segment_header_sealed(s, id, &body),
            None => format::decode_segment_header(&body),
        }
        .expect("a segment we made decodes");
        let link = h.link();
        let entries: Vec<(u64, Loc)> = h.entries.iter().map(|en| (en.extent, Loc { seg: id, offset: en.offset, len: en.len, sum: en.sum })).collect();
        self.planned_extents -= b.extents.len() as u64;
        self.shipping_extents += entries.len() as u64;
        self.shipping.insert(id.seq, Shipment { base: self.chain_end, link, last_of_cut: b.last_of_cut, entries, landed: false });
        self.chain_end = link;
        Outgoing { name: format::segment_name(id), body, kind: Kind::Segment(id.seq) }
    }

    /// Segment `seq` of this life landed. The map moves a whole cut at a
    /// time, in segment order: a cut counts only once every segment of it
    /// has landed, so the map (and any checkpoint of it) is always a real
    /// moment of the disk; a segment that landed early waits. Answers the
    /// extents that became clean (the cache's policy may drop them now).
    pub fn landed(&mut self, seq: u64) -> Vec<u64> {
        let s = self.shipping.get_mut(&seq).expect("a segment this life shipped");
        s.landed = true;
        // the extents that became clean, for the cache's policy
        let mut cleaned = Vec::new();
        // Bounded by the segments in flight.
        loop {
            // the first cut in flight: its segments up to its last
            let mut cut: Vec<u64> = Vec::new();
            let mut complete = false;
            for (&q, s) in &self.shipping {
                if !s.landed {
                    break;
                }
                cut.push(q);
                if s.last_of_cut {
                    complete = true;
                    break;
                }
            }
            if !complete {
                return cleaned;
            }
            for q in cut {
                let s = self.shipping.remove(&q).expect("present");
                assert_eq!(s.base, self.applied_end, "segments are applied in chain order");
                self.shipping_extents -= s.entries.len() as u64;
                for (e, loc) in &s.entries {
                    self.map.insert(*e, *loc);
                    self.pages_dirty.insert(page_of(*e));
                    if self.local.get(e) == Some(&Local::Shipping(q)) {
                        self.set_local(*e, Some(Local::Clean));
                        cleaned.push(*e);
                    }
                    if !loc.is_zero() {
                        // the most recent PACK_EXTENTS_MAX, each once,
                        // the newest last (bounded: one removal in a
                        // list of at most PACK_EXTENTS_MAX)
                        if let Some(i) = self.written.iter().position(|x| x == e) {
                            self.written.remove(i);
                            // carried again: the crash pack's (the same
                            // bound and order)
                            if let Some(j) = self.rewritten.iter().position(|x| x == e) {
                                self.rewritten.remove(j);
                            }
                            if self.rewritten.len() == PACK_EXTENTS_MAX as usize {
                                self.rewritten.pop_front();
                            }
                            self.rewritten.push_back(*e);
                        }
                        if self.written.len() == PACK_EXTENTS_MAX as usize {
                            self.written.pop_front();
                        }
                        self.written.push_back(*e);
                    }
                }
                self.segments.insert(SegId { epoch: self.epoch, seq: q }, s.link.sum);
                self.segments_landed_since += 1;
                self.applied_end = s.link;
            }
        }
    }

    /// Another writer holds one of this life's names: nothing more ships.
    pub fn fence(&mut self) {
        self.fenced = true;
    }

    /// Whether a checkpoint would write anything.
    pub fn checkpoint_due(&self) -> bool {
        let last = self.checkpoint.as_ref();
        !self.pages_dirty.is_empty() || self.pack.is_some() && last.and_then(|c| c.pack) != self.pack || last.and_then(|c| c.crash_pack) != self.crash_pack
    }

    /// The objects of a checkpoint of what the map holds now (as of the
    /// last segment applied): the pages that changed, then the checkpoint
    /// (last). Put the pages, and only once all have landed the
    /// checkpoint; then call `checkpoint_landed`, and post the head.
    pub fn checkpoint(&mut self) -> Vec<Outgoing> {
        assert!(self.head_posted, "a life's own checkpoints follow its head");
        self.checkpoint_objects()
    }

    fn checkpoint_objects(&mut self) -> Vec<Outgoing> {
        let seq = self.next_checkpoint;
        self.next_checkpoint += 1;
        let mut pages: BTreeMap<u32, PageRef> = self.checkpoint.as_ref().map(|c| c.pages.iter().map(|p| (p.page, *p)).collect()).unwrap_or_default();
        let mut out = Vec::new();
        assert!(self.pages_dirty.iter().all(|p| !self.unloaded.contains_key(p)), "a page is loaded before a checkpoint rewrites it");
        for &p in &self.pages_dirty {
            let lo = u64::from(p) * PAGE_EXTENTS as u64;
            let entries: Vec<(u32, Loc)> = self.map.range(lo..lo + PAGE_EXTENTS as u64).map(|(e, l)| (u32::try_from(e - lo).expect("in the page"), *l)).collect();
            let name = format::page_name(self.epoch, seq, p);
            let body = self.sealed_object(&name, format::encode_page(p, &entries));
            pages.insert(p, PageRef { page: p, epoch: self.epoch, seq, sum: format::sha256(&body) });
            out.push(Outgoing { name, body, kind: Kind::Page });
        }
        let mut live: BTreeSet<SegId> = self.map.values().filter(|l| !l.is_zero()).map(|l| l.seg).collect();
        // pages not loaded may name any segment the last checkpoint listed:
        // the list is a superset until every page is loaded (the cleaner,
        // which reads every page, keeps it exact)
        if !self.unloaded.is_empty() {
            if let Some(c) = &self.checkpoint {
                live.extend(c.segments.iter().map(|(id, _)| *id));
            }
        }
        let segments: Vec<(SegId, Sum)> = live.iter().map(|id| (*id, *self.segments.get(id).expect("every mapped segment's seal is known"))).collect();
        assert!(segments.len() <= LIVE_SEGMENTS_MAX as usize, "live segments past their limit: the cleaner's compaction is P5's");
        let boot_pages: Vec<u32> = self.boot_pages.iter().copied().filter(|p| pages.contains_key(p)).collect();
        let c = Checkpoint { epoch: self.epoch, seq, end: self.applied_end, volume_bytes: self.volume_bytes, pack: self.pack, crash_pack: self.crash_pack, pages: pages.into_values().collect(), boot_pages, segments };
        // a sealed disk's checkpoint is readable and authenticated, not
        // sealed: what it names is the platform's cleaner's to read
        let name = format::checkpoint_name(self.epoch, seq);
        let plain = format::encode_checkpoint(&c);
        let body = match &self.seal {
            Some(s) => s.authenticate_object(&name, &plain),
            None => plain,
        };
        let cr = CheckpointRef { epoch: self.epoch, seq, sum: format::sha256(&body) };
        out.push(Outgoing { name: format::checkpoint_name(self.epoch, seq), body, kind: Kind::Checkpoint(cr) });
        out
    }

    /// A checkpoint landed (its pages before it): it is the one later
    /// checkpoints build on. The pages it wrote are clean.
    pub fn checkpoint_landed(&mut self, objects: &[Outgoing]) {
        let Some(Outgoing { name, body, kind: Kind::Checkpoint(cr) }) = objects.last() else { panic!("a checkpoint's objects end with it") };
        let c = format::decode_checkpoint(&self.verified_checkpoint(name, body).expect("a checkpoint we authenticated verifies")).expect("a checkpoint we made decodes");
        assert_eq!(c.end, self.applied_end, "a checkpoint lands before anything else is applied");
        self.pages_dirty.clear();
        self.segments_landed_since = 0;
        self.checkpoint = Some(c);
        self.checkpoint_ref = Some(*cr);
    }

    /// A pack for the next wake: the extents this boot read, in the order
    /// it read them, then those this life wrote, newest first (the next
    /// boot reads what this life left: its databases, its logs), each once,
    /// each clean in
    /// the cache (its bytes the map's), up to PACK_EXTENTS_MAX. One object
    /// for the next wake to fetch.
    pub fn pack(&mut self, cache: &mut dyn CacheGate) -> Result<Option<Outgoing>, StoreError> {
        let order: Vec<u64> = self.boot.iter().chain(self.written.iter().rev()).copied().collect();
        Ok(self.build_pack(cache, order, PACK_EXTENTS_MAX as usize)?.map(|(name, body, pr)| Outgoing { name, body, kind: Kind::Pack(pr) }))
    }

    /// A periodic checkpoint's crash pack: what this life rewrote, newest
    /// first, up to CRASH_PACK_EXTENTS; None when it rewrote nothing.
    pub fn crash_pack(&mut self, cache: &mut dyn CacheGate) -> Result<Option<Outgoing>, StoreError> {
        let order: Vec<u64> = self.rewritten.iter().rev().copied().collect();
        Ok(self.build_pack(cache, order, CRASH_PACK_EXTENTS)?.map(|(name, body, pr)| Outgoing { name, body, kind: Kind::CrashPack(pr) }))
    }

    /// The crash pack landed: the next checkpoint names it.
    pub fn crash_pack_landed(&mut self, pr: PackRef) {
        self.crash_pack = Some(pr);
    }

    /// A pack of `order`'s extents, each once, each clean in the cache
    /// (its bytes the map's), up to `max`.
    fn build_pack(&mut self, cache: &mut dyn CacheGate, order: Vec<u64>, max: usize) -> Result<Option<(String, Vec<u8>, PackRef)>, StoreError> {
        // a sealed extent carries its tag: one fewer fits the object
        let max = if self.seal.is_some() { max.min(format::seal::PACK_EXTENTS_SEALED_MAX as usize) } else { max };
        let seq = self.next_pack;
        let mut images: Vec<(u64, Loc, Vec<u8>)> = Vec::new();
        let mut taken = BTreeSet::new();
        for e in order {
            if images.len() == max || !taken.insert(e) {
                continue;
            }
            if self.local.get(&e) != Some(&Local::Clean) {
                continue;
            }
            let Some(l) = self.map.get(&e).copied() else { continue };
            if l.is_zero() {
                continue;
            }
            let mut b = vec![0u8; EXTENT_BYTES as usize];
            cache.read(e, &mut b)?;
            // clean is the map's bytes; a sealed cache checked them as read
            if self.seal.is_some() || format::sum64(&b) == l.sum {
                images.push((e, l, b));
            }
        }
        if images.is_empty() {
            return Ok(None);
        }
        let refs: Vec<(u64, Loc, &[u8])> = images.iter().map(|(e, l, b)| (*e, *l, b.as_slice())).collect();
        let name = format::pack_name(self.epoch, seq);
        let (body, header_sum) = match &self.seal {
            Some(s) => format::encode_pack_sealed(s, &name, &refs),
            None => {
                let body = format::encode_pack(&refs);
                let h = format::decode_pack_header(&body).expect("a pack we made decodes");
                let sum = format::sha256(&body[..h.len()]);
                (body, sum)
            }
        };
        let pr = PackRef { epoch: self.epoch, seq, header_sum };
        self.next_pack += 1;
        Ok(Some((name, body, pr)))
    }

    /// The pack landed: the next checkpoint names it.
    pub fn pack_landed(&mut self, pr: PackRef) {
        self.pack = Some(pr);
        // a sleep's pack holds what it wrote: no crash pack after it
        self.crash_pack = None;
    }

    /// The pack this life opened with, if any: fetch its header
    /// (`PACK_HEADER_FETCH_BYTES`), then `pack_ranges` in parallel, and
    /// install each with `pack_install`.
    pub fn pack_to_fetch(&self) -> Option<(String, PackRef)> {
        self.opened.pack.map(|p| (format::pack_name(p.epoch, p.seq), p))
    }

    /// The crash pack this life opened with, if any (a wake after a crash):
    /// fetched as the pack is.
    pub fn crash_pack_to_fetch(&self) -> Option<(String, PackRef)> {
        self.opened.crash_pack.map(|p| (format::pack_name(p.epoch, p.seq), p))
    }

    /// The pack's header, checked against the checkpoint's record of it:
    /// where its extents are in the object, and how far apart.
    pub fn pack_header(&self, name: &str, pr: &PackRef, prefix: &[u8]) -> Result<PackView, StoreError> {
        let bad = |e: format::FormatError| StoreError::Corrupt(format!("{name}: {e}"));
        let (header, sum, images_at, stride) = match &self.seal {
            Some(s) => {
                let (h, sum) = format::decode_pack_header_sealed(s, name, prefix).map_err(bad)?;
                let at = format::sealed_pack_header_len(h.entries.len());
                (h, sum, at, SEALED_EXTENT_BYTES)
            }
            None => {
                let h = format::decode_pack_header(prefix).map_err(bad)?;
                let sum = format::sha256(&prefix[..h.len()]);
                let at = h.len();
                (h, sum, at, EXTENT_BYTES)
            }
        };
        if sum != pr.header_sum {
            return Err(StoreError::Corrupt(format!("{name} is not the pack its checkpoint names")));
        }
        Ok(PackView { header, name: name.to_string(), images_at: images_at as u64, stride: u64::from(stride) })
    }

    /// The pack's data from its extent `from` on, split into ranges of
    /// about `bytes` each (whole extents), to fetch in parallel: (offset
    /// in the object, length).
    pub fn pack_ranges(v: &PackView, from: u64, bytes: u64) -> Vec<(u64, u64)> {
        let per = (bytes / v.stride).max(1);
        let n = (v.header.entries.len() as u64).saturating_sub(from);
        let base = v.images_at + from * v.stride;
        (0..n.div_ceil(per)).map(|i| (base + i * per * v.stride, (per.min(n - i * per)) * v.stride)).collect()
    }

    /// Installs a fetched range of the pack: each extent whose sum the map
    /// still names, and whose bytes check out, goes to the cache as clean.
    /// Answers how many were installed.
    pub fn pack_install(&mut self, cache: &mut dyn CacheGate, v: &PackView, offset: u64, bytes: &[u8]) -> Result<u32, StoreError> {
        assert!(offset >= v.images_at && (offset - v.images_at).is_multiple_of(v.stride));
        let first = ((offset - v.images_at) / v.stride) as usize;
        let mut installed = 0;
        let mut plain = vec![0u8; EXTENT_BYTES as usize];
        for (i, got) in bytes.chunks_exact(v.stride as usize).enumerate() {
            let Some(&(e, loc)) = v.header.entries.get(first + i) else { break };
            // the map's placement if it has one (a replay is newer than the
            // pack); else, where its page is not loaded, the pack's (it is
            // the checkpoint's, sealed by the header's sum it names)
            let wanted = !self.local.contains_key(&e)
                && match self.map.get(&e) {
                    Some(l) => !l.is_zero() && l.sum == loc.sum,
                    None => self.unloaded.contains_key(&page_of(e)) && self.segments.contains_key(&loc.seg) && e < self.extents(),
                };
            if !wanted {
                continue;
            }
            let image: &[u8] = match &self.seal {
                Some(s) => {
                    let tag: [u8; 16] = got[EXTENT_BYTES as usize..].try_into().expect("16");
                    plain.copy_from_slice(&got[..EXTENT_BYTES as usize]);
                    let at = Place::Packed { pack: &v.name, index: u32::try_from(first + i).expect("bounded"), extent: e };
                    if s.open_detached(at, &mut plain, &tag).is_err() {
                        continue;
                    }
                    &plain
                }
                None if format::sum64(got) == loc.sum => got,
                None => continue,
            };
            self.map.entry(e).or_insert(loc);
            cache.write(e, image)?;
            self.set_local(e, Some(Local::Clean));
            installed += 1;
        }
        Ok(installed)
    }
}

/// A segment's header, fetched again when it fails its checksum, as an
/// extent is; None when the segment is absent.
fn header(remote: &mut dyn RemoteGate, seal: Option<&Sealer>, id: SegId, buf: &mut [u8]) -> Result<Option<format::SegmentHeader>, StoreError> {
    let name = &format::segment_name(id);
    let mut last = String::new();
    for _ in 0..FETCH_TRIES_MAX {
        match remote.get_range(name, 0, buf) {
            Ok(None) => return Ok(None),
            Ok(Some(n)) => match seal.map_or_else(|| format::decode_segment_header(&buf[..n]), |s| format::decode_segment_header_sealed(s, id, &buf[..n])) {
                Ok(h) => return Ok(Some(h)),
                Err(e) => last = e.to_string(),
            },
            Err(RemoteError::Unavailable(s)) => last = s,
            Err(e) => return Err(from_remote(e)),
        }
    }
    Err(StoreError::Corrupt(format!("{name}, after {FETCH_TRIES_MAX} tries: {last}")))
}

/// Bytes of a pack's object that hold its header, whatever its count.
pub const PACK_HEADER_FETCH_BYTES: usize = format::limits::PACK_HEADER_BYTES_MAX as usize;

/// The pack's head as fetched in the open's round, plain or sealed: its
/// header and its first PACK_HEAD_EXTENTS extents.
pub fn pack_head_fetch_bytes(sealed: bool) -> usize {
    if sealed {
        format::SEALED_PREFIX_BYTES as usize + PACK_HEADER_FETCH_BYTES + format::TAG_BYTES as usize + PACK_HEAD_EXTENTS as usize * SEALED_EXTENT_BYTES as usize
    } else {
        PACK_HEAD_FETCH_BYTES
    }
}

/// Bytes of a segment's object that hold its header, plain or sealed.
pub fn segment_header_fetch_bytes(sealed: bool) -> usize {
    SEGMENT_HEADER_BYTES_MAX as usize + if sealed { format::SEALED_PREFIX_BYTES as usize + format::TAG_BYTES as usize } else { 0 }
}

/// A pack's header as a wake reads it: where its extents start in the
/// object, and how far apart (64 KiB, and a tag when sealed).
#[derive(Debug, Clone)]
pub struct PackView {
    pub header: format::PackHeader,
    pub name: String,
    pub images_at: u64,
    pub stride: u64,
}
/// The pack's head, fetched in the open's one round: its header and, after
/// it, its first extents, so a mount waits for no other round trip. An
/// ext4 mount of a 20 GB disk read past the first 16 and the first 32 of
/// them on krun (docs/spike.md, P5); 64 is 4 MiB.
pub const PACK_HEAD_FETCH_BYTES: usize = PACK_HEADER_FETCH_BYTES + PACK_HEAD_EXTENTS as usize * EXTENT_BYTES as usize;
pub const PACK_HEAD_EXTENTS: u32 = 64;
/// Segment headers an open asks for with its pages: a replay's first
/// round. A replay past them fetches its headers this many at a time.
pub const OPEN_SEGMENT_HEADERS: u32 = 16;

// Every object the store writes is one the route takes (docs/contract.md,
// Limits): a pack over it was refused 413 on krun before this was asserted.
const _: () = assert!(format::limits::SEGMENT_BYTES_MAX <= fpac_remote::limits::OBJECT_BYTES_MAX);
const _: () = assert!(format::limits::PACK_OBJECT_BYTES_MAX <= fpac_remote::limits::OBJECT_BYTES_MAX);
const _: () = assert!(format::limits::CHECKPOINT_BYTES_MAX <= fpac_remote::limits::OBJECT_BYTES_MAX);
const _: () = assert!(format::limits::PAGE_BYTES_MAX <= fpac_remote::limits::OBJECT_BYTES_MAX);
const _: () = assert!(format::limits::CHECKPOINT_BYTES_MAX + format::seal::SEALED_OBJECT_EXTRA_BYTES <= fpac_remote::limits::OBJECT_BYTES_MAX);
const _: () = assert!(format::limits::PAGE_BYTES_MAX + format::seal::SEALED_OBJECT_EXTRA_BYTES <= fpac_remote::limits::OBJECT_BYTES_MAX);

fn remote_err_on(name: &str) -> impl Fn(RemoteError) -> StoreError + '_ {
    move |e| match from_remote(e) {
        StoreError::Unavailable(s) => StoreError::Unavailable(format!("{name}: {s}")),
        other => other,
    }
}

/// A whole object, at most `max` bytes, checked by the remote against the
/// checksum stored with it.
fn whole(remote: &mut dyn RemoteGate, name: &str, max: u64) -> Result<Vec<u8>, StoreError> {
    let mut body = Vec::new();
    let mut got = Err(StoreError::Unavailable(name.to_string()));
    // A whole get that fails its stored checksum is fetched again, as a
    // wrong extent is.
    for _ in 0..FETCH_TRIES_MAX {
        body.clear();
        got = remote.get(name, &mut |b| body.extend_from_slice(b)).map_err(remote_err_on(name));
        if !matches!(got, Err(StoreError::Corrupt(_)) | Err(StoreError::Unavailable(_))) {
            break;
        }
    }
    match got? {
        Some(n) if n <= max => Ok(body),
        Some(n) => Err(StoreError::Corrupt(format!("{name} is {n} bytes, past its limit"))),
        None => Err(StoreError::Corrupt(format!("{name} is gone, and the head names it"))),
    }
}

/// Puts one object, retrying an unavailable remote `tries` times in all:
/// `Put::Same` is our own earlier put whose answer was lost; a conflict
/// is another writer, and fences this life.
pub fn ship(remote: &mut dyn RemoteGate, o: &Outgoing, tries: u32) -> Result<(), StoreError> {
    assert!(tries >= 1);
    let mut last = String::new();
    for _ in 0..tries {
        match remote.put(&o.name, &o.body) {
            Ok(Put::Created) | Ok(Put::Same) => return Ok(()),
            Ok(Put::Conflict { .. }) => return Err(StoreError::Fenced(o.name.clone())),
            Err(RemoteError::Unavailable(s)) => last = s,
            Err(e) => return Err(from_remote(e)),
        }
    }
    Err(StoreError::Unavailable(format!("{}: {last}", o.name)))
}
