//! The disk as the daemon's threads share it: the store and the cache
//! file under one lock, a pool of connections to the remote, and the
//! FUSE file's backing. The lock is never held across the network: a read
//! that misses plans its fetch under the lock, fetches without it, and
//! installs under it again (docs/design.md, The face). Nor across a
//! segment's build: its 8 MiB are read from the cache file and encoded
//! without it (`read_unlocked`, `Store::build_take`).

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use fpac_device::serve::{Backing, BackingError};
use fpac_format::limits::{AHEAD_EXTENTS_MAX, AHEAD_PIECE_EXTENTS, DIRTY_EXTENTS_MAX, EXTENT_BYTES, FETCH_TRIES_MAX, PACK_BYTES_MAX, UNSHIPPED_BYTES_MAX};
use fpac_remote::{Remote, RemoteError};
use fpac_format::{PageRef, Place, Sealer};
use fpac_store::evict::{Evictor, Taken, Verdict};
use fpac_store::{page_name, CacheGate, Fetch, Need, Read, Store, StoreError};

/// Connections kept to the remote, at most: the FUSE threads' and the
/// flusher's.
pub const CONNECTIONS_MAX: usize = 32;
/// How long a write waits for a cut when too much is unsent, per wait;
/// it waits again until the flusher cuts.
const FULL_WAIT_MS: u64 = 200;
/// How long a read waits for the pack to bring its extent, at most; past
/// it the read fetches the extent itself.
const PACK_WAIT_MS: u64 = 3000;
/// Tries of one read or write, past which it fails (EIO): a fetch that
/// moved under it, or a remote that stays down.
const OP_TRIES_MAX: u32 = 8;
/// Sequential reads followed at once (docs/design.md, Read-ahead): each
/// has a window of AHEAD_EXTENTS_MAX kept ahead of it.
const STREAMS_MAX: usize = 8;
/// How far apart two reads may be and still be one scan (extents): the
/// loop device's requests arrive out of order across FUSE threads.
const STREAM_GAP_EXTENTS: u64 = 8;
/// Connections one read-ahead window fetches on, and windows in flight at
/// once across every scan.
const AHEAD_FETCHES: usize = 8;
const AHEAD_WINDOWS_MAX: usize = 4;

/// What the cache's disk keeps free beyond the shadow's worst case, for
/// everything else on it (local scratch, the guest's own files).
pub const CACHE_DISK_HEADROOM_BYTES: u64 = 2 << 30;
/// The cache's bound never falls under this while it gives room back:
/// the unshipped budget (unshipped extents cannot leave) and the next
/// pack's extents (the boot's, kept for the next wake).
pub const CACHE_FLOOR_BYTES: u64 = UNSHIPPED_BYTES_MAX + PACK_BYTES_MAX;
/// A full cache disk is said in the log at most this often.
const NO_SPACE_LOG_MS: u64 = 10_000;
/// Extents dropped under one hold of the lock while giving room back, so
/// writes beside it wait at most one batch's punches.
const ROOM_DROP_BATCH: usize = 1024;

/// The cache's bound, in extents, for the room its disk has now
/// (docs/design.md, The cache): what the cache holds plus what is free,
/// less what the shadow file may still take (the unshipped budget less
/// what it holds) and CACHE_DISK_HEADROOM_BYTES; never under the floor
/// (CACHE_FLOOR_BYTES), never over `ceiling`, the bound the mount set.
/// At the mount (nothing cached) this is the mount's own rule.
pub fn room_bound(free_bytes: u64, cached_extents: u64, shadow_bytes: u64, ceiling: u64) -> u64 {
    let e = u64::from(EXTENT_BYTES);
    let floor = (CACHE_FLOOR_BYTES / e).min(ceiling);
    let reserve = UNSHIPPED_BYTES_MAX.saturating_sub(shadow_bytes) + CACHE_DISK_HEADROOM_BYTES;
    ((cached_extents * e).saturating_add(free_bytes).saturating_sub(reserve) / e).clamp(floor, ceiling)
}

/// The cache file: one sparse file of the volume's size on the container's
/// own disk, an extent at `extent * EXTENT_BYTES` (a sealed cache's is
/// twice that: two sides, `CacheSeal`); and beside it the
/// shadow file, where a frozen extent's bytes as of its cut wait for its
/// segment's build when a write changes them first, one slot each
/// (bounded by UNSHIPPED_EXTENTS_MAX), its space freed when taken.
pub struct FileCache<F: CacheBytes = File> {
    file: F,
    shadow: File,
    /// Each kept copy's slot, and the seal its bytes were written under.
    slots: std::collections::HashMap<(u64, u64), (u64, Option<Sealed>)>,
    free: Vec<u64>,
    next_slot: u64,
    seal: Option<CacheSeal>,
}

/// A sealed disk's cache (docs/encryption.md): each extent is written
/// sealed, bound to this life, the extent and its count of writes, and
/// only its tag and that count stay in memory. The host's disk holds
/// ciphertext; one that changes it, or hands back an earlier write of the
/// same extent, fails the open.
///
/// A sealed extent is opened whole or not at all, so it is never
/// overwritten in place: a write that failed partway (a full copy-on-write
/// disk refuses even an overwrite) would leave neither copy readable, and
/// the extent may be the only copy of acknowledged bytes. Each extent has
/// two sides, the second a volume further on; a write goes to the side not
/// in use, the tag moves to it once it is whole, and only then is the old
/// side punched.
struct CacheSeal {
    sealer: Arc<Sealer>,
    life: u64,
    writes: u64,
    tags: std::collections::HashMap<u64, Sealed>,
    /// Where the second side starts: the volume's size.
    side_bytes: u64,
}

/// How an extent's bytes in the cache were sealed, and on which side.
#[derive(Debug, Clone, Copy)]
pub struct Sealed {
    generation: u64,
    tag: [u8; 16],
    side: bool,
}

/// The key and life a sealed cache's reads open with (a segment's build
/// reads without the lock).
#[derive(Clone)]
pub struct CacheKey {
    sealer: Arc<Sealer>,
    life: u64,
    side_bytes: u64,
}

/// Where an extent's bytes lie in the cache file: its side's, when sealed.
fn cache_at(extent: u64, sealed: Option<Sealed>, side_bytes: u64) -> u64 {
    let at = extent * EXTENT_BYTES as u64;
    match sealed {
        Some(s) if s.side => side_bytes + at,
        _ => at,
    }
}

