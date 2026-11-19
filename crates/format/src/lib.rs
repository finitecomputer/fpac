//! `fpac-format`: what the disk is in the store (docs/design.md, The
//! objects). Every object is written once and never changed; its name says
//! what it is and which life wrote it:
//!
//! - `e<epoch>/s<seq>`: a **segment**, the extents one flush shipped.
//! - `e<epoch>/m<n>-<page>`: a **map page**, where 4,096 extents live,
//!   written by that life's checkpoint `n`.
//! - `e<epoch>/c<n>`: a **checkpoint**, that life's `n`th (from 1): the
//!   map's pages and the live segments, as of the chain's end it names.
//! - `e<epoch>/p<n>`: a **pack**, the extents a boot read, in its order.
//!
//! Integers are little-endian and fixed-width; every object starts with an
//! eight-byte magic and a version, and ends its header with the SHA-256 of
//! what came before. Every extent carries the first eight bytes of its own
//! SHA-256 (`sum64`), checked after every read, from the store and from
//! the cache alike. Decoding refuses anything past a limit, out of order or
//! failing its checksum as a value; it never panics on input.

pub mod limits;

use limits::*;
use sha2::{Digest, Sha256};

pub type Sum = [u8; 32];

pub fn sha256(b: &[u8]) -> Sum {
    Sha256::digest(b).into()
}

