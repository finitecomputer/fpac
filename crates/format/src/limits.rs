//! The store's limits, each a named constant with its reason
//! (docs/design.md, Limits). Relations between them are asserted at
//! compile time; a value past one is refused where it arrives, never
//! truncated.

/// The unit fetched, shipped and checked: one extent of the disk.
pub const EXTENT_BYTES: u32 = 64 << 10;
/// The largest volume: 100 GB is the goal; 1 TiB leaves room.
pub const VOLUME_BYTES_MAX: u64 = 1 << 40;
/// Extents in the largest volume.
pub const EXTENTS_MAX: u64 = VOLUME_BYTES_MAX / EXTENT_BYTES as u64;

/// A segment's size the cut aims for: puts cost about the same up to
/// 8 MiB (docs/spike.md, P2's path), so fewer, larger ones.
pub const SEGMENT_BYTES_TARGET: u64 = 8 << 20;
/// A segment's largest size: under the intercept's 100 MiB body limit.
pub const SEGMENT_BYTES_MAX: u64 = 64 << 20;
/// Extents in one segment.
pub const SEGMENT_EXTENTS_MAX: u32 = (SEGMENT_BYTES_MAX / EXTENT_BYTES as u64) as u32 - 1;
/// Extents a segment of a cut takes at most: the target size.
pub const SEGMENT_EXTENTS_TARGET: u32 = (SEGMENT_BYTES_TARGET / EXTENT_BYTES as u64) as u32;
/// What may be written and not yet cut, at most. A cut takes every dirty
/// extent at one instant (a subset would reorder writes across the
/// flushes ext4 orders its journal by), copying them under the store's
/// lock; past this, writes wait for the next cut.
pub const DIRTY_BYTES_MAX: u64 = 256 << 20;
pub const DIRTY_EXTENTS_MAX: u32 = (DIRTY_BYTES_MAX / EXTENT_BYTES as u64) as u32;
/// Segments one cut may need.
pub const CUT_SEGMENTS_MAX: u32 = DIRTY_EXTENTS_MAX.div_ceil(SEGMENT_EXTENTS_TARGET);

/// A map page covers this many extents (256 MiB of the disk), so a
/// checkpoint rewrites only the pages whose extents changed.
pub const PAGE_EXTENTS: u32 = 4096;
/// A map page's object at most: its header, its entries, its checksum.
pub const PAGE_BYTES_MAX: u64 = 24 + PAGE_EXTENTS as u64 * 40 + 32;
/// Pages in the largest volume.
pub const PAGES_MAX: u32 = (EXTENTS_MAX / PAGE_EXTENTS as u64) as u32;

/// Segments that may be live at once (referenced by the map): a 100 GB
/// disk at 8 MiB is about 12,000.
pub const LIVE_SEGMENTS_MAX: u32 = 16_384;
/// Segments a checkpoint may leave behind it, and so the most an open
/// replays. One more is corruption.
pub const CHECKPOINT_SEGMENTS: u32 = 256;
pub const REPLAY_SEGMENTS_MAX: u32 = CHECKPOINT_SEGMENTS;

/// A wake's one boot pack, at most: one extent short of 96 MiB, so the
/// whole object, its header too, is within the route's object limit
/// (the store asserts it against the contract's `OBJECT_BYTES_MAX`).
pub const PACK_EXTENTS_MAX: u32 = (96 << 20) / EXTENT_BYTES - 1;
pub const PACK_BYTES_MAX: u64 = PACK_EXTENTS_MAX as u64 * EXTENT_BYTES as u64;
/// A pack's header at most: the fixed part, an entry per extent, its seal.
pub const PACK_HEADER_BYTES_MAX: u64 = PACK_FIXED_BYTES + PACK_EXTENTS_MAX as u64 * PACK_ENTRY_BYTES + 32;
pub const PACK_FIXED_BYTES: u64 = 24;
pub const PACK_ENTRY_BYTES: u64 = 16;
/// A pack's whole object at most.
pub const PACK_OBJECT_BYTES_MAX: u64 = PACK_HEADER_BYTES_MAX + PACK_BYTES_MAX;

/// A miss fetches this much of its segment around the extent asked for:
/// a 1 MiB range costs about what 64 KiB does (docs/spike.md, P2).
pub const READ_AHEAD_BYTES: u32 = 1 << 20;
pub const READ_AHEAD_EXTENTS: u32 = READ_AHEAD_BYTES / EXTENT_BYTES;

/// A fetch whose bytes fail their checksum is fetched again this many
/// times in all before the read fails: a wrong read was seen on
/// Cloudflare (docs/spike.md, Results).
pub const FETCH_TRIES_MAX: u32 = 3;

/// Bytes of a segment header at most: the fixed part, the entries, the
/// header's checksum. A replay fetches only this much of each segment.
pub const SEGMENT_HEADER_BYTES_MAX: u32 = SEGMENT_FIXED_BYTES + SEGMENT_EXTENTS_MAX * SEGMENT_ENTRY_BYTES + 32;
pub const SEGMENT_FIXED_BYTES: u32 = 96;
pub const SEGMENT_ENTRY_BYTES: u32 = 24;

/// A checkpoint object's largest size: its pages and its live segments.
pub const CHECKPOINT_BYTES_MAX: u64 = 128 + PAGES_MAX as u64 * 56 + LIVE_SEGMENTS_MAX as u64 * 48 + 32;

const _: () = assert!(SEGMENT_BYTES_TARGET <= SEGMENT_BYTES_MAX);
const _: () = assert!(SEGMENT_EXTENTS_TARGET <= SEGMENT_EXTENTS_MAX);
const _: () = assert!(CUT_SEGMENTS_MAX * SEGMENT_EXTENTS_TARGET >= DIRTY_EXTENTS_MAX);
const _: () = assert!((SEGMENT_HEADER_BYTES_MAX as u64) + SEGMENT_EXTENTS_MAX as u64 * EXTENT_BYTES as u64 <= SEGMENT_BYTES_MAX);
const _: () = assert!(SEGMENT_BYTES_MAX < 100 << 20, "the intercept's body limit");
const _: () = assert!(PACK_OBJECT_BYTES_MAX < 100 << 20, "the intercept's body limit");
const _: () = assert!(READ_AHEAD_EXTENTS >= 1 && READ_AHEAD_EXTENTS <= SEGMENT_EXTENTS_MAX);
const _: () = assert!(CHECKPOINT_BYTES_MAX < 100 << 20);