impl CacheKey {
    /// Where `extent`, sealed as `sealed` says, lies in the cache file.
    fn at(&self, extent: u64, sealed: Option<Sealed>) -> u64 {
        cache_at(extent, sealed, self.side_bytes)
    }

    /// Opens an extent read from the cache file as `sealed` says it was
    /// written; None never written (zeros).
    fn open(&self, extent: u64, sealed: Option<Sealed>, buf: &mut [u8]) -> std::io::Result<()> {
        match sealed {
            None => {
                buf.fill(0);
                Ok(())
            }
            Some(s) => self.sealer.open_detached(Place::Cache { life: self.life, extent, generation: s.generation }, buf, &s.tag).map_err(|_| std::io::Error::other(format!("extent {extent}: the cache's seal does not open (the host's disk changed it)"))),
        }
    }
}

/// The cache file's bytes: the file itself, or in a test one that fails
/// as a full copy-on-write disk can (even an overwrite refused partway).
pub trait CacheBytes: Send {
    fn get(&self, buf: &mut [u8], at: u64) -> std::io::Result<()>;
    fn put(&self, buf: &[u8], at: u64) -> std::io::Result<()>;
    /// The extent at `at` reads as zeros, its room given back.
    fn punch(&self, at: u64) -> std::io::Result<()>;
}

impl CacheBytes for File {
    fn get(&self, buf: &mut [u8], at: u64) -> std::io::Result<()> {
        self.read_exact_at(buf, at)
    }
    fn put(&self, buf: &[u8], at: u64) -> std::io::Result<()> {
        self.write_all_at(buf, at)
    }
    fn punch(&self, at: u64) -> std::io::Result<()> {
        punch_hole(self, at)
    }
}

impl FileCache {
    pub fn create(path: &Path, volume_bytes: u64, seal: Option<(Arc<Sealer>, u64)>) -> std::io::Result<FileCache> {
        let open = |p: &Path| OpenOptions::new().read(true).write(true).create(true).truncate(true).custom_flags(libc::O_CLOEXEC).open(p);
        let file = open(path)?;
        // a sealed cache's two sides (CacheSeal)
        file.set_len(if seal.is_some() { 2 * volume_bytes } else { volume_bytes })?;
        FileCache::over(file, open(&path.with_extension("shadow"))?, seal, volume_bytes)
    }

    /// A second handle on the cache file, for reads outside the lock.
    pub fn reader(&self) -> std::io::Result<File> {
        self.file.try_clone()
    }
}

impl<F: CacheBytes> FileCache<F> {
    fn over(file: F, shadow: File, seal: Option<(Arc<Sealer>, u64)>, volume_bytes: u64) -> std::io::Result<FileCache<F>> {
        let seal = seal.map(|(sealer, life)| CacheSeal { sealer, life, writes: 0, tags: Default::default(), side_bytes: volume_bytes });
        Ok(FileCache { file, shadow, slots: Default::default(), free: Vec::new(), next_slot: 0, seal })
    }

    /// The key for reads outside the lock, when the cache is sealed.
    pub fn key(&self) -> Option<CacheKey> {
        self.seal.as_ref().map(|c| CacheKey { sealer: c.sealer.clone(), life: c.life, side_bytes: c.side_bytes })
    }

    /// How each extent was last sealed (for reads outside the lock); None
    /// when the cache is plain.
    pub fn seals_of(&self, extents: &[u64]) -> Option<Vec<Option<Sealed>>> {
        self.seal.as_ref().map(|c| extents.iter().map(|e| c.tags.get(e).copied()).collect())
    }

    /// What the shadow file holds now, in bytes.
    pub fn shadow_bytes(&self) -> u64 {
        self.slots.len() as u64 * u64::from(EXTENT_BYTES)
    }
}