/// An extent's checksum: the first eight bytes of its SHA-256. Enough to
/// catch a disk or a store that lies (2^-64), at 8 bytes a map entry.
pub fn sum64(extent: &[u8]) -> u64 {
    assert_eq!(extent.len(), EXTENT_BYTES as usize, "an extent is whole");
    let s = sha256(extent);
    u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]])
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FormatError {
    #[error("{what}: {got} bytes, {need} needed")]
    Short { what: &'static str, need: usize, got: usize },
    #[error("{0}: not this kind of object")]
    Magic(&'static str),
    #[error("{0}: version {1}")]
    Version(&'static str, u32),
    #[error("{0}: past its limit")]
    Limit(&'static str),
    #[error("{0}: out of order or repeated")]
    Order(&'static str),
    #[error("{0}: its checksum does not match")]
    Sum(&'static str),
    #[error("{0}: its layout is inconsistent")]
    Layout(&'static str),
}

/// A segment's identity: the life that wrote it and its number in that
/// life (from 1, without gaps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SegId {
    pub epoch: u64,
    pub seq: u64,
}

/// A link in the chain of segments: the one a segment follows, by its
/// life, number and the SHA-256 of its whole object. A life's first
/// segment follows the end of the checkpoint it opened from; a new disk's
/// first follows `GENESIS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    pub epoch: u64,
    pub seq: u64,
    pub sum: Sum,
}

pub const GENESIS: Link = Link { epoch: 0, seq: 0, sum: [0; 32] };

pub fn segment_name(id: SegId) -> String {
    assert!(id.epoch > 0 && id.seq > 0);
    format!("e{}/s{}", id.epoch, id.seq)
}

pub fn checkpoint_name(epoch: u64, seq: u64) -> String {
    assert!(epoch > 0);
    format!("e{epoch}/c{seq}")
}

pub fn page_name(epoch: u64, seq: u64, page: u32) -> String {
    assert!(epoch > 0 && page < PAGES_MAX);
    format!("e{epoch}/m{seq}-{page}")
}

pub fn pack_name(epoch: u64, seq: u64) -> String {
    assert!(epoch > 0);
    format!("e{epoch}/p{seq}")
}

/// Where an extent's bytes live: a segment and the offset of its image in
/// the segment's object, or nowhere (`len` 0: the extent reads as zeros,
/// as after a discard).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loc {
    pub seg: SegId,
    pub offset: u32,
    pub len: u32,
    pub sum: u64,
}

impl Loc {
    pub fn is_zero(&self) -> bool {
        self.len == 0
    }
}

struct W(Vec<u8>);

impl W {
    fn u32(&mut self, x: u32) {
        self.0.extend_from_slice(&x.to_le_bytes());
    }
    fn u64(&mut self, x: u64) {
        self.0.extend_from_slice(&x.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn seal(&mut self) {
        let s = sha256(&self.0);
        self.0.extend_from_slice(&s);
    }
}

struct R<'a> {
    b: &'a [u8],
    at: usize,
    what: &'static str,
}

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        if self.b.len() - self.at < n {
            return Err(FormatError::Short { what: self.what, need: self.at + n, got: self.b.len() });
        }
        let s = &self.b[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32, FormatError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4")))
    }
    fn u64(&mut self) -> Result<u64, FormatError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8")))
    }
    fn sum(&mut self) -> Result<Sum, FormatError> {
        Ok(self.take(32)?.try_into().expect("32"))
    }
    fn magic(&mut self, m: &[u8; 8], version: u32) -> Result<(), FormatError> {
        if self.take(8)? != m {
            return Err(FormatError::Magic(self.what));
        }
        let v = self.u32()?;
        if v == version {
            Ok(())
        } else {
            Err(FormatError::Version(self.what, v))
        }
    }
    /// The header's own checksum: the SHA-256 of everything read so far.
    fn sealed(&mut self) -> Result<(), FormatError> {
        let want = sha256(&self.b[..self.at]);
        if self.sum()? == want {
            Ok(())
        } else {
            Err(FormatError::Sum(self.what))
        }
    }
}

const SEGMENT_MAGIC: &[u8; 8] = b"FPACSEG1";
const PAGE_MAGIC: &[u8; 8] = b"FPACMAP1";
const CHECKPOINT_MAGIC: &[u8; 8] = b"FPACCKP1";
const PACK_MAGIC: &[u8; 8] = b"FPACPAK1";
const VERSION: u32 = 1;
const LAST_OF_CUT: u32 = 1;

/// One extent in a segment: its number on the disk, where its image is in
/// the object (`len` 0 for a zeroed extent, which has none), its sum64.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub extent: u64,
    pub offset: u32,
    pub len: u32,
    pub sum: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentHeader {
    pub id: SegId,
    /// The link this segment follows.
    pub base: Link,
    /// The flush this segment is part of; a flush bigger than one segment
    /// counts only when its last segment is there.
    pub cut: u64,
    pub last_of_cut: bool,
    pub entries: Vec<Entry>,
    /// The header's own SHA-256 (its last 32 bytes): what the next
    /// segment's base names, so a replay checks the chain from headers.
    pub seal: Sum,
}

impl SegmentHeader {
    /// The link a segment after this one follows.
    pub fn link(&self) -> Link {
        Link { epoch: self.id.epoch, seq: self.id.seq, sum: self.seal }
    }

    pub fn header_len(&self) -> usize {
        segment_header_len(self.entries.len())
    }
}

pub fn segment_header_len(count: usize) -> usize {
    SEGMENT_FIXED_BYTES as usize + count * SEGMENT_ENTRY_BYTES as usize + 32
}

/// An extent to ship: its number and its bytes, or `None` for zeros.
pub struct Image<'a> {
    pub extent: u64,
    pub bytes: Option<&'a [u8]>,
}

/// A segment's object. `images` are in ascending extent order, each
/// extent once, at most `SEGMENT_EXTENTS_MAX`.
pub fn encode_segment(id: SegId, base: Link, cut: u64, last_of_cut: bool, images: &[Image<'_>]) -> Vec<u8> {
    assert!(id.epoch > 0 && id.seq > 0);
    assert!(!images.is_empty() && images.len() <= SEGMENT_EXTENTS_MAX as usize);
    assert!(images.windows(2).all(|w| w[0].extent < w[1].extent), "ascending, each once");
    let header = segment_header_len(images.len());
    let data: usize = images.iter().filter(|i| i.bytes.is_some()).count() * EXTENT_BYTES as usize;
    let mut w = W(Vec::with_capacity(header + data));
    w.bytes(SEGMENT_MAGIC);
    w.u32(VERSION);
    w.u32(if last_of_cut { LAST_OF_CUT } else { 0 });
    w.u64(id.epoch);
    w.u64(id.seq);
    w.u64(base.epoch);
    w.u64(base.seq);
    w.bytes(&base.sum);
    w.u64(cut);
    w.u32(u32::try_from(images.len()).expect("bounded"));
    w.u32(0);
    assert_eq!(w.0.len(), SEGMENT_FIXED_BYTES as usize);
    let mut offset = u32::try_from(header).expect("bounded");
    for i in images {
        assert!(i.extent < EXTENTS_MAX);
        w.u64(i.extent);
        match i.bytes {
            Some(b) => {
                w.u32(offset);
                w.u32(EXTENT_BYTES);
                w.u64(sum64(b));
                offset += EXTENT_BYTES;
            }
            None => {
                w.u32(0);
                w.u32(0);
                w.u64(0);
            }
        }
    }
    w.seal();
    assert_eq!(w.0.len(), header);
    for i in images {
        if let Some(b) = i.bytes {
            w.bytes(b);
        }
    }
    assert_eq!(w.0.len(), header + data);
    w.0
}

/// A segment's header, from a prefix of its object at least as long as
/// the header (a replay fetches `SEGMENT_HEADER_BYTES_MAX`, or less of a
/// smaller object).
pub fn decode_segment_header(b: &[u8]) -> Result<SegmentHeader, FormatError> {
    let mut r = R { b, at: 0, what: "a segment" };
    r.magic(SEGMENT_MAGIC, VERSION)?;
    let flags = r.u32()?;
    if flags & !LAST_OF_CUT != 0 {
        return Err(FormatError::Layout("a segment's flags"));
    }
    let id = SegId { epoch: r.u64()?, seq: r.u64()? };
    let base = Link { epoch: r.u64()?, seq: r.u64()?, sum: r.sum()? };
    let cut = r.u64()?;
    let count = r.u32()?;
    let _reserved = r.u32()?;
    if id.epoch == 0 || id.seq == 0 {
        return Err(FormatError::Layout("a segment's identity"));
    }
    if count == 0 || count > SEGMENT_EXTENTS_MAX {
        return Err(FormatError::Limit("a segment's extents"));
    }
    let header = segment_header_len(count as usize);
    let mut entries = Vec::with_capacity(count as usize);
    let mut offset = u32::try_from(header).expect("bounded");
    for _ in 0..count {
        let e = Entry { extent: r.u64()?, offset: r.u32()?, len: r.u32()?, sum: r.u64()? };
        if e.extent >= EXTENTS_MAX {
            return Err(FormatError::Limit("an extent's number"));
        }
        if entries.last().is_some_and(|p: &Entry| p.extent >= e.extent) {
            return Err(FormatError::Order("a segment's extents"));
        }
        let placed = match e.len {
            0 => e.offset == 0 && e.sum == 0,
            EXTENT_BYTES => e.offset == offset,
            _ => false,
        };
        if !placed {
            return Err(FormatError::Layout("an extent's place in its segment"));
        }
        offset += e.len;
        entries.push(e);
    }
    r.sealed()?;
    let seal: Sum = b[r.at - 32..r.at].try_into().expect("32 bytes");
    Ok(SegmentHeader { id, base, cut, last_of_cut: flags & LAST_OF_CUT != 0, entries, seal })
}

/// A map page: the locations of the extents of page `page` that are
/// written, by their index in the page, ascending.
pub fn encode_page(page: u32, entries: &[(u32, Loc)]) -> Vec<u8> {
    assert!(page < PAGES_MAX && entries.len() <= PAGE_EXTENTS as usize);
    assert!(entries.windows(2).all(|w| w[0].0 < w[1].0));
    let mut w = W(Vec::with_capacity(24 + entries.len() * 40 + 32));
    w.bytes(PAGE_MAGIC);
    w.u32(VERSION);
    w.u32(page);
    w.u32(u32::try_from(entries.len()).expect("bounded"));
    for (i, l) in entries {
        assert!(*i < PAGE_EXTENTS);
        w.u32(*i);
        w.u32(l.len);
        w.u64(l.seg.epoch);
        w.u64(l.seg.seq);
        w.u32(l.offset);
        w.u32(0);
        w.u64(l.sum);
    }
    w.seal();
    w.0
}

pub fn decode_page(b: &[u8]) -> Result<(u32, Vec<(u32, Loc)>), FormatError> {
    let mut r = R { b, at: 0, what: "a map page" };
    r.magic(PAGE_MAGIC, VERSION)?;
    let page = r.u32()?;
    let count = r.u32()?;
    if page >= PAGES_MAX {
        return Err(FormatError::Limit("a page's number"));
    }
    if count > PAGE_EXTENTS {
        return Err(FormatError::Limit("a page's entries"));
    }
    let mut out: Vec<(u32, Loc)> = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let i = r.u32()?;
        let len = r.u32()?;
        let seg = SegId { epoch: r.u64()?, seq: r.u64()? };
        let offset = r.u32()?;
        let _reserved = r.u32()?;
        let sum = r.u64()?;
        if i >= PAGE_EXTENTS {
            return Err(FormatError::Limit("an entry's index"));
        }
        if out.last().is_some_and(|(p, _)| *p >= i) {
            return Err(FormatError::Order("a page's entries"));
        }
        let shaped = match len {
            0 => offset == 0 && sum == 0,
            EXTENT_BYTES => seg.epoch > 0 && seg.seq > 0 && offset >= SEGMENT_FIXED_BYTES,
            _ => false,
        };
        if !shaped {
            return Err(FormatError::Layout("a page's entry"));
        }
        out.push((i, Loc { seg, offset, len, sum }));
    }
    r.sealed()?;
    if r.at != b.len() {
        return Err(FormatError::Layout("bytes past a page"));
    }
    Ok((page, out))
}

/// Where a page's current object is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRef {
    pub page: u32,
    /// The checkpoint that wrote it: `e<epoch>/m<seq>-<page>`.
    pub epoch: u64,
    pub seq: u64,
    pub sum: Sum,
}

/// A pack: `e<epoch>/p<seq>`, with the SHA-256 of its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackRef {
    pub epoch: u64,
    pub seq: u64,
    pub header_sum: Sum,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// It is `e<epoch>/c<seq>`: the life that wrote it, and its number in
    /// that life (from 1). What it covers is `end`.
    pub epoch: u64,
    pub seq: u64,
    /// The chain's end it covers: a life's next segment follows this.
    pub end: Link,
    pub volume_bytes: u64,
    pub pack: Option<PackRef>,
    /// Every page that has an entry, ascending.
    pub pages: Vec<PageRef>,
    /// Every segment the map refers to, ascending, with its object's
    /// SHA-256: what a cleaner must keep.
    pub segments: Vec<(SegId, Sum)>,
}

