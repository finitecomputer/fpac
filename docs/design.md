# How FPAC works: the design

contract.md says what FPAC answers; library.md what it is built on. This
document is how the two meet: the flows, the state, the limits, the
phases with their exits, the evidence, the debt and the risks. It is held
to the engineering style throughout (last section).

## The answer in brief

**FPAC is a lazy disk, not a filesystem.** `/data` stays an ordinary ext4
filesystem, made by the guest's kernel. What changes is the block device
under it. A small daemon in the guest (`fpac`) serves that device. Its
blocks live in an object store (R2) as numbered, write-once objects
("segments"). The guest keeps a cache of them on the container's own
disk. It reaches the store through an intercepted host, so it holds no
credential.

Why it is fast:

- **A wake fetches one object, beside the agent's own start.** At each
  sleep the daemon writes a **boot pack**: the blocks the last boot read,
  in the order it read them. The next wake streams that pack while the
  agent does its imports, which read only the image.
- **Every wake takes the same path,** on any host and at any disk size.
  There is no snapshot to be on the wrong host and no restore that grows
  with the disk.
- **A sleep is a flush.** The daemon ships what changed about once a
  second while awake; a sleep sends the last second.
- **Nothing is lost by more than a second or two.**

## The architecture

```
 the agent, git, npm, SQLite
        │  ordinary file calls
   ext4 (the kernel's)                          the container
        │  blocks
   /dev/loopN ── one FUSE file ── fpac ── cache file on the container's disk
                                  │  plain HTTP to fpac.fragment.internal
 ─────────────────────────────────┼──────────────────────────────────────
   the platform's route (on the container's machine; knows the disk
   and the epoch from what the platform gave it at the start)
        │  ranged get, put-if-absent                   │ at checkpoints only
        R2: disks/<disk>/e<epoch>/…              the platform's state:
        segments, checkpoints, packs             the head and kept checkpoints
```

- **The control plane and the data plane are apart.** The platform
  decides and persists: the epoch, the head, the kept checkpoints, the
  deletes. The guest and R2 carry opaque bytes. No platform state is on a
  read or a write.
- **The metadata is a block map in R2**, in checkpoint objects, kept in
  pages so a checkpoint writes only the pages that changed. A full 100 GB
  disk's map is about 24 MB before compression (64 KiB extents, 16 bytes
  each). ext4 holds the file metadata, inside the blocks.
- **One object per flush,** however many writes it holds: 10,000 small
  files written in a second are one R2 write.

## The wake

1. The platform mints the epoch and starts the container, the intercept
   registered first.
2. The agent's own start begins at once (in the product; the spike's
   harness mounts first, then starts Hermes, and measures both).
3. Beside it, `fpac mount` (crates/fpac): open (the route answers from
   what the platform gave it: no storage read), then **two rounds** to
   the store, since each round trip is about 100 ms on R2: the head's
   checkpoint; then, all at once, every map page it names, the first
   sixteen segment headers a replay may read, and the pack's head (its
   header and first sixteen extents, which hold the mount's own reads).
   Replay the head life's segments after the checkpoint (past sixteen,
   their headers come sixteen at a time), serve the FUSE file, attach the
   loop device, mount ext4. The rest of the pack streams in beside the
   mount as ranged gets on eight connections; a read of one of its
   extents waits for it rather than fetching it twice; a block not in the
   pack is one ranged get with 1 MiB of read-ahead (a **miss**). Before
   P5 the pages came one at a time: a 20 GB ext4 spreads its metadata
   over about twenty pages, and opens on Cloudflare took 2.5 to 9 s,
   growing with the data.

The pack is what the last boot read, in its order, then what the last
life wrote, newest first (its databases' pages, its logs' tails), each
extent clean and checked, up to 96 MiB.

The pass line: from the second wake on, zero misses on the path to the
agent's first reply, and a first reply no later than on the container's
own disk.

## The sleep

SIGTERM → the guest stops the agent → the daemon unmounts ext4 (or, if
`/data` is still busy after 2 s, freezes it, which makes the device
consistent with files open), detaches the loop device, cuts everything
written, ships it, writes the pack and a checkpoint naming it, posts the
head, and exits. The platform then destroys the container.

While awake: a cut about once a second when something changed, its
segments shipped eight at a time; a checkpoint after 128 segments or
five minutes. **fsync returns at once**: the cache file dies with the
container, so syncing it would protect nothing; every cut is a whole
moment of the device, so the order ext4's journal relies on holds in
the store.

## Consistency and failure

