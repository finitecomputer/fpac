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
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use fpac_device::serve::{Backing, BackingError};
use fpac_format::limits::{AHEAD_EXTENTS_MAX, AHEAD_PIECE_EXTENTS, DIRTY_EXTENTS_MAX, EXTENT_BYTES, FETCH_TRIES_MAX};
use fpac_remote::Remote;
use fpac_format::PageRef;
use fpac_store::evict::{Evictor, Taken};
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
const AHEAD_FETCHES: usize = 4;
const AHEAD_WINDOWS_MAX: usize = 4;

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
    /// The cache's bound, in extents (docs/design.md, The cache): past it,
    /// clean extents leave as `evict` chooses (S3-FIFO); never one that is
    /// unshipped or that a write is reading.
    cache_max: usize,
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
        Ok(Disk { inner: Mutex::new(Inner { store, cache }), reader, cut: Condvar::new(), pool: Mutex::new(Vec::new()), host: host.to_string(), port, stats: Stats::default(), fenced: AtomicBool::new(false), coming: Mutex::new(Default::default()), came: Condvar::new(), waited: Mutex::new(Vec::new()), cache_max, evict: Mutex::new(Evictor::new(cache_max)), streams: Mutex::new(Vec::new()), windows: AtomicU64::new(0), me: std::sync::OnceLock::new(), pinned: Mutex::new(Default::default()), wake: (Mutex::new(Wake::default()), Condvar::new()), cut_wanted: AtomicBool::new(false) })
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
        while inner.store.cached_extents() > self.cache_max {
            let store = &inner.store;
            let Some(e) = ev.victim(|x| !fresh.contains(&x) && !pinned.contains_key(&x) && store.may_leave(x)) else { break };
            if inner.store.evict(&mut inner.cache, e)? {
                self.stats.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    /// Gives the disk its own handle, once it is shared: the read-ahead's
    /// threads hold it.
    pub fn set_me(self: &Arc<Self>) {
        let _ = self.me.set(Arc::downgrade(self));
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
