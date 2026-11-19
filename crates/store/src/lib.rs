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

pub mod gates;
pub mod head;

use std::collections::{BTreeMap, BTreeSet};

use fpac_format::limits::*;
use fpac_format::{self as format, Checkpoint, Image, Link, Loc, PackRef, PageRef, SegId, Sum, GENESIS};
use fpac_remote::{Put, RemoteError};

pub use gates::{CacheGate, MemCache, RemoteGate};
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
}

/// An extent's state in the cache file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Local {
    /// Its bytes are the map's (checked against its sum when read).
    Clean,
    /// Written since the last cut: the cache is its truth.
    Dirty,
    /// Cut into this life's segment `seq`, not yet landed; a write makes
    /// it Dirty again.
    Shipping(u64),
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
    pub pack: Option<PackRef>,
}

pub struct Store {
    volume_bytes: u64,
    epoch: u64,
    /// Where each written extent's bytes are, as the store holds them.
    map: BTreeMap<u64, Loc>,
    /// Every segment the map may name, with its header's seal.
    segments: BTreeMap<SegId, Sum>,
    local: BTreeMap<u64, Local>,
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
    pack: Option<PackRef>,
    head_posted: bool,
    fenced: bool,
    boot: Vec<u64>,
    boot_seen: BTreeSet<u64>,
    recording: bool,
    opened: Opened,
}

fn page_of(extent: u64) -> u32 {
    u32::try_from(extent / PAGE_EXTENTS as u64).expect("bounded by the volume")
}

