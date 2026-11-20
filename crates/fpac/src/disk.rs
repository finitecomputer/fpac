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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use fpac_device::serve::{Backing, BackingError};
use fpac_format::limits::{DIRTY_EXTENTS_MAX, EXTENT_BYTES, FETCH_TRIES_MAX};
use fpac_remote::Remote;
use fpac_format::PageRef;
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

/// The cache file: one sparse file of the volume's size on the container's
/// own disk, an extent at `extent * EXTENT_BYTES`; and beside it the
/// shadow file, where a frozen extent's bytes as of its cut wait for its
/// segment's build when a write changes them first, one slot each
/// (bounded by UNSHIPPED_EXTENTS_MAX), its space freed when taken.
pub struct FileCache {
    file: File,
    shadow: File,
    slots: std::collections::HashMap<(u64, u64), u64>,
    free: Vec<u64>,
    next_slot: u64,
}

impl FileCache {
    pub fn create(path: &Path, volume_bytes: u64) -> std::io::Result<FileCache> {
        let open = |p: &Path| OpenOptions::new().read(true).write(true).create(true).truncate(true).custom_flags(libc::O_CLOEXEC).open(p);
        let file = open(path)?;
        file.set_len(volume_bytes)?;
        let shadow = open(&path.with_extension("shadow"))?;
        Ok(FileCache { file, shadow, slots: Default::default(), free: Vec::new(), next_slot: 0 })
    }