- **One writer.** A disk's epoch is the platform's start generation. The
  route writes only under that epoch, and only if absent. A container
  from an older start can only write under its old epoch, which the new
  life never reads past the point it opened at.
- **What R2 holds is always a real moment of the disk.** Each flush cuts
  the changed blocks at one instant. Segments are numbered without gaps,
  and one too big for an object counts only when its last part lands.
- **A crash mid-flush** leaves a whole segment or none (a put is atomic).
  A crash before a checkpoint costs the next open a replay of the
  segments after it, found by number, never by listing.
- **A head only moves forward.** The platform takes a head only for its
  running epoch and only with a higher sequence. The same head again is a
  replay, answered as before; the same sequence with another body is
  refused; a head from an earlier epoch is refused.
- **Order alone gives no authority.** What is current is the platform's
  stored head, never "the newest object in the bucket". A segment names
  its epoch, its sequence and the checksum of the one before it, and
  replay takes it only if all three fit. One that is present but wrong
  fails the start as unusable, nothing applied in part, and the wake falls
  back a checkpoint.
- **A put is idempotent** (contract.md): `Same` is our own earlier put;
  `Conflict` is another writer, and the daemon stops.
- **A slow or absent store.** Unsent data piles up on the container's
  disk. Past a cap (1 GiB or 60 s), writes slow down rather than go on
  unprotected.

### Each cache names its source

| Cache | Source of truth | Made stale by | A stale read |
| -- | -- | -- | -- |
| The cache file, clean extents | The segment and offset the map names | Nothing: objects never change | Can't happen; the checksum catches a lying disk |
| The cache file, dirty extents | They are the truth until shipped | – | – |
| The boot pack | The segments it was copied from | A later write to the same extent | The map no longer points there, so the entry is ignored and the read is a miss |
| The head the route was given | The platform's state | A checkpoint during the life | Used once, at open |
| The kernel's page cache | ext4 on the device | – | The kernel's own business |

## The face

- One file, `disk`, served over `/dev/fuse` by the daemon's own threads
  (`FUSE_THREADS`, each on a cloned session descriptor, each marked
  `PR_SET_IO_FLUSHER`), opened with direct I/O so FUSE keeps no second
  copy of each block.
- A loop device over it: `LOOP_CONFIGURE` with direct I/O, autoclear and
  4 KiB blocks.
- ext4 made with nothing zeroed ahead (the store reads zeros where
  nothing was written): `mke2fs -t ext4 -b 4096 -m 0 -E
  nodiscard,lazy_itable_init=1,lazy_journal_init=1,assume_storage_prezeroed=1`,
  mounted `noatime,commit=1,discard,noinit_itable`. A discard reaches
  the daemon as a hole punch, which frees the extent.

## Limits

The values are proposals until the spike's numbers set them. Each
becomes a named constant in one file, with compile-time assertions tying
them together. Those already in code are marked with their file.