/// How many extents a segment of a cut takes at most: the target size.
const SEGMENT_EXTENTS_TARGET: usize = (SEGMENT_BYTES_TARGET / EXTENT_BYTES as u64) as usize;

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
            map: BTreeMap::new(),
            segments: BTreeMap::new(),
            local: BTreeMap::new(),
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
            pack: None,
            head_posted: false,
            fenced: false,
            boot: Vec::new(),
            boot_seen: BTreeSet::new(),
            recording: false,
            opened: Opened { replayed: 0, extents_mapped: 0, pack: None },
        };
        let Some(head) = info.head else {
            return Ok(s);
        };
        assert!(head.epoch < info.epoch, "a new life is later than the head's");
        let mut first_seq = 1;
        if let Some(cr) = head.checkpoint {
            let c = s.load_checkpoint(remote, cr)?;
            if c.volume_bytes > info.volume_bytes {
                return Err(StoreError::Corrupt(format!("the disk is {} bytes; this life was given {}", c.volume_bytes, info.volume_bytes)));
            }
            // the head life's segments after the chain's end the
            // checkpoint covers (none of them, if it ends in another life)
            if c.end.epoch == head.epoch {
                first_seq = c.end.seq + 1;
            }
            s.chain_end = c.end;
            s.applied_end = c.end;
            s.pack = c.pack;
            s.checkpoint = Some(c);
            s.checkpoint_ref = Some(cr);
        }
        s.replay(remote, head.epoch, first_seq)?;
        s.opened = Opened { replayed: s.opened.replayed, extents_mapped: s.map.len() as u64, pack: s.pack };
        assert_eq!(s.chain_end, s.applied_end, "nothing of this life is shipping yet");
        Ok(s)
    }

    fn load_checkpoint(&mut self, remote: &mut dyn RemoteGate, cr: CheckpointRef) -> Result<Checkpoint, StoreError> {
        let name = format::checkpoint_name(cr.epoch, cr.seq);
        let body = whole(remote, &name, CHECKPOINT_BYTES_MAX)?;
        if format::sha256(&body) != cr.sum {
            return Err(StoreError::Corrupt(format!("{name} is not the checkpoint the head names")));
        }
        let c = format::decode_checkpoint(&body).map_err(|e| StoreError::Corrupt(format!("{name}: {e}")))?;
        if (c.epoch, c.seq) != (cr.epoch, cr.seq) {
            return Err(StoreError::Corrupt(format!("{name} names itself otherwise")));
        }
        let live: BTreeMap<SegId, Sum> = c.segments.iter().copied().collect();
        // Bounded: at most PAGES_MAX pages.
        for p in &c.pages {
            let pname = format::page_name(p.epoch, p.seq, p.page);
            let body = whole(remote, &pname, PAGE_BYTES_MAX)?;
            if format::sha256(&body) != p.sum {
                return Err(StoreError::Corrupt(format!("{pname} is not the page {name} names")));
            }
            let (page, entries) = format::decode_page(&body).map_err(|e| StoreError::Corrupt(format!("{pname}: {e}")))?;
            if page != p.page {
                return Err(StoreError::Corrupt(format!("{pname} is page {page}")));
            }
            for (i, loc) in entries {
                if !loc.is_zero() && !live.contains_key(&loc.seg) {
                    return Err(StoreError::Corrupt(format!("{pname} names a segment {name} does not list")));
                }
                let extent = u64::from(page) * PAGE_EXTENTS as u64 + u64::from(i);
                if extent >= c.volume_bytes / EXTENT_BYTES as u64 {
                    return Err(StoreError::Corrupt(format!("{pname} maps an extent past the volume")));
                }
                self.map.insert(extent, loc);
            }
        }
        self.segments = live;
        Ok(c)
    }

    /// Applies life `epoch`'s segments from `first`, each following the
    /// chain's end, until one is absent. Bounded by REPLAY_SEGMENTS_MAX:
    /// a checkpoint is written every CHECKPOINT_SEGMENTS, so one more is
    /// corruption.
    fn replay(&mut self, remote: &mut dyn RemoteGate, epoch: u64, first: u64) -> Result<(), StoreError> {
        let mut group: Vec<format::SegmentHeader> = Vec::new();
        let mut seq = first;
        let mut buf = vec![0u8; SEGMENT_HEADER_BYTES_MAX as usize];
        loop {
            if seq - first >= u64::from(REPLAY_SEGMENTS_MAX) + u64::from(CUT_SEGMENTS_MAX) {
                return Err(StoreError::Corrupt(format!("more than {REPLAY_SEGMENTS_MAX} segments after a checkpoint")));
            }
            let name = format::segment_name(SegId { epoch, seq });
            let Some(h) = header(remote, &name, &mut buf)? else { break };
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
        self.local.values().filter(|l| **l == Local::Dirty).count()
    }

    pub fn shipping(&self) -> usize {
        self.shipping.len()
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
            return (out, Head { epoch: self.epoch, checkpoint: Some(cr) });
        }
        (Vec::new(), Head { epoch: self.epoch, checkpoint: self.checkpoint_ref })
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
    }

    /// Reads extent `extent` into `buf` (one extent long). From the cache
    /// when it holds it, else from the store, with read-ahead.
    pub fn read(&mut self, remote: &mut dyn RemoteGate, cache: &mut dyn CacheGate, extent: u64, buf: &mut [u8]) -> Result<(), StoreError> {
        assert_eq!(buf.len(), EXTENT_BYTES as usize);
        assert!(extent < self.extents());
        self.note(extent);
        match self.local.get(&extent).copied() {
            Some(Local::Dirty) | Some(Local::Shipping(_)) => {
                cache.read(extent, buf)?;
                return Ok(());
            }
            Some(Local::Clean) => {
                cache.read(extent, buf)?;
                let loc = self.map.get(&extent).copied();
                let right = match loc {
                    Some(l) if !l.is_zero() => format::sum64(buf) == l.sum,
                    _ => buf.iter().all(|&b| b == 0),
                };
                if right {
                    return Ok(());
                }
                // A cache that lies: dropped, and fetched again.
                self.local.remove(&extent);
            }
            None => {}
        }
        match self.map.get(&extent).copied() {
            None => {
                buf.fill(0);
                Ok(())
            }
            Some(l) if l.is_zero() => {
                buf.fill(0);
                Ok(())
            }
            Some(l) => self.fetch(remote, cache, extent, l, buf),
        }
    }

    /// Fetches `extent` and the extents after it in the same segment, up
    /// to READ_AHEAD_EXTENTS, that the cache does not hold: one ranged get.
    /// Each image is checked against its sum64; the one asked for is
    /// fetched again on a mismatch, FETCH_TRIES_MAX times in all.
    fn fetch(&mut self, remote: &mut dyn RemoteGate, cache: &mut dyn CacheGate, extent: u64, loc: Loc, buf: &mut [u8]) -> Result<(), StoreError> {
        let mut run = vec![(extent, loc)];
        // Bounded: READ_AHEAD_EXTENTS.
        while run.len() < READ_AHEAD_EXTENTS as usize {
            let (pe, pl) = *run.last().expect("non-empty");
            let next = pe + 1;
            if next >= self.extents() || self.local.contains_key(&next) {
                break;
            }
            match self.map.get(&next) {
                Some(l) if !l.is_zero() && l.seg == pl.seg && l.offset == pl.offset + EXTENT_BYTES => run.push((next, *l)),
                _ => break,
            }
        }
        let name = format::segment_name(loc.seg);
        let ext = EXTENT_BYTES as usize;
        let mut window = vec![0u8; run.len() * ext];
        let mut last_error = String::new();
        for _try in 0..FETCH_TRIES_MAX {
            let got = match remote.get_range(&name, u64::from(loc.offset), &mut window) {
                Ok(Some(n)) => n,
                Ok(None) => return Err(StoreError::Corrupt(format!("{name} is gone, and the map names it"))),
                Err(RemoteError::Unavailable(s)) => {
                    last_error = s;
                    continue;
                }
                Err(e) => return Err(from_remote(e)),
            };
            let whole_extents = got / ext;
            let first = &window[..ext.min(got)];
            if whole_extents >= 1 && format::sum64(first) == loc.sum {
                buf.copy_from_slice(first);
                cache.write(extent, first)?;
                self.local.insert(extent, Local::Clean);
                // the read-ahead's extents that check out; any other is
                // simply not installed, and is a miss of its own later
                for (i, (e, l)) in run.iter().enumerate().skip(1).take(whole_extents.saturating_sub(1)) {
                    let image = &window[i * ext..(i + 1) * ext];
                    if format::sum64(image) == l.sum {
                        cache.write(*e, image)?;
                        self.local.insert(*e, Local::Clean);
                    }
                }
                return Ok(());
            }
            last_error = format!("{name} at {}: {got} bytes, the extent's checksum does not match", loc.offset);
            // fetch only the extent asked for on the next try
            run.truncate(1);
            window.truncate(ext);
        }
        Err(StoreError::Corrupt(format!("extent {extent}, after {FETCH_TRIES_MAX} tries: {last_error}")))
    }

    /// Writes `data` at `offset` within extent `extent`. A partial write
    /// to an extent the cache does not hold reads it first.
    pub fn write(&mut self, remote: &mut dyn RemoteGate, cache: &mut dyn CacheGate, extent: u64, offset: u32, data: &[u8]) -> Result<(), StoreError> {
        assert!(extent < self.extents());
        assert!(offset as usize + data.len() <= EXTENT_BYTES as usize && !data.is_empty());
        if self.fenced {
            return Err(StoreError::Fenced("this life".into()));
        }
        let whole_extent = offset == 0 && data.len() == EXTENT_BYTES as usize;
        if whole_extent {
            cache.write(extent, data)?;
        } else {
            let mut buf = vec![0u8; EXTENT_BYTES as usize];
            self.read(remote, cache, extent, &mut buf)?;
            buf[offset as usize..offset as usize + data.len()].copy_from_slice(data);
            cache.write(extent, &buf)?;
        }
        self.local.insert(extent, Local::Dirty);
        Ok(())
    }

    /// The extent reads as zeros from now on (a discard).
    pub fn punch(&mut self, cache: &mut dyn CacheGate, extent: u64) -> Result<(), StoreError> {
        assert!(extent < self.extents());
        if self.fenced {
            return Err(StoreError::Fenced("this life".into()));
        }
        cache.punch(extent)?;
        self.local.insert(extent, Local::Dirty);
        Ok(())
    }

    /// Cuts what changed into segments at this instant (docs/design.md,
    /// The cut): one cut, as many segments as it needs (at most
    /// CUT_SEGMENTS_MAX; the rest waits for the next cut), the last one
    /// marked. Answers them in order, for the caller to put, several at
    /// once.
    pub fn cut(&mut self, cache: &mut dyn CacheGate) -> Result<Vec<Outgoing>, StoreError> {
        assert!(self.head_posted, "a life ships only after its head is posted");
        if self.fenced {
            return Err(StoreError::Fenced("this life".into()));
        }
        let dirty: Vec<u64> = self.local.iter().filter(|(_, l)| **l == Local::Dirty).map(|(e, _)| *e).take(SEGMENT_EXTENTS_TARGET * CUT_SEGMENTS_MAX as usize).collect();
        if dirty.is_empty() {
            return Ok(Vec::new());
        }
        let cut = self.next_cut;
        self.next_cut += 1;
        let groups: Vec<&[u64]> = dirty.chunks(SEGMENT_EXTENTS_TARGET).collect();
        let mut out = Vec::with_capacity(groups.len());
        let mut bytes: Vec<Vec<u8>> = Vec::with_capacity(SEGMENT_EXTENTS_TARGET);
        for (gi, g) in groups.iter().enumerate() {
            bytes.clear();
            for e in g.iter() {
                let mut b = vec![0u8; EXTENT_BYTES as usize];
                cache.read(*e, &mut b)?;
                bytes.push(b);
            }
            // an extent of zeros ships as a zeroed entry, with no image
            let images: Vec<Image<'_>> = g.iter().zip(&bytes).map(|(e, b)| Image { extent: *e, bytes: if b.iter().all(|&x| x == 0) { None } else { Some(b.as_slice()) } }).collect();
            let id = SegId { epoch: self.epoch, seq: self.next_seq };
            let body = format::encode_segment(id, self.chain_end, cut, gi + 1 == groups.len(), &images);
            let h = format::decode_segment_header(&body).expect("a segment we made decodes");
            let link = h.link();
            let entries = h.entries.iter().map(|en| (en.extent, Loc { seg: id, offset: en.offset, len: en.len, sum: en.sum })).collect();
            for e in g.iter() {
                self.local.insert(*e, Local::Shipping(id.seq));
            }
            self.shipping.insert(id.seq, Shipment { base: self.chain_end, link, last_of_cut: gi + 1 == groups.len(), entries, landed: false });
            self.chain_end = link;
            self.next_seq += 1;
            out.push(Outgoing { name: format::segment_name(id), body, kind: Kind::Segment(id.seq) });
        }
        Ok(out)
    }

    /// Segment `seq` of this life landed. The map moves a whole cut at a
    /// time, in segment order: a cut counts only once every segment of it
    /// has landed, so the map (and any checkpoint of it) is always a real
    /// moment of the disk; a segment that landed early waits.
    pub fn landed(&mut self, seq: u64) {
        let s = self.shipping.get_mut(&seq).expect("a segment this life shipped");
        s.landed = true;
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
                return;
            }
            for q in cut {
                let s = self.shipping.remove(&q).expect("present");
                assert_eq!(s.base, self.applied_end, "segments are applied in chain order");
                for (e, loc) in &s.entries {
                    self.map.insert(*e, *loc);
                    self.pages_dirty.insert(page_of(*e));
                    if self.local.get(e) == Some(&Local::Shipping(q)) {
                        self.local.insert(*e, Local::Clean);
                    }
                }
                self.segments.insert(SegId { epoch: self.epoch, seq: q }, s.link.sum);
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
        !self.pages_dirty.is_empty() || self.pack.is_some() && self.checkpoint.as_ref().and_then(|c| c.pack) != self.pack
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
        for &p in &self.pages_dirty {
            let lo = u64::from(p) * PAGE_EXTENTS as u64;
            let entries: Vec<(u32, Loc)> = self.map.range(lo..lo + PAGE_EXTENTS as u64).map(|(e, l)| (u32::try_from(e - lo).expect("in the page"), *l)).collect();
            let body = format::encode_page(p, &entries);
            pages.insert(p, PageRef { page: p, epoch: self.epoch, seq, sum: format::sha256(&body) });
            out.push(Outgoing { name: format::page_name(self.epoch, seq, p), body, kind: Kind::Page });
        }
        let live: BTreeSet<SegId> = self.map.values().filter(|l| !l.is_zero()).map(|l| l.seg).collect();
        let segments: Vec<(SegId, Sum)> = live.iter().map(|id| (*id, *self.segments.get(id).expect("every mapped segment's seal is known"))).collect();
        assert!(segments.len() <= LIVE_SEGMENTS_MAX as usize, "live segments past their limit: the cleaner's compaction is P5's");
        let c = Checkpoint { epoch: self.epoch, seq, end: self.applied_end, volume_bytes: self.volume_bytes, pack: self.pack, pages: pages.into_values().collect(), segments };
        let body = format::encode_checkpoint(&c);
        let cr = CheckpointRef { epoch: self.epoch, seq, sum: format::sha256(&body) };
        out.push(Outgoing { name: format::checkpoint_name(self.epoch, seq), body, kind: Kind::Checkpoint(cr) });
        out
    }

    /// A checkpoint landed (its pages before it): it is the one later
    /// checkpoints build on. The pages it wrote are clean.
    pub fn checkpoint_landed(&mut self, objects: &[Outgoing]) {
        let Some(Outgoing { body, kind: Kind::Checkpoint(cr), .. }) = objects.last() else { panic!("a checkpoint's objects end with it") };
        let c = format::decode_checkpoint(body).expect("a checkpoint we made decodes");
        assert_eq!(c.end, self.applied_end, "a checkpoint lands before anything else is applied");
        self.pages_dirty.clear();
        self.checkpoint = Some(c);
        self.checkpoint_ref = Some(*cr);
    }

    /// A pack of the extents the boot read, in order, that are clean in
    /// the cache (their bytes the map's): one object for the next wake to
    /// fetch. Its name follows the last segment applied.
    pub fn pack(&mut self, cache: &mut dyn CacheGate) -> Result<Option<Outgoing>, StoreError> {
        let seq = self.next_pack;
        let mut images: Vec<(u64, Vec<u8>)> = Vec::new();
        for &e in &self.boot {
            if self.local.get(&e) != Some(&Local::Clean) {
                continue;
            }
            let Some(l) = self.map.get(&e) else { continue };
            if l.is_zero() {
                continue;
            }
            let mut b = vec![0u8; EXTENT_BYTES as usize];
            cache.read(e, &mut b)?;
            if format::sum64(&b) == l.sum {
                images.push((e, b));
            }
        }
        if images.is_empty() {
            return Ok(None);
        }
        let refs: Vec<(u64, &[u8])> = images.iter().map(|(e, b)| (*e, b.as_slice())).collect();
        let body = format::encode_pack(&refs);
        let h = format::decode_pack_header(&body).expect("a pack we made decodes");
        let header_sum = format::sha256(&body[..h.len()]);
        let pr = PackRef { epoch: self.epoch, seq, header_sum };
        self.next_pack += 1;
        Ok(Some(Outgoing { name: format::pack_name(self.epoch, seq), body, kind: Kind::Pack(pr) }))
    }

    /// The pack landed: the next checkpoint names it.
    pub fn pack_landed(&mut self, pr: PackRef) {
        self.pack = Some(pr);
    }

    /// The pack this life opened with, if any: fetch its header
    /// (`PACK_HEADER_FETCH_BYTES`), then `pack_ranges` in parallel, and
    /// install each with `pack_install`.
    pub fn pack_to_fetch(&self) -> Option<(String, PackRef)> {
        self.opened.pack.map(|p| (format::pack_name(p.epoch, p.seq), p))
    }

    /// The pack's header, checked against the checkpoint's record of it.
    pub fn pack_header(&self, pr: &PackRef, prefix: &[u8]) -> Result<format::PackHeader, StoreError> {
        let h = format::decode_pack_header(prefix).map_err(|e| StoreError::Corrupt(format!("a pack: {e}")))?;
        if format::sha256(&prefix[..h.len()]) == pr.header_sum {
            Ok(h)
        } else {
            Err(StoreError::Corrupt("a pack that is not the one its checkpoint names".into()))
        }
    }

    /// The pack's data split into ranges of about `bytes` each (whole
    /// extents), to fetch in parallel: (offset in the object, length).
    pub fn pack_ranges(h: &format::PackHeader, bytes: u64) -> Vec<(u64, u64)> {
        let per = (bytes / EXTENT_BYTES as u64).max(1);
        let n = h.entries.len() as u64;
        (0..n.div_ceil(per)).map(|i| (h.len() as u64 + i * per * EXTENT_BYTES as u64, (per.min(n - i * per)) * EXTENT_BYTES as u64)).collect()
    }

    /// Installs a fetched range of the pack: each extent whose sum the map
    /// still names, and whose bytes check out, goes to the cache as clean.
    /// Answers how many were installed.
    pub fn pack_install(&mut self, cache: &mut dyn CacheGate, h: &format::PackHeader, offset: u64, bytes: &[u8]) -> Result<u32, StoreError> {
        let ext = EXTENT_BYTES as u64;
        assert!(offset >= h.len() as u64 && (offset - h.len() as u64).is_multiple_of(ext));
        let first = ((offset - h.len() as u64) / ext) as usize;
        let mut installed = 0;
        for (i, image) in bytes.chunks_exact(ext as usize).enumerate() {
            let Some(&(e, sum)) = h.entries.get(first + i) else { break };
            let wanted = self.map.get(&e).is_some_and(|l| !l.is_zero() && l.sum == sum) && !self.local.contains_key(&e);
            if wanted && format::sum64(image) == sum {
                cache.write(e, image)?;
                self.local.insert(e, Local::Clean);
                installed += 1;
            }
        }
        Ok(installed)
    }
}

/// A segment's header, fetched again when it fails its checksum, as an
/// extent is; None when the segment is absent.
fn header(remote: &mut dyn RemoteGate, name: &str, buf: &mut [u8]) -> Result<Option<format::SegmentHeader>, StoreError> {
    let mut last = String::new();
    for _ in 0..FETCH_TRIES_MAX {
        match remote.get_range(name, 0, buf) {
            Ok(None) => return Ok(None),
            Ok(Some(n)) => match format::decode_segment_header(&buf[..n]) {
                Ok(h) => return Ok(Some(h)),
                Err(e) => last = e.to_string(),
            },
            Err(RemoteError::Unavailable(s)) => last = s,
            Err(e) => return Err(from_remote(e)),
        }
    }
    Err(StoreError::Corrupt(format!("{name}, after {FETCH_TRIES_MAX} tries: {last}")))
}

/// Bytes of a pack's object a wake fetches first, to read its header.
pub const PACK_HEADER_FETCH_BYTES: usize = 16 + 4 + 4 + PACK_EXTENTS_MAX as usize * 16 + 32;

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