pub fn encode_checkpoint(c: &Checkpoint) -> Vec<u8> {
    assert!(c.epoch > 0 && c.seq > 0 && c.volume_bytes > 0 && c.volume_bytes <= VOLUME_BYTES_MAX);
    assert!(c.pages.len() <= PAGES_MAX as usize && c.segments.len() <= LIVE_SEGMENTS_MAX as usize);
    assert!(c.pages.windows(2).all(|w| w[0].page < w[1].page));
    assert!(c.segments.windows(2).all(|w| w[0].0 < w[1].0));
    let mut w = W(Vec::new());
    w.bytes(CHECKPOINT_MAGIC);
    w.u32(VERSION);
    w.u32(u32::from(c.pack.is_some()));
    w.u64(c.epoch);
    w.u64(c.seq);
    w.u64(c.end.epoch);
    w.u64(c.end.seq);
    w.bytes(&c.end.sum);
    w.u64(c.volume_bytes);
    let p = c.pack.unwrap_or(PackRef { epoch: 0, seq: 0, header_sum: [0; 32] });
    w.u64(p.epoch);
    w.u64(p.seq);
    w.bytes(&p.header_sum);
    w.u32(u32::try_from(c.pages.len()).expect("bounded"));
    w.u32(u32::try_from(c.segments.len()).expect("bounded"));
    for pr in &c.pages {
        w.u32(pr.page);
        w.u32(0);
        w.u64(pr.epoch);
        w.u64(pr.seq);
        w.bytes(&pr.sum);
    }
    for (id, s) in &c.segments {
        w.u64(id.epoch);
        w.u64(id.seq);
        w.bytes(s);
    }
    w.seal();
    assert!(w.0.len() as u64 <= CHECKPOINT_BYTES_MAX);
    w.0
}