fn punch_hole(f: &File, offset: u64) -> std::io::Result<()> {
    let (o, l) = (i64::try_from(offset).expect("bounded"), i64::from(EXTENT_BYTES));
    // SAFETY: a plain syscall on a file we own.
    let r = unsafe { libc::fallocate(f.as_raw_fd(), libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, o, l) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

impl<F: CacheBytes> CacheGate for FileCache<F> {
    fn read(&mut self, extent: u64, buf: &mut [u8]) -> std::io::Result<()> {
        match self.key() {
            Some(k) => {
                let sealed = self.seal.as_ref().and_then(|c| c.tags.get(&extent).copied());
                self.file.get(buf, k.at(extent, sealed))?;
                k.open(extent, sealed, buf)
            }
            None => self.file.get(buf, extent * EXTENT_BYTES as u64),
        }
    }
    fn write(&mut self, extent: u64, bytes: &[u8]) -> std::io::Result<()> {
        let Some(c) = &mut self.seal else { return self.file.put(bytes, extent * EXTENT_BYTES as u64) };
        // the side not in use: the current copy stays whole until this one is
        let old = c.tags.get(&extent).copied();
        let side = old.is_some_and(|s| !s.side);
        c.writes += 1;
        let generation = c.writes;
        let mut sealed = bytes.to_vec();
        let tag = c.sealer.seal_detached(Place::Cache { life: c.life, extent, generation }, &mut sealed);
        let new = Sealed { generation, tag, side };
        let at = cache_at(extent, Some(new), c.side_bytes);
        if let Err(e) = self.file.put(&sealed, at) {
            // a part written is nobody's: its room back, if the disk lets
            let _ = self.file.punch(at);
            return Err(e);
        }
        c.tags.insert(extent, new);
        if let Some(o) = old {
            // the old copy's room back; one left is only room, never read
            let _ = self.file.punch(cache_at(extent, Some(o), c.side_bytes));
        }
        Ok(())
    }
    fn punch(&mut self, extent: u64) -> std::io::Result<()> {
        let at = extent * EXTENT_BYTES as u64;
        self.file.punch(at)?;
        if let Some(c) = &mut self.seal {
            self.file.punch(c.side_bytes + at)?;
            c.tags.remove(&extent);
        }
        Ok(())
    }
    fn shadow_save(&mut self, extent: u64, seq: u64) -> std::io::Result<()> {
        // the bytes as they lie in the file (sealed, when the cache is),
        // and how they were sealed
        let mut b = vec![0u8; EXTENT_BYTES as usize];
        let sealed = self.seal.as_ref().and_then(|c| c.tags.get(&extent).copied());
        self.file.get(&mut b, cache_at(extent, sealed, self.seal.as_ref().map_or(0, |c| c.side_bytes)))?;
        let slot = self.free.pop().unwrap_or_else(|| {
            self.next_slot += 1;
            self.next_slot - 1
        });
        self.shadow.write_all_at(&b, slot * EXTENT_BYTES as u64)?;
        assert!(self.slots.insert((extent, seq), (slot, sealed)).is_none(), "one copy per extent and segment");
        Ok(())
    }
    fn shadow_take(&mut self, extent: u64, seq: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let (slot, sealed) = self.slots.remove(&(extent, seq)).ok_or_else(|| std::io::Error::other("no such shadow"))?;
        self.shadow.read_exact_at(buf, slot * EXTENT_BYTES as u64)?;
        punch_hole(&self.shadow, slot * EXTENT_BYTES as u64)?;
        self.free.push(slot);
        match self.key() {
            Some(k) => k.open(extent, sealed, buf),
            None => Ok(()),
        }
    }
}

pub struct Inner {
    pub store: Store,
    pub cache: FileCache,
}

/// Counters for `fpac status`.
#[derive(Default)]
pub struct Stats {
    pub misses: AtomicU64,
    pub fetch_retries: AtomicU64,
    pub read_errors: AtomicU64,
    pub full_waits: AtomicU64,
    /// The kernel's fsyncs of the file (ext4's journal commits and the
    /// programs' syncs above it, as they reach the device).
    pub syncs: AtomicU64,
    /// Reads that waited for the pack to bring their extent.
    pub pack_waits: AtomicU64,
    /// Clean extents dropped to keep the cache within its bound, and of
    /// those, the ones dropped because its disk was short of room.
    pub evictions: AtomicU64,
    pub room_evictions: AtomicU64,
    /// Writes answered with an error (the kernel sees EIO), and those
    /// refused because the cache file's disk is full (ENOSPC).
    pub write_errors: AtomicU64,
    pub no_space_errors: AtomicU64,
    /// When a refusal for a full disk was last logged (ms since the mount's
    /// epoch), so a full disk is said once each NO_SPACE_LOG_MS, not per
    /// write.
    no_space_logged_ms: AtomicU64,
    /// Map pages fetched (lazy pages).
    pub page_loads: AtomicU64,
    /// Read-ahead: windows started, extents they fetched, reads that
    /// waited for one.
    pub ahead_windows: AtomicU64,
    pub ahead_extents: AtomicU64,
    pub ahead_waits: AtomicU64,
    /// Writes that waited over a millisecond for the lock, and the
    /// longest wait (µs): what a segment's build or a fetch's install
    /// costs the writes beside it.
    pub lock_waits_over_1ms: AtomicU64,
    pub lock_wait_max_us: AtomicU64,
}

/// The first extents a read waited for the pack to bring, kept for the
/// pack's event (which of them a larger pack head would have held).
const WAITED_KEPT: usize = 16;

/// A sequential read being followed: its furthest read, how far its
/// window reaches (exclusive), and its reads that moved it on.
#[derive(Debug, Clone, Copy)]
struct Stream {
    last: u64,
    end: u64,
    seen: u32,
}

/// What `coming` names for an extent read ahead (not the pack's).
const AHEAD_MARK: usize = usize::MAX - 1;

/// What wakes the flusher: a stop, or an event (a put that ended, a
/// writer waiting on a full store).
#[derive(Default)]
pub struct Wake {
    pub stop: bool,
    pub events: u64,
}

pub struct Disk {
    pub inner: Mutex<Inner>,
    /// The cache file, read without the lock (a segment's build), and
    /// its key when it is sealed.
    reader: File,
    reader_key: Option<CacheKey>,
    /// Signaled after every cut, for writes waiting on a full store.
    pub cut: Condvar,
    pool: Mutex<Vec<Remote>>,
    host: String,
    port: u16,
    pub stats: Stats,
    pub fenced: AtomicBool,
    /// Extents the pack is bringing: a read of one waits for it (at most
    /// PACK_WAIT_MS) rather than fetching it twice.
    /// The pack's extents still on their way, each with its place in the
    /// pack.
    coming: Mutex<std::collections::BTreeMap<u64, usize>>,
    came: Condvar,
    /// Where in the pack the first reads that waited for it found their
    /// extents.
    pub waited: Mutex<Vec<usize>>,
    /// The cache's bound, in extents (docs/design.md, The cache): past it,
    /// clean extents leave as `evict` chooses (S3-FIFO); never one that is
    /// unshipped or that a write is reading. It falls while the cache's
    /// disk is short of room (`give_room`), and rises again to
    /// `cache_ceiling`, the mount's.
    cache_max: AtomicUsize,
    cache_ceiling: usize,
    evict: Mutex<Evictor>,
    /// The sequential reads being followed (`ahead`), and read-ahead
    /// windows in flight.
    streams: Mutex<Vec<Stream>>,
    windows: AtomicU64,
    /// The disk's own handle, for the read-ahead's threads (`set_me`).
    me: std::sync::OnceLock<std::sync::Weak<Disk>>,
    /// Extents a partial write is reading first: never dropped between
    /// the fetch of their bytes and the write over them.
    pinned: Mutex<std::collections::HashMap<u64, u32>>,
    /// The flusher waits on this between turns.
    pub wake: (Mutex<Wake>, Condvar),
    /// A write waits on a full cut: the flusher cuts at once.
    pub cut_wanted: AtomicBool,
}

impl Disk {
    pub fn new(store: Store, cache: FileCache, host: &str, port: u16, cache_max_bytes: u64) -> std::io::Result<Disk> {
        let cache_max = usize::try_from(cache_max_bytes / EXTENT_BYTES as u64).expect("bounded by the volume");
        let reader = cache.reader()?;
        let reader_key = cache.key();
        Ok(Disk { inner: Mutex::new(Inner { store, cache }), reader, reader_key, cut: Condvar::new(), pool: Mutex::new(Vec::new()), host: host.to_string(), port, stats: Stats::default(), fenced: AtomicBool::new(false), coming: Mutex::new(Default::default()), came: Condvar::new(), waited: Mutex::new(Vec::new()), cache_max: AtomicUsize::new(cache_max), cache_ceiling: cache_max, evict: Mutex::new(Evictor::new(cache_max)), streams: Mutex::new(Vec::new()), windows: AtomicU64::new(0), me: std::sync::OnceLock::new(), pinned: Mutex::new(Default::default()), wake: (Mutex::new(Wake::default()), Condvar::new()), cut_wanted: AtomicBool::new(false) })
    }

    /// Extents' bytes from the cache file, without the lock: a write
    /// meanwhile may tear one, and `Store::build_resolve` replaces each
    /// extent written since its cut with the copy kept apart before it. A
    /// sealed cache's extents are opened as `seals` (taken under the lock)
    /// says; each answers whether it opened, and one that did not is good
    /// only where a kept copy replaces it.
    pub fn read_unlocked(&self, extents: &[u64], seals: Option<Vec<Option<Sealed>>>) -> std::io::Result<(Vec<Vec<u8>>, Vec<bool>)> {
        let mut out = Vec::with_capacity(extents.len());
        let mut opened = Vec::with_capacity(extents.len());
        for (i, &e) in extents.iter().enumerate() {
            let mut b = vec![0u8; EXTENT_BYTES as usize];
            let ok = match (&self.reader_key, &seals) {
                (Some(k), Some(s)) => {
                    self.reader.read_exact_at(&mut b, k.at(e, s[i]))?;
                    k.open(e, s[i], &mut b).is_ok()
                }
                _ => {
                    self.reader.read_exact_at(&mut b, e * EXTENT_BYTES as u64)?;
                    true
                }
            };
            out.push(b);
            opened.push(ok);
        }
        Ok((out, opened))
    }

    /// Wakes the flusher early: a put ended, or a write waits.
    pub fn nudge(&self) {
        self.wake.0.lock().unwrap().events += 1;
        self.wake.1.notify_all();
    }

    /// A write waits: if on a full cut, the flusher cuts at once; if on
    /// the unshipped budget, only landings help, and the flusher wakes
    /// writers as they come.
    fn want_cut(&self, store: &Store) {
        if store.dirty_extents() >= DIRTY_EXTENTS_MAX as usize && !self.cut_wanted.swap(true, Ordering::SeqCst) {
            self.nudge();
        }
    }

    /// Asks the flusher to stop now rather than at its next turn.
    pub fn stop_flusher(&self) {
        self.wake.0.lock().unwrap().stop = true;
        self.wake.1.notify_all();
    }

    /// Waits until something happened since `seen` events, a stop, or
    /// `timeout`: (stop, events).
    pub fn wait_flusher(&self, seen: u64, timeout: Duration) -> (bool, u64) {
        let g = self.wake.0.lock().unwrap();
        let (g, _) = self.wake.1.wait_timeout_while(g, timeout, |w| !w.stop && w.events == seen).unwrap();
        (g.stop, g.events)
    }

    /// Notes extents the cache just took (`how`: used now, or read ahead),
    /// then, past `cache_max`, drops clean ones as the policy chooses;
    /// never those it just took, which the caller is using (a partial
    /// write's read, say), nor one a write has pinned. Called with the
    /// store's lock held.
    pub(crate) fn took(&self, inner: &mut Inner, extents: impl Iterator<Item = u64>, how: Taken) -> Result<(), StoreError> {
        self.took_each(inner, extents.map(|e| (e, how)))
    }

    /// `took`, each extent with its own way in (a fetch's asked extent is
    /// used now, its read-ahead not yet), in one pass so none of them is
    /// dropped by the others' arrival.
    pub(crate) fn took_each(&self, inner: &mut Inner, extents: impl Iterator<Item = (u64, Taken)>) -> Result<(), StoreError> {
        let mut ev = self.evict.lock().unwrap();
        let mut fresh = std::collections::HashSet::new();
        for (e, how) in extents {
            fresh.insert(e);
            ev.taken(e, how);
        }
        let pinned = self.pinned.lock().unwrap();
        // Bounded: each drop lowers the count, and the policy answers None
        // once nothing it holds may go.
        while inner.store.cached_extents() > self.cache_max.load(Ordering::Relaxed) {
            let store = &inner.store;
            let verdict = |x: u64| match () {
                _ if !store.is_cached(x) => Verdict::Gone,
                _ if fresh.contains(&x) || pinned.contains_key(&x) => Verdict::Keep,
                // unshipped, or the next pack's: not until it is clean
                _ if !store.may_leave(x) => Verdict::Park,
                _ => Verdict::Drop,
            };
            let Some(e) = ev.victim(verdict) else { break };
            if inner.store.evict(&mut inner.cache, e)? {
                self.stats.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    /// Sets the cache's bound for the room its disk has (`room_bound`,
    /// with `free_bytes` from its statvfs) and drops clean extents down to
    /// it at once, ROOM_DROP_BATCH under each hold of the lock: their
    /// holes are punched, so whatever else writes to that disk gets the
    /// room back. The bound rises again as room comes back. Answers the
    /// bound, in extents.
    pub fn give_room(&self, free_bytes: u64) -> Result<usize, StoreError> {
        let (cached, shadow) = {
            let inner = self.inner.lock().unwrap();
            (inner.store.cached_extents(), inner.cache.shadow_bytes())
        };
        let ceiling = self.cache_ceiling as u64;
        let bound = usize::try_from(room_bound(free_bytes, cached as u64, shadow, ceiling)).expect("at most the ceiling");
        let mut now = cached;
        // Bounded: each turn drops a batch, or ends when none can go.
        while now > bound {
            let step = bound.max(now - ROOM_DROP_BATCH.min(now));
            self.cache_max.store(step, Ordering::Relaxed);
            let mut inner = self.inner.lock().unwrap();
            let before = self.stats.evictions.load(Ordering::Relaxed);
            self.took_each(&mut inner, std::iter::empty())?;
            self.stats.room_evictions.fetch_add(self.stats.evictions.load(Ordering::Relaxed) - before, Ordering::Relaxed);
            let after = inner.store.cached_extents();
            if after >= now {
                // nothing more may go (unshipped, pinned, the pack's)
                break;
            }
            now = after;
        }
        self.cache_max.store(bound, Ordering::Relaxed);
        Ok(bound)
    }

    /// The cache's bound now, in extents.
    pub fn cache_bound(&self) -> usize {
        self.cache_max.load(Ordering::Relaxed)
    }

    /// Whether the mount is ending (the flusher was told to stop).
    pub fn stopping(&self) -> bool {
        self.wake.0.lock().unwrap().stop
    }

    /// Gives the disk its own handle, once it is shared: the read-ahead's
    /// threads hold it.
    pub fn set_me(self: &Arc<Self>) {
        let _ = self.me.set(Arc::downgrade(self));
    }

    /// Extents whose cut landed are clean: the policy may drop them again.
    pub fn cleaned(&self, extents: Vec<u64>) {
        if !extents.is_empty() {
            self.evict.lock().unwrap().unpark(extents.into_iter());
        }
    }

    /// A read of `extent` hit the cache: a use, for the policy.
    fn used(&self, extent: u64) {
        self.evict.lock().unwrap().used(extent);
    }

    /// Follows sequential reads (docs/design.md, Read-ahead): a read next to
    /// a scan's last moves it on, and once less than half its window is
    /// ahead of it, the window is extended to AHEAD_EXTENTS_MAX past the
    /// read, fetched in the background on AHEAD_FETCHES connections. A
    /// miss far from every scan starts one (it goes ahead from its second
    /// read). Answers the window to fetch, if any.
    fn follow(&self, extent: u64, missed: bool) -> Option<(u64, u64)> {
        let mut st = self.streams.lock().unwrap();
        let ahead = u64::from(AHEAD_EXTENTS_MAX);
        if let Some(s) = st.iter_mut().find(|s| extent + STREAM_GAP_EXTENTS >= s.last && extent <= s.last + STREAM_GAP_EXTENTS) {
            let moved = extent > s.last;
            s.last = s.last.max(extent);
            s.seen = s.seen.saturating_add(u32::from(moved));
            if s.seen >= 1 && s.end < s.last + 1 + ahead / 2 {
                let from = s.end.max(s.last + 1);
                s.end = s.last + 1 + ahead;
                return Some((from, s.end));
            }
            return None;
        }
        if missed {
            if st.len() == STREAMS_MAX {
                st.remove(0);
            }
            st.push(Stream { last: extent, end: extent + 1, seen: 0 });
        }
        None
    }

    /// Fetches a read-ahead window: what of it the cache lacks and the map
    /// places, in pieces of AHEAD_PIECE_EXTENTS on AHEAD_FETCHES
    /// connections, each installed as it arrives; a read of one waits for
    /// it rather than fetching it twice. At most AHEAD_WINDOWS_MAX at once.
    fn ahead(&self, from: u64, to: u64) {
        let Some(d) = self.me.get().and_then(std::sync::Weak::upgrade) else { return };
        if self.windows.fetch_add(1, Ordering::SeqCst) >= AHEAD_WINDOWS_MAX as u64 {
            self.windows.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        std::thread::spawn(move || {
            let fetches = d.inner.lock().unwrap().store.ahead(from, to, AHEAD_PIECE_EXTENTS as usize);
            if !fetches.is_empty() {
                d.stats.ahead_windows.fetch_add(1, Ordering::Relaxed);
                let all: Vec<u64> = fetches.iter().flat_map(|f| f.extents().collect::<Vec<_>>()).collect();
                d.expect_pack(all.iter().map(|&e| (e, AHEAD_MARK)));
                pool(fetches, AHEAD_FETCHES, |f| {
                    if let Ok(window) = d.get(&f) {
                        let mut g = d.inner.lock().unwrap();
                        let ok = {
                            let Inner { store, cache } = &mut *g;
                            store.install(cache, &f, &window, &mut vec![0u8; EXTENT_BYTES as usize])
                        };
                        if ok.is_ok() {
                            d.stats.ahead_extents.fetch_add(f.extents().count() as u64, Ordering::Relaxed);
                        }
                        let _ = d.took(&mut g, f.extents(), Taken::Ahead);
                    }
                    d.arrived(f.extents());
                });
                d.arrived(all.into_iter());
            }
            d.windows.fetch_sub(1, Ordering::SeqCst);
        });
    }

    pub fn expect_pack(&self, extents: impl Iterator<Item = (u64, usize)>) {
        self.coming.lock().unwrap().extend(extents);
    }

    pub fn arrived(&self, extents: impl Iterator<Item = u64>) {
        let mut c = self.coming.lock().unwrap();
        for e in extents {
            c.remove(&e);
        }
        self.came.notify_all();
    }

    /// Waits while the pack is bringing `extent`, at most PACK_WAIT_MS.
    fn wait_for_pack(&self, extent: u64) {
        let c = self.coming.lock().unwrap();
        if let Some(&at) = c.get(&extent) {
            if at == AHEAD_MARK {
                self.stats.ahead_waits.fetch_add(1, Ordering::Relaxed);
            } else {
                self.stats.pack_waits.fetch_add(1, Ordering::Relaxed);
                let mut w = self.waited.lock().unwrap();
                if w.len() < WAITED_KEPT {
                    w.push(at);
                }
            }
            let _ = self.came.wait_timeout_while(c, Duration::from_millis(PACK_WAIT_MS), |c| c.contains_key(&extent)).unwrap();
        }
    }

    /// A connection from the pool, or a new one.
    pub fn remote(&self) -> Remote {
        self.pool.lock().unwrap().pop().unwrap_or_else(|| Remote::new(&self.host, self.port))
    }

    pub fn give_back(&self, r: Remote) {
        let mut p = self.pool.lock().unwrap();
        if p.len() < CONNECTIONS_MAX {
            p.push(r);
        }
    }

    /// One ranged get for a fetch, without the lock.
    fn get(&self, f: &Fetch) -> Result<Vec<u8>, StoreError> {
        let mut r = self.remote();
        let mut window = vec![0u8; f.len];
        let got = r.get_range(&f.name, f.offset, &mut window);
        self.give_back(r);
        match got {
            Ok(Some(n)) => {
                window.truncate(n);
                Ok(window)
            }
            Ok(None) => Err(StoreError::Corrupt(format!("{} is gone, and the map names it", f.name))),
            Err(e) => Err(StoreError::Unavailable(e.to_string())),
        }
    }

    /// A map page, fetched without the lock and installed under it
    /// (lazy pages: docs/design.md). Every failure of the remote is one to
    /// try again, as a read's or a write's bound allows.
    pub fn load_page(&self, pr: PageRef) -> Result<(), StoreError> {
        self.fetch_page(pr).map_err(|e| match e {
            PageError::Remote(e) => StoreError::Unavailable(e.to_string()),
            PageError::Store(e) => e,
        })
    }

    /// `load_page`, the remote's failure kept as it said it: a sleep's
    /// checkpoint asks again only a remote that is down (flush.rs).
    pub fn fetch_page(&self, pr: PageRef) -> Result<(), PageError> {
        let mut r = self.remote();
        let mut body = Vec::new();
        let got = r.get(&page_name(&pr), &mut |b| body.extend_from_slice(b));
        self.give_back(r);
        match got {
            Ok(Some(_)) => {
                self.stats.page_loads.fetch_add(1, Ordering::Relaxed);
                self.inner.lock().unwrap().store.install_page(pr, &body).map_err(PageError::Store)
            }
            Ok(None) => Err(PageError::Store(StoreError::Corrupt(format!("{} is gone, and a checkpoint names it", page_name(&pr))))),
            Err(e) => Err(PageError::Remote(e)),
        }
    }

    /// Fetches and installs until the extent asked for is in `buf`.
    fn fetch(&self, f: Fetch, buf: &mut [u8]) -> Result<(), StoreError> {
        let mut f = f;
        self.stats.misses.fetch_add(1, Ordering::Relaxed);
        let mut last = String::new();
        for _ in 0..FETCH_TRIES_MAX {
            match self.get(&f) {
                Ok(window) => {
                    let mut g = self.inner.lock().unwrap();
                    let installed = {
                        let Inner { store, cache } = &mut *g;
                        store.install(cache, &f, &window, buf)?
                    };
                    // the one asked for is used now; its read-ahead is not yet
                    self.took_each(&mut g, f.extents().enumerate().map(|(i, e)| (e, if i == 0 { Taken::Used } else { Taken::Ahead })))?;
                    if installed {
                        return Ok(());
                    }
                    last = format!("{}: the extent's checksum does not match", f.name);
                }
                Err(StoreError::Unavailable(s)) => last = s,
                Err(e) => return Err(e),
            }
            self.stats.fetch_retries.fetch_add(1, Ordering::Relaxed);
            f = f.single();
        }
        Err(StoreError::Corrupt(format!("after {FETCH_TRIES_MAX} tries: {last}")))
    }

    /// One whole extent into `buf`.
    pub fn read_extent(&self, extent: u64, buf: &mut [u8]) -> Result<(), StoreError> {
        self.wait_for_pack(extent);
        let mut last = None;
        let mut first = true;
        for _ in 0..OP_TRIES_MAX {
            let (plan, cached) = {
                let mut g = self.inner.lock().unwrap();
                let Inner { store, cache } = &mut *g;
                let cached = store.is_cached(extent);
                (store.read_local(cache, extent, buf)?, cached)
            };
            if first {
                first = false;
                if let Some((from, to)) = self.follow(extent, !matches!(plan, Read::Done)) {
                    self.ahead(from, to);
                }
            }
            match plan {
                Read::Done => {
                    if cached {
                        self.used(extent);
                    }
                    return Ok(());
                }
                Read::Fetch(f) => match self.fetch(f, buf) {
                    Ok(()) => return Ok(()),
                    // the extent moved while it was fetched: plan again
                    Err(e @ StoreError::Unavailable(_)) => last = Some(e),
                    Err(e) => return Err(e),
                },
                Read::Page(pr) => match self.load_page(pr) {
                    Ok(()) => {}
                    Err(e @ StoreError::Unavailable(_)) => last = Some(e),
                    Err(e) => return Err(e),
                },
            }
        }
        Err(last.unwrap_or(StoreError::Unavailable("a read".into())))
    }

    /// `data` at `offset` within one extent, the extent pinned in the cache
    /// while a partial write reads it first.
    /// A write the store refused: counted, and answered as the kernel
    /// should hear it. A full disk (ENOSPC) is said in the log too, at most
    /// once each NO_SPACE_LOG_MS, with how many writes it has refused.
    fn write_failed(&self, err: StoreError) -> BackingError {
        self.stats.write_errors.fetch_add(1, Ordering::Relaxed);
        if let StoreError::NoSpace(why) = &err {
            let refused = self.stats.no_space_errors.fetch_add(1, Ordering::Relaxed) + 1;
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
            let last = self.stats.no_space_logged_ms.load(Ordering::Relaxed);
            if now.saturating_sub(last) >= NO_SPACE_LOG_MS && self.stats.no_space_logged_ms.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                eprintln!("{}", serde_json::json!({ "fpac": "no-space", "refused_writes": refused, "why": why, "at_ms": now }));
            }
        }
        eio(err)
    }

    pub fn write_extent(&self, extent: u64, offset: u32, data: &[u8]) -> Result<(), StoreError> {
        *self.pinned.lock().unwrap().entry(extent).or_insert(0) += 1;
        let r = self.write_extent_pinned(extent, offset, data);
        let mut p = self.pinned.lock().unwrap();
        if let Some(n) = p.get_mut(&extent) {
            *n -= 1;
            if *n == 0 {
                p.remove(&extent);
            }
        }
        r
    }

    fn write_extent_pinned(&self, extent: u64, offset: u32, data: &[u8]) -> Result<(), StoreError> {
        self.wait_for_pack(extent);
        // Bounded: a few fetches, and waits for cuts while the store is
        // full (the flusher cuts at least once a second).
        let mut fetches = 0;
        loop {
            let need = {
                let asked = Instant::now();
                let mut g = self.inner.lock().unwrap();
                let waited = u64::try_from(asked.elapsed().as_micros()).unwrap_or(u64::MAX);
                if waited > 1000 {
                    self.stats.lock_waits_over_1ms.fetch_add(1, Ordering::Relaxed);
                }
                self.stats.lock_wait_max_us.fetch_max(waited, Ordering::Relaxed);
                loop {
                    let Inner { store, cache } = &mut *g;
                    match store.write_local(cache, extent, offset, data) {
                        Err(StoreError::Full(_)) => {
                            self.stats.full_waits.fetch_add(1, Ordering::Relaxed);
                            self.want_cut(&g.store);
                            g = self.cut.wait_timeout(g, Duration::from_millis(FULL_WAIT_MS)).unwrap().0;
                        }
                        other => {
                            let need = other?;
                            self.took(&mut g, std::iter::once(extent), Taken::Used)?;
                            break need;
                        }
                    }
                }
            };
            match need {
                None => return Ok(()),
                Some(n) => {
                    fetches += 1;
                    if fetches > OP_TRIES_MAX {
                        return Err(StoreError::Unavailable("a write's read".into()));
                    }
                    let got = match n {
                        Need::Fetch(f) => self.fetch(f, &mut vec![0u8; EXTENT_BYTES as usize]),
                        Need::Page(pr) => self.load_page(pr),
                    };
                    match got {
                        Ok(()) | Err(StoreError::Unavailable(_)) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        }
    }

    pub fn punch_extent(&self, extent: u64) -> Result<(), StoreError> {
        let mut g = self.inner.lock().unwrap();
        loop {
            let Inner { store, cache } = &mut *g;
            match store.punch(cache, extent) {
                Err(StoreError::Full(_)) => {
                    self.want_cut(&g.store);
                    g = self.cut.wait_timeout(g, Duration::from_millis(FULL_WAIT_MS)).unwrap().0;
                }
                other => return other,
            }
        }
    }
}

fn eio(e: StoreError) -> BackingError {
    match e {
        StoreError::NoSpace(s) => BackingError::NoSpace(s),
        e => BackingError::Io(e.to_string()),
    }
}

/// The pieces of `[offset, offset + len)`, extent by extent: (extent,
/// offset in it, length, offset in the caller's buffer).
fn pieces(offset: u64, len: usize) -> impl Iterator<Item = (u64, u32, usize, usize)> {
    let ext = EXTENT_BYTES as u64;
    let end = offset + len as u64;
    let mut at = offset;
    std::iter::from_fn(move || {
        if at >= end {
            return None;
        }
        let e = at / ext;
        let within = at % ext;
        let n = (ext - within).min(end - at);
        let piece = (e, within as u32, n as usize, (at - offset) as usize);
        at += n;
        Some(piece)
    })
}

/// Why a map page did not load: the remote's answer, its kind kept, or
/// the store's (its page refused as malformed, or gone).
#[derive(Debug)]
pub enum PageError {
    Remote(RemoteError),
    Store(StoreError),
}

impl Backing for Disk {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), BackingError> {
        let mut extent = vec![0u8; EXTENT_BYTES as usize];
        for (e, within, n, at) in pieces(offset, buf.len()) {
            if let Err(err) = self.read_extent(e, &mut extent) {
                self.stats.read_errors.fetch_add(1, Ordering::Relaxed);
                return Err(eio(err));
            }
            buf[at..at + n].copy_from_slice(&extent[within as usize..within as usize + n]);
        }
        Ok(())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), BackingError> {
        for (e, within, n, at) in pieces(offset, data.len()) {
            if let Err(err) = self.write_extent(e, within, &data[at..at + n]) {
                return Err(self.write_failed(err));
            }
        }
        Ok(())
    }

    /// The kernel's fsync returns at once: the cache file dies with the
    /// container, so syncing it protects nothing; what protects a write is
    /// the next cut reaching the store, about a second later (the loss
    /// window, docs/contract.md). Every cut is a whole moment of the
    /// device, so the order ext4's journal relies on holds in the store.
    fn sync(&self) -> Result<(), BackingError> {
        self.stats.syncs.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn punch(&self, offset: u64, length: u64) -> Result<(), BackingError> {
        let zeros = vec![0u8; EXTENT_BYTES as usize];
        for (e, within, n, _) in pieces(offset, usize::try_from(length).expect("bounded")) {
            let r = if within == 0 && n == EXTENT_BYTES as usize { self.punch_extent(e) } else { self.write_extent(e, within, &zeros[..n]) };
            r.map_err(|err| self.write_failed(err))?;
        }
        Ok(())
    }
}

/// Runs `jobs` on at most `workers` threads, in order of start, each
/// answer in its job's place.
pub fn pool<T: Send, R: Send>(jobs: Vec<T>, workers: usize, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let n = jobs.len();
    let jobs = std::sync::Mutex::new(jobs.into_iter().enumerate().collect::<std::collections::VecDeque<_>>());
    let out = std::sync::Mutex::new((0..n).map(|_| None).collect::<Vec<Option<R>>>());
    std::thread::scope(|s| {
        for _ in 0..workers.min(n) {
            s.spawn(|| {
                // Bounded by the jobs.
                loop {
                    let Some((i, job)) = jobs.lock().unwrap().pop_front() else { break };
                    let r = f(job);
                    out.lock().unwrap()[i] = Some(r);
                }
            });
        }
    });
    out.into_inner().unwrap().into_iter().map(|r| r.expect("every job ran")).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_cache_gives_room_back_down_to_its_floor_and_takes_it_again() {
        use super::{room_bound, CACHE_DISK_HEADROOM_BYTES, CACHE_FLOOR_BYTES};
        use fpac_format::limits::{EXTENT_BYTES, UNSHIPPED_BYTES_MAX};
        let e = u64::from(EXTENT_BYTES);
        let gib = 1u64 << 30;
        let reserve = UNSHIPPED_BYTES_MAX + CACHE_DISK_HEADROOM_BYTES;
        let ceiling = 10 * gib / e;
        // at the mount, nothing cached: the mount's rule (free less the
        // shadow's worst case and the headroom), at most the ceiling
        assert_eq!(room_bound(12 * gib, 0, 0, ceiling), (12 * gib - reserve) / e);
        assert_eq!(room_bound(40 * gib, 0, 0, ceiling), ceiling);
        // a full cache with its reserve free keeps its bound
        assert_eq!(room_bound(reserve, ceiling, 0, ceiling), ceiling);
        // a writer beside it takes 3 GiB: the cache gives 3 GiB back
        assert_eq!(room_bound(reserve - 3 * gib, ceiling, 0, ceiling), ceiling - 3 * gib / e);
        // and never goes under its floor, however little is free
        assert_eq!(room_bound(0, ceiling, 0, ceiling), CACHE_FLOOR_BYTES / e);
        // a floor above a lower ceiling (`--cache-max`) is the ceiling
        assert_eq!(room_bound(0, gib / e, 0, gib / e), gib / e);
        // what the shadow holds is already off the free room
        assert_eq!(room_bound(reserve - gib, ceiling, gib, ceiling), ceiling);
        // the room back: the bound rises again, to the ceiling at most
        assert_eq!(room_bound(reserve + gib, ceiling - 3 * gib / e, 0, ceiling), ceiling - 2 * gib / e);
        assert_eq!(room_bound(u64::MAX, ceiling, 0, ceiling), ceiling);
    }

    use super::*;

    // Goal: a sealed cache file holds only ciphertext, and hands back
    // only what this life last wrote: a byte the host changed, or an
    // earlier write of the same extent put back, fails; a kept copy opens
    // as it was; a punched extent reads as zeros. Method: a cache file in
    // a scratch folder, its bytes changed under it.
    #[test]
    fn a_sealed_cache_hands_back_only_its_last_write() {
        let dir = std::env::temp_dir().join(format!("fpac-sealed-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cache");
        let sealer = Arc::new(Sealer::new(&[3u8; 32], "disk"));
        let mut c = FileCache::create(&path, 4 * EXTENT_BYTES as u64, Some((sealer, 9))).unwrap();
        let ext = EXTENT_BYTES as usize;
        let (a, b) = (vec![0xa5u8; ext], vec![0x5au8; ext]);
        c.write(1, &a).unwrap();
        let mut raw = vec![0u8; ext];
        c.file.read_exact_at(&mut raw, EXTENT_BYTES as u64).unwrap();
        assert!(raw != a && !raw.windows(16).any(|w| w == &a[..16]), "ciphertext on the host's disk");
        let first = raw.clone();
        let mut buf = vec![0u8; ext];
        c.read(1, &mut buf).unwrap();
        assert_eq!(buf, a);
        // kept apart, then written again: the copy opens as it was
        c.shadow_save(1, 5).unwrap();
        c.write(1, &b).unwrap();
        c.read(1, &mut buf).unwrap();
        assert_eq!(buf, b);
        c.shadow_take(1, 5, &mut buf).unwrap();
        assert_eq!(buf, a);
        // the host changes a byte, then hands back the first write (where
        // the extent's copy lies now)
        let at = c.key().unwrap().at(1, c.seal.as_ref().unwrap().tags.get(&1).copied());
        let mut cur = vec![0u8; ext];
        c.file.read_exact_at(&mut cur, at).unwrap();
        cur[100] ^= 1;
        c.file.write_all_at(&cur, at).unwrap();
        assert!(c.read(1, &mut buf).is_err(), "a changed byte fails");
        c.file.write_all_at(&first, at).unwrap();
        assert!(c.read(1, &mut buf).is_err(), "an earlier write fails");
        c.punch(1).unwrap();
        c.read(1, &mut buf).unwrap();
        assert!(buf.iter().all(|&x| x == 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cache file that tears its next write when told: the first `n`
    /// bytes written, the rest refused as a full copy-on-write disk does.
    struct Tearing {
        file: File,
        tear: std::sync::Mutex<Option<usize>>,
    }

    impl CacheBytes for Tearing {
        fn get(&self, buf: &mut [u8], at: u64) -> std::io::Result<()> {
            FileExt::read_exact_at(&self.file, buf, at)
        }
        fn put(&self, buf: &[u8], at: u64) -> std::io::Result<()> {
            match self.tear.lock().unwrap().take() {
                Some(n) => {
                    FileExt::write_all_at(&self.file, &buf[..n], at)?;
                    Err(std::io::Error::from(std::io::ErrorKind::StorageFull))
                }
                None => FileExt::write_all_at(&self.file, buf, at),
            }
        }
        fn punch(&self, at: u64) -> std::io::Result<()> {
            punch_hole(&self.file, at)
        }
    }

    // Goal: a sealed cache's write that fails partway (a full
    // copy-on-write disk refuses even an overwrite) leaves the extent's last
    // whole write readable, and loses nothing: the extent was acknowledged.
    // Method: a sealed cache over a file that tears the next write (half
    // of it written, the rest refused); the extent read back; the same
    // write made again, and the old copy's room given back.
    #[test]
    fn a_torn_write_leaves_the_last_whole_one_readable() {
        let dir = std::env::temp_dir().join(format!("fpac-torn-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let open = |p: &Path| OpenOptions::new().read(true).write(true).create(true).truncate(true).open(p).unwrap();
        let file = open(&dir.join("cache"));
        file.set_len(8 * EXTENT_BYTES as u64).unwrap();
        let sealer = Arc::new(Sealer::new(&[4u8; 32], "disk"));
        let mut c = FileCache::over(Tearing { file, tear: std::sync::Mutex::new(None) }, open(&dir.join("shadow")), Some((sealer, 9)), 4 * EXTENT_BYTES as u64).unwrap();
        let ext = EXTENT_BYTES as usize;
        let (a, b) = (vec![0xa5u8; ext], vec![0x5au8; ext]);
        let mut buf = vec![0u8; ext];
        c.write(1, &a).unwrap();
        let a_at = c.key().unwrap().at(1, c.seal.as_ref().unwrap().tags.get(&1).copied());
        *c.file.tear.lock().unwrap() = Some(ext / 2);
        let e = c.write(1, &b).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::StorageFull);
        c.read(1, &mut buf).expect("the last whole write still opens");
        assert_eq!(buf, a);
        c.write(1, &b).unwrap();
        c.read(1, &mut buf).unwrap();
        assert_eq!(buf, b);
        // the copy written over is punched: its side reads as zeros
        FileExt::read_exact_at(&c.file.file, &mut buf, a_at).unwrap();
        assert!(buf.iter().all(|&x| x == 0), "the old copy's room given back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Goal: a byte range splits into the extents it covers, each piece
    // inside its extent, the pieces covering the range exactly. Method:
    // ranges at and across extent boundaries.
    #[test]
    fn pieces_cover_the_range() {
        let ext = EXTENT_BYTES as u64;
        assert_eq!(pieces(0, ext as usize).collect::<Vec<_>>(), vec![(0, 0, ext as usize, 0)]);
        assert_eq!(pieces(ext - 10, 20).collect::<Vec<_>>(), vec![(0, (ext - 10) as u32, 10, 0), (1, 0, 10, 10)]);
        let p: Vec<_> = pieces(5, 3 * ext as usize).collect();
        assert_eq!(p.len(), 4);
        assert_eq!(p.iter().map(|x| x.2).sum::<usize>(), 3 * ext as usize);
    }
}