    /// A second handle on the cache file, for reads outside the lock.
    pub fn reader(&self) -> std::io::Result<File> {
        self.file.try_clone()
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

impl CacheGate for FileCache {
    fn read(&mut self, extent: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.file.read_exact_at(buf, extent * EXTENT_BYTES as u64)
    }
    fn write(&mut self, extent: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.file.write_all_at(bytes, extent * EXTENT_BYTES as u64)
    }
    fn punch(&mut self, extent: u64) -> std::io::Result<()> {
        punch_hole(&self.file, extent * EXTENT_BYTES as u64)
    }
    fn shadow_save(&mut self, extent: u64, seq: u64) -> std::io::Result<()> {
        let mut b = vec![0u8; EXTENT_BYTES as usize];
        self.file.read_exact_at(&mut b, extent * EXTENT_BYTES as u64)?;
        let slot = self.free.pop().unwrap_or_else(|| {
            self.next_slot += 1;
            self.next_slot - 1
        });
        self.shadow.write_all_at(&b, slot * EXTENT_BYTES as u64)?;
        assert!(self.slots.insert((extent, seq), slot).is_none(), "one copy per extent and segment");
        Ok(())
    }
    fn shadow_take(&mut self, extent: u64, seq: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let slot = self.slots.remove(&(extent, seq)).ok_or_else(|| std::io::Error::other("no such shadow"))?;
        self.shadow.read_exact_at(buf, slot * EXTENT_BYTES as u64)?;
        punch_hole(&self.shadow, slot * EXTENT_BYTES as u64)?;
        self.free.push(slot);
        Ok(())
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
    /// Clean extents dropped to keep the cache within `--cache-max`.
    pub evictions: AtomicU64,
    /// Writes answered with an error (the kernel sees EIO).
    pub write_errors: AtomicU64,
    /// Map pages fetched (lazy pages).
    pub page_loads: AtomicU64,
    /// Writes that waited over a millisecond for the lock, and the
    /// longest wait (µs): what a segment's build or a fetch's install
    /// costs the writes beside it.
    pub lock_waits_over_1ms: AtomicU64,
    pub lock_wait_max_us: AtomicU64,
}

/// The first extents a read waited for the pack to bring, kept for the
/// pack's event (which of them a larger pack head would have held).
const WAITED_KEPT: usize = 16;

/// What wakes the flusher: a stop, or an event (a put that ended, a
/// writer waiting on a full store).
#[derive(Default)]
pub struct Wake {
    pub stop: bool,
    pub events: u64,
}

pub struct Disk {
    pub inner: Mutex<Inner>,
    /// The cache file, read without the lock (a segment's build).
    reader: File,
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
    /// `--cache-max`, in extents: past it, clean extents leave the cache
    /// first in, first out. A measurement aid for docs/costs.md (a working
    /// set larger than the cache), not the product's policy; None keeps
    /// every extent touched.
    cache_max: Option<usize>,
    /// The extents the cache took, in the order it took them, each once.
    order: Mutex<(std::collections::VecDeque<u64>, std::collections::HashSet<u64>)>,
    /// Extents a partial write is reading first: never dropped between
    /// the fetch of their bytes and the write over them.
    pinned: Mutex<std::collections::HashMap<u64, u32>>,
    /// The flusher waits on this between turns.
    pub wake: (Mutex<Wake>, Condvar),
    /// A write waits on a full cut: the flusher cuts at once.
    pub cut_wanted: AtomicBool,
}

impl Disk {
    pub fn new(store: Store, cache: FileCache, host: &str, port: u16, cache_max_bytes: Option<u64>) -> std::io::Result<Disk> {
        let cache_max = cache_max_bytes.map(|b| (b / EXTENT_BYTES as u64) as usize);
        let reader = cache.reader()?;
        Ok(Disk { inner: Mutex::new(Inner { store, cache }), reader, cut: Condvar::new(), pool: Mutex::new(Vec::new()), host: host.to_string(), port, stats: Stats::default(), fenced: AtomicBool::new(false), coming: Mutex::new(Default::default()), came: Condvar::new(), waited: Mutex::new(Vec::new()), cache_max, order: Mutex::new(Default::default()), pinned: Mutex::new(Default::default()), wake: (Mutex::new(Wake::default()), Condvar::new()), cut_wanted: AtomicBool::new(false) })
    }

    /// Extents' bytes from the cache file, without the lock: a write
    /// meanwhile may tear one, and `Store::build_resolve` replaces each
    /// extent written since its cut with the copy kept apart before it.
    pub fn read_unlocked(&self, extents: &[u64]) -> std::io::Result<Vec<Vec<u8>>> {
        extents
            .iter()
            .map(|&e| {
                let mut b = vec![0u8; EXTENT_BYTES as usize];
                self.reader.read_exact_at(&mut b, e * EXTENT_BYTES as u64)?;
                Ok(b)
            })
            .collect()
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

    /// Notes extents the cache just took, then, past `cache_max`, drops
    /// clean ones, oldest first; never those it just took, which the
    /// caller is using (a partial write's read, say). Called with the
    /// store's lock held.
    pub(crate) fn took(&self, inner: &mut Inner, extents: impl Iterator<Item = u64>) -> Result<(), StoreError> {
        let Some(max) = self.cache_max else { return Ok(()) };
        let mut o = self.order.lock().unwrap();
        let (queue, queued) = &mut *o;
        let mut fresh = std::collections::HashSet::new();
        for e in extents {
            fresh.insert(e);
            if queued.insert(e) {
                queue.push_back(e);
            }
        }
        // Bounded: one pass over the queue at most.
        let mut tries = queue.len();
        while inner.store.cached_extents() > max && tries > 0 {
            tries -= 1;
            let Some(e) = queue.pop_front() else { break };
            if fresh.contains(&e) || self.pinned.lock().unwrap().contains_key(&e) {
                queue.push_back(e);
            } else if inner.store.evict(&mut inner.cache, e)? {
                queued.remove(&e);
                self.stats.evictions.fetch_add(1, Ordering::Relaxed);
            } else if inner.store.is_cached(e) {
                // dirty or shipping: kept, and asked again later
                queue.push_back(e);
            } else {
                queued.remove(&e);
            }
        }
        Ok(())
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
            self.stats.pack_waits.fetch_add(1, Ordering::Relaxed);
            let mut w = self.waited.lock().unwrap();
            if w.len() < WAITED_KEPT {
                w.push(at);
            }
            drop(w);
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
    /// (lazy pages: docs/design.md).
    pub fn load_page(&self, pr: PageRef) -> Result<(), StoreError> {
        let mut r = self.remote();
        let mut body = Vec::new();
        let got = r.get(&page_name(&pr), &mut |b| body.extend_from_slice(b));
        self.give_back(r);
        match got {
            Ok(Some(_)) => {
                self.stats.page_loads.fetch_add(1, Ordering::Relaxed);
                self.inner.lock().unwrap().store.install_page(pr, &body)
            }
            Ok(None) => Err(StoreError::Corrupt(format!("{} is gone, and a checkpoint names it", page_name(&pr)))),
            Err(e) => Err(StoreError::Unavailable(e.to_string())),
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
                    self.took(&mut g, f.extents())?;
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
        for _ in 0..OP_TRIES_MAX {
            let plan = {
                let mut g = self.inner.lock().unwrap();
                let Inner { store, cache } = &mut *g;
                store.read_local(cache, extent, buf)?
            };
            match plan {
                Read::Done => return Ok(()),
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
                            self.took(&mut g, std::iter::once(extent))?;
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
    BackingError(e.to_string())
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
                self.stats.write_errors.fetch_add(1, Ordering::Relaxed);
                return Err(eio(err));
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
            if within == 0 && n == EXTENT_BYTES as usize {
                self.punch_extent(e).map_err(eio)?;
            } else {
                self.write_extent(e, within, &zeros[..n]).map_err(eio)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