pub fn decode_checkpoint(b: &[u8]) -> Result<Checkpoint, FormatError> {
    if b.len() as u64 > CHECKPOINT_BYTES_MAX {
        return Err(FormatError::Limit("a checkpoint's size"));
    }
    let mut r = R { b, at: 0, what: "a checkpoint" };
    r.magic(CHECKPOINT_MAGIC, VERSION)?;
    let flags = r.u32()?;
    if flags > 1 {
        return Err(FormatError::Layout("a checkpoint's flags"));
    }
    let (epoch, seq) = (r.u64()?, r.u64()?);
    let end = Link { epoch: r.u64()?, seq: r.u64()?, sum: r.sum()? };
    let volume_bytes = r.u64()?;
    let pack = PackRef { epoch: r.u64()?, seq: r.u64()?, header_sum: r.sum()? };
    let (pages_n, segs_n) = (r.u32()?, r.u32()?);
    if epoch == 0 || seq == 0 || volume_bytes == 0 || volume_bytes > VOLUME_BYTES_MAX || volume_bytes % EXTENT_BYTES as u64 != 0 {
        return Err(FormatError::Layout("a checkpoint's identity or volume"));
    }
    if pages_n > PAGES_MAX || segs_n > LIVE_SEGMENTS_MAX {
        return Err(FormatError::Limit("a checkpoint's lists"));
    }
    let mut pages: Vec<PageRef> = Vec::with_capacity(pages_n as usize);
    for _ in 0..pages_n {
        let page = r.u32()?;
        let _ = r.u32()?;
        let pr = PageRef { page, epoch: r.u64()?, seq: r.u64()?, sum: r.sum()? };
        if pr.page >= PAGES_MAX || pr.epoch == 0 {
            return Err(FormatError::Layout("a checkpoint's page"));
        }
        if pages.last().is_some_and(|p| p.page >= pr.page) {
            return Err(FormatError::Order("a checkpoint's pages"));
        }
        pages.push(pr);
    }
    let mut segments: Vec<(SegId, Sum)> = Vec::with_capacity(segs_n as usize);
    for _ in 0..segs_n {
        let id = SegId { epoch: r.u64()?, seq: r.u64()? };
        let s = r.sum()?;
        if id.epoch == 0 || id.seq == 0 {
            return Err(FormatError::Layout("a checkpoint's segment"));
        }
        if segments.last().is_some_and(|(p, _)| *p >= id) {
            return Err(FormatError::Order("a checkpoint's segments"));
        }
        segments.push((id, s));
    }
    r.sealed()?;
    if r.at != b.len() {
        return Err(FormatError::Layout("bytes past a checkpoint"));
    }
    let pack = if flags == 1 { Some(pack) } else { None };
    Ok(Checkpoint { epoch, seq, end, volume_bytes, pack, pages, segments })
}

