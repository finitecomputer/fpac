//! The cleaner (docs/design.md, The cleaner; plan 3a): what keeps a disk's
//! store from growing with every rewrite. Two parts, both the platform's,
//! never the guest's:
//!
//! - **Compaction** (`compact`): a life of its own, the platform's, while
//!   the disk is asleep. It opens the disk as any life does, loads every
//!   map page, chooses the segments mostly dead (their live extents under
//!   COMPACT_LIVE_FRACTION_MAX of their images), and writes their live
//!   extents again: the store cuts them into new, dense segments, lands
//!   them, checkpoints, and the head moves to that checkpoint by the
//!   platform's own rules. The old segments are then named by no
//!   checkpoint. Every rule of a life holds, so a crash anywhere leaves a
//!   disk that opens (each cut is whole). It runs where its rewrites pay
//!   no container egress: the platform's Worker in the product, through
//!   the same gate. A sealed disk is not compacted here: its map pages
//!   are sealed (docs/encryption.md), so that is the guest's to do.
//! - **Deletes** (`fpac_format::plan_deletes`): planned from the stored
//!   head and the checkpoints kept before it, of objects at least
//!   CLEAN_AGE_MS_MIN old, at the platform's own pace (`kept_checkpoints`).

use std::collections::BTreeMap;

use fpac_format::limits::{EXTENT_BYTES, SEGMENT_FIXED_BYTES};
use fpac_format::{self as format, SegId};

use crate::{ship, Head, MemCache, OpenInfo, RemoteGate, Store, StoreError};

/// A segment is compacted when its live extents are under this share of
/// its images (in hundredths).
pub const COMPACT_LIVE_PERCENT_MAX: u64 = 50;
/// Bytes one pass moves, at most: a pass is bounded in time and in the
/// memory its cache holds (each cut's extents leave it once landed).
pub const COMPACT_BYTES_MAX: u64 = 1 << 30;
/// Extents moved between cuts: one cut's worth at most, so the cleaner's
/// cache holds a cut's extents and no more.
const COMPACT_CUT_EXTENTS: usize = 1024;
/// Tries of one put (an unavailable store is tried again; a conflict is
/// another writer and stops the pass).
const SHIP_TRIES: u32 = 8;
/// Objects younger than this are never deleted (plan 3a): a replay or a
/// life still writing may need them.
pub const CLEAN_AGE_MS_MIN: u64 = 24 * 60 * 60 * 1000;
/// Checkpoints kept: the head's and those just before it, so an older one
/// can still be opened if the newest proves wrong.
pub const KEPT_CHECKPOINTS: usize = 3;

/// What a compaction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    /// Segments the last checkpoint listed as live, and the new one.
    pub live_before: usize,
    pub live_after: usize,
    pub segments_compacted: usize,
    pub extents_moved: u64,
    /// The head it posted (clean), or None when nothing was worth moving
    /// (then it posts the head it opened with, marked clean, so the life
    /// ends as a sleep does).
    pub head: Head,
}