| Limit | Value | Why, and what hitting it means |
| -- | -- | -- |
| `BLOCK_BYTES` | 4 KiB | The loop device's and ext4's block (`device/limits.rs`) |
| `FUSE_WRITE_BYTES_MAX` | 1 MiB | FUSE's largest request, 256 pages (`device/limits.rs`) |
| `FUSE_THREADS` | 4 | Requests in flight from the loop device (`device/limits.rs`) |
| `VOLUME_BYTES_MAX` | 1 TiB | The file's largest size; 100 GB is the goal (`device/limits.rs`) |
| `OBJECT_BYTES_MAX` | 96 MiB | Under the intercept's 100 MiB body limit, asserted (`remote/limits.rs`) |
| `IO_MS_MAX` | 30 s | One read or write on the remote's connection (`remote/limits.rs`) |
| `EXTENT_BYTES` | 64 KiB | The unit fetched, shipped and checked (`format/limits.rs`, as the rest below) |
| `SEGMENT_BYTES_TARGET`, `SEGMENT_BYTES_MAX` | 8 MiB, 64 MiB | A put costs about the same to 8 MiB; a bigger cut is a group of segments |
| `DIRTY_BYTES_MAX` | 256 MiB | Written and not yet cut. A cut takes all of it at one instant (a subset would reorder ext4's writes), copied under the store's lock; past it writes wait for the next cut |
| `CUT_SEGMENTS_MAX` | 32 | The segments one cut may need |
| `PACK_EXTENTS_MAX` | 1,535 (96 MiB less one extent) | A wake's pack; past it, the extents written longest ago are left out. With its header the object stays within `OBJECT_BYTES_MAX`, asserted for every kind of object |
| `READ_AHEAD_BYTES` | 1 MiB | A miss fetches this much of its segment (P2: it costs about what 64 KiB does) |
| `FETCH_TRIES_MAX` | 3 | A fetch whose bytes fail their checksum, tried again |
| `CHECKPOINT_SEGMENTS`, `REPLAY_SEGMENTS_MAX` | 256 | An open never replays more; the daemon checkpoints at half |
| `LIVE_SEGMENTS_MAX` | 16,384 | A 100 GB disk at 8 MiB is about 12,000 |
| `PAGE_EXTENTS` | 4,096 | A map page covers 256 MiB of the disk; a checkpoint rewrites only the pages that changed |
| `FLUSH_MS`, `CHECKPOINT_MS` | 1,000, 5 min | The loss window; the replay's other bound (`fpac/src/flush.rs`) |
| `PUTS_IN_FLIGHT` | 8 | Segments shipped at once (`fpac/src/flush.rs`) |
| The guest's writeback | 5 s expiry, 1 s wakeups | Set by the daemon at its mount: a page only written reaches the device within 5 s, not 30 |

## Benchmarks to match

Each line is against **the container's own ext4 disk on the same
machine**: that is what light speed means here. "Warm" is data already in
the cache; "cold" is after a wake on a host with an empty cache.

| Workload | Pass line |
| -- | -- |
| Mount a disk | ≤ 150 ms on Cloudflare, ≤ 50 ms on krun, beside the agent's start |
| The agent's cold boot to its first reply | within 0.1 s of the local disk |
| A miss (64 KiB ranged get) | p50 ≤ 40 ms, p99 ≤ 200 ms; zero misses on the boot's path |
| The path: first byte; a stream; an 8 MiB put | p50 ≤ 30 ms and p99 ≤ 150 ms; ≥ 80 MB/s; p50 ≤ 500 ms |
| 4 KiB write, `O_SYNC` | ≤ 1.5× local |
| `git clone`, `status`, `checkout`, `rm -rf` | ≤ 1.25× local (`status` cold ≤ 3 s) |
| `npm ci` (about 20,000 files), `rm -rf node_modules` | ≤ 1.25× local |
| SQLite: 10,000 single-row transactions (`FULL`, `NORMAL`); 1 M rows in one; 100,000 point reads | ≤ 1.25× local |
| Sequential write, 4 GiB and an fsync | ≥ 0.7× local; shipped behind it at ≥ 50 MB/s |
| 4 KiB random write, 30 s and an fsync | ≥ 0.7× local |
| A sleep's flush | ≤ 4 s at any size |

**The sniff test:** the store alone should read cached blocks at 1 GB/s
and take writes at 300 MB/s on a workstation; through the loop device it
should keep half of ext4's sequential speed. Less is accidental
complexity, to be found before any tuning.

## Dependencies

Pinned exactly in the workspace's `Cargo.toml`; each vetted when it came
in. All are sandcastle's or macrofiche's own pins.

| Crate | Version | Why | Where |
| -- | -- | -- | -- |
| libc | 0.2.189 | `/dev/fuse`, the loop device's ioctls, mount, prctl, mlockall | device, probe, testkit |
| sha2 | 0.10.9 | A put names its body's SHA-256; pure Rust, so the guest links statically | remote |
| thiserror | 2.0.20 | Typed errors at each crate's edge | device, remote |
| serde, serde_json | 1.0.229, 1.0.151 | The evidence's JSON; the driver's config | probe, bench |
| ring | 0.17.14 | The driver's ECDSA P-256 key, which signs its calls to the stand-in Worker | bench only |
| rustls, webpki-roots | 0.23.43, 1.0.9 | TLS from the driver to the Worker and to Cloudflare's API | bench only |

Refused: `fuser`, `hyper` in the guest, and `losetup`'s library; each in
library.md, "Written by hand, and why". The compressor is chosen in P3
(one small pure-Rust crate, by the CPU sketch).

The stand-in Worker pins `wrangler` 4.145.0 (`worker/package.json`) and
runs on Node 24.22.1, whose tarball's SHA-256 the driver's operator
checks.

## Phases

Each ends with its tests green, `cargo clippy --workspace --all-targets
-- -D warnings` clean, its evidence written, a commit, and a push.

| Phase | What | Exit |
| -- | -- | -- |
| P1, the design | contract.md, library.md, this document, spike.md | The open questions answered; the crates and phases named |
| P2, the probe and the path | The face (`fpac-device`), the remote's client (`fpac-remote`), the folder remote and the suite (`fpac-testkit`), the probe (`fpac-probe`), the driver (`fpac-bench`), the stand-in Worker | `probe.json` and `path.json` from Cloudflare, krun and workerd's Docker; the face chosen. **No block device on Cloudflare stops the work here** |
| P3, the store | `fpac-format` (extents, segments, checkpoints, packs; every limit a constant), `fpac-store` (the map, the cut, open and replay, the cleaner, behind three gates: the remote, the cache file, the clock), the remote's retries | Every mutation valid, invalid, replayed and restarted; a deterministic simulator that crashes at every step, all recovered |
| P4, the device and the binary | `fpac`: `mount`, `flush`, `status`, making a new volume | SQLite, git and npm on a mounted disk in a krun microVM; `crash.json` |
| P5, on krun, then on Cloudflare | spike.md's Acceptance 3 to 6, 8 and 9 | `wake.json`, `hermes.json`, `tools.json`; then a stop for the owner |

## Evidence

Written by `fpac-bench` into `docs/evidence/`, one file per scenario, each
place under its own key, with the commit it ran. spike.md's Results
quote them as they are.

## How the engineering style applies

- **Limits, named and asserted** (above): each a constant, checked where
  its input arrives; relations between them asserted at compile time.
- **Typed errors** at each crate's edge (`thiserror`): the device's
  `DeviceError`, the remote's `RemoteError` (unavailable, refused,
  protocol, corrupt), never `anyhow` outside the driver.