/// A pack's header: its extents in the order they will be read, each with
/// the sum64 its bytes had. An entry is used at a wake only where the map
/// still names that sum; otherwise its extent changed since and is read
/// as a miss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackHeader {
    pub entries: Vec<(u64, u64)>,
}

impl PackHeader {
    pub fn len(&self) -> usize {
        pack_header_len(self.entries.len())
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

pub fn pack_header_len(count: usize) -> usize {
    PACK_FIXED_BYTES as usize + count * PACK_ENTRY_BYTES as usize + 32
}

/// A pack's object: its header, then each extent's bytes in order.
pub fn encode_pack(extents: &[(u64, &[u8])]) -> Vec<u8> {
    assert!(!extents.is_empty() && extents.len() <= PACK_EXTENTS_MAX as usize);
    let mut w = W(Vec::with_capacity(pack_header_len(extents.len()) + extents.len() * EXTENT_BYTES as usize));
    w.bytes(PACK_MAGIC);
    w.u32(VERSION);
    w.u32(0);
    w.u32(u32::try_from(extents.len()).expect("bounded"));
    w.u32(0);
    for (e, b) in extents {
        assert!(*e < EXTENTS_MAX);
        w.u64(*e);
        w.u64(sum64(b));
    }
    w.seal();
    for (_, b) in extents {
        w.bytes(b);
    }
    w.0
}

/// A pack's header from a prefix of its object.
pub fn decode_pack_header(b: &[u8]) -> Result<PackHeader, FormatError> {
    let mut r = R { b, at: 0, what: "a pack" };
    r.magic(PACK_MAGIC, VERSION)?;
    let _ = r.u32()?;
    let count = r.u32()?;
    let _ = r.u32()?;
    if count == 0 || count > PACK_EXTENTS_MAX {
        return Err(FormatError::Limit("a pack's extents"));
    }
    let mut entries = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (e, s) = (r.u64()?, r.u64()?);
        if e >= EXTENTS_MAX {
            return Err(FormatError::Limit("an extent's number"));
        }
        entries.push((e, s));
    }
    r.sealed()?;
    Ok(PackHeader { entries })
}

/// What a cleaner may delete (docs/design.md: the platform deletes at its
/// own pace, from its own state, never on a guest's call). An object is
/// kept if any kept checkpoint names it (itself, its pages, its live
/// segments, its pack), or if it belongs to a life at or after the oldest
/// kept checkpoint's (a life still writing, or one a replay may read), or
/// if it is younger than `age_ms_min`. Everything else is deleted.
/// `objects` is (name, age in ms); names that are not FPAC's are kept.
pub fn plan_deletes(kept: &[Checkpoint], objects: &[(String, u64)], age_ms_min: u64) -> Vec<String> {
    assert!(!kept.is_empty(), "a cleaner keeps at least the head's checkpoint");
    let mut keep = std::collections::BTreeSet::new();
    let mut oldest_epoch = u64::MAX;
    for c in kept {
        oldest_epoch = oldest_epoch.min(c.epoch);
        keep.insert(checkpoint_name(c.epoch, c.seq));
        for p in &c.pages {
            keep.insert(page_name(p.epoch, p.seq, p.page));
        }
        for (id, _) in &c.segments {
            keep.insert(segment_name(*id));
        }
        if let Some(p) = c.pack {
            keep.insert(pack_name(p.epoch, p.seq));
        }
    }
    let epoch_of = |n: &str| n.strip_prefix('e').and_then(|r| r.split_once('/')).and_then(|(e, _)| e.parse::<u64>().ok());
    objects
        .iter()
        .filter(|(n, age)| {
            let ours = epoch_of(n).is_some();
            let recent = *age < age_ms_min;
            let live_life = epoch_of(n).is_some_and(|e| e >= oldest_epoch);
            ours && !recent && !live_life && !keep.contains(n)
        })
        .map(|(n, _)| n.clone())
        .collect()
}

#[cfg(test)]
mod tests;