/// Compacts a disk as the life `info` names (an epoch the platform minted
/// for the cleaner, the disk asleep). `sizes` is each segment object's
/// size in bytes, from the store's listing (how many images a segment
/// holds). `post` posts a head and answers whether the platform took it.
/// Moves at most `budget` bytes.
pub fn compact(remote: &mut dyn RemoteGate, info: &OpenInfo, sizes: &BTreeMap<SegId, u64>, budget: u64, mut post: impl FnMut(&Head) -> bool) -> Result<Compacted, StoreError> {
    assert!(budget <= COMPACT_BYTES_MAX);
    let mut s = Store::open(remote, info)?;
    if s.sealed() {
        return Err(StoreError::Refused("a sealed disk is compacted by its guest: its map pages are sealed".into()));
    }
    let mut cache = MemCache::default();
    // this life's head first, as every life's
    let (objs, head) = s.prepare_head();
    for o in &objs {
        ship(remote, o, SHIP_TRIES)?;
    }
    if !objs.is_empty() {
        s.checkpoint_landed(&objs);
    }
    if !post(&head) {
        return Err(StoreError::Fenced("the platform refused the cleaner's head".into()));
    }
    s.head_posted();
    // every page: the live counts are exact only then
    let pages: Vec<_> = s.unloaded.values().copied().collect();
    for pr in pages {
        s.load_page(remote, pr)?;
    }
    let live_before = s.checkpoint.as_ref().map_or(0, |c| c.segments.len());
    let mut live: BTreeMap<SegId, Vec<u64>> = BTreeMap::new();
    for (e, l) in &s.map {
        if !l.is_zero() {
            live.entry(l.seg).or_default().push(*e);
        }
    }
    // the mostly dead, the emptiest first, within the budget
    let images = |seg: &SegId| sizes.get(seg).map(|b| b.saturating_sub(u64::from(SEGMENT_FIXED_BYTES)) / u64::from(EXTENT_BYTES)).unwrap_or(0);
    let mut chosen: Vec<(SegId, Vec<u64>)> = live.into_iter().filter(|(seg, ext)| seg.epoch != info.epoch && images(seg) > 0 && (ext.len() as u64) * 100 < images(seg) * COMPACT_LIVE_PERCENT_MAX).collect();
    chosen.sort_by_key(|(seg, ext)| ((ext.len() as u64) * 1000 / images(seg).max(1), *seg));
    let mut moved = 0u64;
    let mut take = Vec::new();
    for (seg, ext) in chosen {
        if (moved + ext.len() as u64) * u64::from(EXTENT_BYTES) > budget {
            break;
        }
        moved += ext.len() as u64;
        take.push((seg, ext));
    }
    let segments_compacted = take.len();
    // write each live extent again: the next cuts carry it to new segments
    let mut buf = vec![0u8; EXTENT_BYTES as usize];
    let mut since_cut = 0;
    for (_, ext) in &take {
        for &e in ext {
            s.read(remote, &mut cache, e, &mut buf)?;
            s.write(remote, &mut cache, e, 0, &buf)?;
            since_cut += 1;
            if since_cut == COMPACT_CUT_EXTENTS {
                ship_cut(&mut s, remote, &mut cache)?;
                since_cut = 0;
            }
        }
    }
    ship_cut(&mut s, remote, &mut cache)?;
    if s.checkpoint_due() {
        let out = s.checkpoint();
        for o in &out {
            ship(remote, o, SHIP_TRIES)?;
        }
        s.checkpoint_landed(&out);
    }
    let head = s.clean_head();
    if !post(&head) {
        return Err(StoreError::Fenced("the platform refused the cleaner's last head".into()));
    }
    let live_after = s.checkpoint.as_ref().map_or(0, |c| c.segments.len());
    Ok(Compacted { live_before, live_after, segments_compacted, extents_moved: moved, head })
}

/// Cuts what was written, ships it in order and lands it; the landed
/// extents leave the cleaner's cache.
fn ship_cut(s: &mut Store, remote: &mut dyn RemoteGate, cache: &mut MemCache) -> Result<(), StoreError> {
    s.cut()?;
    // Bounded by the segments the cut planned.
    while let Some(o) = s.build_next(cache)? {
        ship(remote, &o, SHIP_TRIES)?;
        let crate::Kind::Segment(seq) = o.kind else { unreachable!("a build is a segment") };
        for e in s.landed(seq) {
            s.evict(cache, e)?;
        }
    }
    Ok(())
}

/// The checkpoints a delete pass keeps: the head's, and the
/// KEPT_CHECKPOINTS - 1 before it in the chain's order, from the names of
/// the checkpoint objects the store holds. Answers (epoch, seq) pairs.
pub fn kept_checkpoints(head: &Head, names: &[String]) -> Vec<(u64, u64)> {
    let Some(h) = head.checkpoint else { return Vec::new() };
    let mut all: Vec<(u64, u64)> = names
        .iter()
        .filter_map(|n| {
            let (e, rest) = n.strip_prefix('e')?.split_once('/')?;
            let seq = rest.strip_prefix('c')?;
            Some((e.parse().ok()?, seq.parse().ok()?))
        })
        .filter(|&c| c <= (h.epoch, h.seq))
        .collect();
    all.sort_unstable();
    all.dedup();
    let keep = all.len().saturating_sub(KEPT_CHECKPOINTS);
    let kept = all.split_off(keep);
    assert!(kept.last() == Some(&(h.epoch, h.seq)), "the head's checkpoint is in the store");
    kept
}

/// A checkpoint as a delete pass reads it, plain or a sealed disk's
/// (authenticated, read without the key).
pub fn read_checkpoint(body: &[u8]) -> Result<format::Checkpoint, StoreError> {
    format::decode_checkpoint_unverified(body).map_err(|e| StoreError::Corrupt(format!("a checkpoint: {e}")))
}