- **Assertions stay on in release** (`panic = "abort"`, `overflow-checks`).
  A request from `/dev/fuse` that does not decode means we read the ABI
  wrong: the daemon stops rather than serve a disk it no longer trusts.
- **No recursion; bounded loops,** each with its bound said beside it.
- **Pure cores, narrow gates:** the FUSE answers are a pure function of
  the request and a `Backing`; the store will sit behind three gates (the
  remote, the cache file, the clock), shimmed in tests.
- **Run at your own pace:** the platform never deletes in reaction to the
  guest; a delete pass plans from stored heads, for objects a day old.
- **Server state in schema:** the platform's disk state is a table with
  typed columns, not a JSON blob; the live segment list stays in the
  checkpoint object.
- **Prove the product, not a harness:** no fake disk. The face runs on a
  real kernel in a microVM or a container; ext4 is never simulated.
- **Names:** disk, extent, segment, checkpoint, pack, head, epoch; units
  last (`flush_ms`, `segment_bytes_max`).

## Debt

*Each item stays here until it is paid.*

- **The cut copies every dirty extent under the store's lock** (up to
  256 MiB, about 0.1 s): writes pause meanwhile. A copy-on-write cut
  (mark at the instant, copy outside the lock, preserving a written
  extent's old bytes for the segment) would remove the pause. Delete
  when a write pause shows in `tools.json` or in the agent's turns.
- **A wake after a crash replays ext4's journal through misses**: the
  journal's blocks are not in the pack (the crashed life wrote none).
  0.2-1.3 s on krun; on Cloudflare each miss is about 100 ms. Pay by
  writing the pack at checkpoints too, or by prefetching the journal's
  extents the superblock names.
- **Live segments are never compacted**: an extent rewritten often
  leaves old segments live while any extent in them is current. Bounded
  by LIVE_SEGMENTS_MAX (asserted); a compaction pass that rewrites a
  mostly-dead segment's live extents into a new one is the fix, before
  any disk can reach the bound.
- **The image must carry mke2fs** for a new disk (Hermes' image has
  none; the spike's harness made the disk from the tiny image).
- The probe's FUSE file is backed by a plain local file (`LocalFile`): a
  stand-in, in the probe only.
- The stand-in Worker's epoch is the driver's clock at the start, not a
  platform's start generation.
- The folder remote reads a request's head a byte at a time: a stand-in,
  not a fast path.
- The stand-in Worker carries diagnostics outside the contract (`GET
  /null`, `x-fpac-mode` and `x-fpac-diagnose` on a ranged get, the
  `/r2/` outside route). The platform's route has none of them.

## Risks

- **Cloudflare's kernel and devices.** Whether a container may attach a
  loop device is the hinge; the probe answers it. A later kernel change
  could take it away; a hosted check that mounts a disk on every deploy
  would hear at once.
- **The loop-over-FUSE path under memory pressure.** The known answers are
  in the design (direct I/O, `PR_SET_IO_FLUSHER`, `mlockall`); P5's
  pressure test decides.
- **The store's own bugs.** A lost block can damage the whole filesystem,
  not one file. The answers: a small, pure store tested by a simulator
  that crashes at every step; kept checkpoints to fall back to; the crash
  test on the real binary.
- **R2 as the one copy** (as for today's saves).
