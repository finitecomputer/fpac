# Results after the spike: the queued items

The spike (docs/spike.md) ended at P5. This file is the queued work after
it, item by item, and every line of design.md's "Benchmarks to match"
(section 10 of the plan) as it stands now: pass or fail, where, and from
which evidence file. Then the items added after item 7: C and D, A
(sealed disks, docs/encryption.md) and B (fuzzing, fuzz/README.md); then
6 (the cleaner).

**What ran where.** Cloudflare: the stand-in Worker `fpac-spike`'s
containers (2 vCPU, 6 GiB, 20 GB) and R2, items 1 to 5 and 7 on commit
`aa3ac0c` (deployed as version `aacc5ef1`), 1b on `4da6478` (`c0b8c13f`),
the tools line and C on `7384d39` and `d1df490` (`e4e2af69`, `7dd38af0`),
A on `55ca5a6` (`0dbdd213`), 6 on `8cca53f` (`aba127c8`); each evidence
place names its commit.
krun: a microVM per life (2 vCPU, 6 GiB, 20 GB) on this machine, the
folder remote as the store, with R2's costs added where it says "R2
modelled" (90 ms on each object call, puts sharing 50 MB/s, the rate a
container sustained to R2 in P2). **R2 modelled is not R2.**

## The lines

| Line (design.md) | Pass line | krun | Cloudflare | Holds |
| -- | -- | -- | -- | -- |
| Mount a disk | ≤ 50 ms krun, ≤ 150 ms Cloudflare | 12 ms new, 70 ms after a sleep; 226–441 ms R2 modelled | 595–1,457 ms after a sleep at 1 to 100 GB, 3.3 s at the first wake after a 100 GB fill (`wake.json`, `lives.json`) | **no** on Cloudflare and R2 modelled: the open is one round to R2 (about 300 ms) and the pack's head |
| The agent's cold boot to its first reply | within 0.1 s of its own disk | 2.87 s against 5.66 s (P5) | **8.52 s** against 8.85 s at 1 GB; 8.49 s at 10 GB; 9.41 s after an hour asleep; 9.05 s after a crash (`hermes.json`) | yes: never slower than its own disk |
| A miss (64 KiB ranged get) | p50 ≤ 40 ms, p99 ≤ 200 ms | 0.19 ms (no R2) | 97 ms p50, 130 ms p99 (`path.json`, `cloudflare-items`) | **no** at p50: R2's first byte |
| No miss on the boot's path | 0 | 0 from the second wake | Hermes: 0 in 8 of 9 wakes at 1 GB, 7 of 9 at 10 GB; the wake test: 0 from the second wake at 1, 10 and 100 GB | nearly: one or two misses in a few wakes |
| The path: first byte; a stream; an 8 MiB put | p50 ≤ 30 ms, p99 ≤ 150 ms; ≥ 80 MB/s; p50 ≤ 500 ms | – | 97/130 ms; 42–145 MB/s one stream, 94–127 MB/s four; 488 ms | first byte no; stream yes at its median; put yes |
| 4 KiB write, `O_SYNC` | ≤ 1.5× local | 0.025 against 4.39 ms p50 | 0.20 against 0.76 ms p50 (0.14 against 0.87 with nothing shipping) | yes at p50 |
| `git clone`, `status`, `checkout`, `rm -rf` | ≤ 1.25× local, `status` cold ≤ 3 s | 0.18–0.99× | 0.35–1.05×; `status` cold 0.87 s | yes |
| `npm ci`, `rm -rf node_modules` | ≤ 1.25× local | not run: the guests have no internet | not run | – |
| SQLite: 10,000 transactions (`FULL`, `NORMAL`); 1 M rows in one; 100,000 point reads | ≤ 1.25× local | 0.02–1.00× | 0.31–0.93× | yes (it failed at 1.28× before item 1) |
| Sequential write, 4 GiB and an fsync | ≥ 0.7× local; shipped behind it at ≥ 50 MB/s | 0.71–1.97× (the host's own disk varies); 43–45 MB/s, the stand-in's 50 MB/s pipe the bound | **0.45×** (322 against 714 MB/s); 85 MB/s behind | **no** on Cloudflare (0.40× before item 1's changes) |
| 4 KiB random write, 10 s and an fsync | ≥ 0.7× local | 0.66–1.87× (the host's own run varied 0.8–1.8 M IOPS) | 39.7× | yes |
| A sleep's flush | ≤ 4 s for a disk at rest (decided: after a burst, as long as shipping the backlog takes) | 0.2–1.1 s | 2.3–3.7 s at 1 to 100 GB at rest; 52 and 82 s right after a 100 GB fill | yes, as decided |
| An office save (spike.md, acceptance 6) | ≤ 1.5× local | 0.31–0.33 ms against 2.45–8.8 ms | **3.81 against 0.96 ms** p50 beside the shipping (12.4 before the shipping's priority, 4.98 before item 1); 0.65 against 0.94 ms with nothing shipping | **no** beside the shipping on Cloudflare; yes at rest |
| 100 GB: the open does not grow with the data (item 2) | flat | 191–278 ms at 1 and 10 GB of a 120 GiB volume, R2 modelled | 0.51–1.0 s at 1 and 10 GB, 0.90–0.92 s at 100 GB, one round each | nearly: the checkpoint's list of 12,800 live segments (614 KB) is the difference |
| Nothing flushed is lost (crash rounds) | every round | 70 of 70 (50 plain, 20 R2 modelled) | 10 of 10; the mount after a crash 2.4–3.4 s (6.9 s p50 before item 4's boost) | yes |

## Item by item

### 1. Unshipped writes held in the cache file

A cut names its extents; each segment is built from the cache file when
there is a put slot; up to `UNSHIPPED_BYTES_MAX` (4 GiB) written and not
yet landed, held on the container's disk (design.md, The sleep, names its
loss window and its sleep-flush cost). Then, found on Cloudflare: the
flusher read and checksummed each segment's 8 MiB under the disk's one
lock, and every FUSE read and write waited behind it (485232e moved the
reads and the encoding outside the lock; a planted bug in the new step
fails four simulator tests).

- **krun, R2 modelled:** the sequential line passes (0.71×), the office
  save passes (0.33 against 2.45 ms), 20 of 20 crash rounds.
- **Cloudflare:** the office save is the path's speed at rest (0.65
  against 0.94 ms local, the rows timed again once everything has landed)
  and slow beside the shipping: 12.4 ms while the random writes' backlog
  shipped. Running the shipping below the serving threads (nice 10, the
  lowest best-effort I/O priority; 3d071b6) took that to 3.81 ms (p99
  188 to 41 ms); the line is 1.43 ms. The sequential write stays at 0.45×:
  the guest writes 4 GiB while the daemon reads the same bytes back,
  checksums them and sends them, on two vCPUs. Writes still wait for the
  lock (about 5,300 waits over 1 ms, the longest 56 ms): a write's own
  `pwrite` to the cache file is under it. Leftovers 1 below.

### 1b. A real cache bound, and read-ahead

The cache file is bounded by its disk's room at the mount (less the
shadow file's worst case and 2 GiB); past it, clean extents leave by
S3-FIFO, so a one-pass scan cannot flush out what is used again; never
dropped are what is unshipped (parked out of the queues until its cut
lands), what a write is reading, and the next pack's boot extents.
Sequential reads are followed and kept 64 MiB ahead on eight connections
(design.md, The cache). The policy's tests fail as plain FIFO.

- **The boot's files survive a fill larger than the cache** (krun, R2
  modelled, 10 GB through a 6 GiB cache): the first wake read them in
  0.23 s with no miss, against 3.9 s and 56 misses under the old FIFO aid
  (`wake.json`, `krun-after-6gib`, `krun-before-6gib`). The same thing cost
  the 100 GB Cloudflare wake 116 misses before.
- **The thrash test** (`thrash.json`; krun, R2 modelled, a 10.9 GB build
  directory through a 3 GiB cache, as costs.md ran it):

  | | Write 10.9 GB | `sync` | Read it back | After a wake |
  | -- | -- | -- | -- | -- |
  | The container's own disk | 23–40 s | 0.4–2.6 s | 9–22 s | – |
  | Before 1b (FIFO, one miss at a time) | 296 s | 19 s | 1,137 s (9.6 MB/s) | 1,126 s |
  | 1b | 136 s | 18 s | 171 s (64 MB/s) | 146 s (75 MB/s) |

  Read-back moved from 9.6 to 64–75 MB/s, about 35–40% of the way to the
  176–194 MB/s four parallel gets reached on Cloudflare: one reader reads
  2,560 separate 4 MiB files, and each file's first read is a miss before
  its window starts (858 misses). The write is faster than before 1b
  because the 4 GiB budget takes the first part at the local disk's speed;
  the rest goes at the stand-in's 50 MB/s puts. (A first 1b version scanned
  the unshipped extents on every drop under the lock: 403 s, a 5.3 s
  lock wait; parking them fixed it.)
- **Cloudflare on 1b** (4da6478): 10 of 10 crash rounds (the mount after a
  crash 2.4–3.7 s), the lives, wakes at 1 and 10 GB (0.65–0.88 s to the
  mount after the first), and Hermes (`hermes.json`, `cloudflare-1gb-1b`, `cloudflare-10gb-1b`):
  a first reply in 8.47 s (1 GB) and 7.96 s (10 GB), the upper median of
  four wakes each, 8.81 s after a crash, one miss in nine wakes.

### 2. Lazy map pages, held at 100 GB

An open loads no map page but those the last boot touched (in its one
round) and those its replay changed. At 100 GB a disk lists about 12,800
live segments, past half of `LIVE_SEGMENTS_MAX`, and the open then loaded
every page (436, a second round of 2.0–3.4 s on Cloudflare): the periodic
checkpoint's rule. An open and a sleep now load every page only near the
limit itself (aa3ac0c; the simulator's case fetched every page before it).
On Cloudflare the 100 GB wake then opened in 0.90–0.92 s in one round.

### 3. A clean sleep marks its head

The head carries ` clean`; a clean open replays nothing and asks for no
segment header. 19 of 19 conformance cases on krun and Cloudflare. The
krun wakes opened with one ask in their round at 10 GB.

### 4. A crash pack at each checkpoint

A periodic checkpoint names a crash pack: the extents this life rewrote,
newest first (ext4's journal and metadata), up to 8 MiB; a wake after a
crash also fetches what its replay brought back. The mount after a crash
on Cloudflare: 2.4–3.4 s (p50 6.9 s before); with R2 modelled on krun
0.9–5.0 s (p50 1.4 s). Hermes after a crash: first reply 9.05 s, 6
misses (11.4 s and 55 misses before).

### 5. A new disk's first pack

A new disk's first life notes its boot's writes as the pack's. First
wakes: no miss at 1 and 10 GB. At 100 GB with a 12 GiB cache bound the
first wake missed 116 times: the bound had dropped the boot's files
before the sleep made the pack (fixed by 1b: below).

### 7. Real Hermes on Cloudflare

Fragment's Hermes image (`24b575cde4a2`) with FPAC's `/data` mounted
before its entrypoint, against fragment's fake API and a scripted model on
this machine through the stand-in Worker's relay (STAND-INS for the API
and the model, whose answers cross the internet twice; the same for both
disks). `hermes.json`:

| | FPAC | Its own disk |
| -- | -- | -- |
| First reply, 1 GB, median (range), wakes after the first | **8.52 s** (8.01–13.45), n = 9 | 8.85 s (8.35–12.36), n = 7 |
| First reply, 10 GB | 8.49 s (7.94–12.30), n = 9 | – |
| After an hour asleep, 10 GB | 9.41 s, 0 misses | – |
| Right after a crash, 10 GB | 9.05 s, 6 misses | – |
| A warm turn | 0.40 s (1 GB), 0.41 s (10 GB) | 0.47 s |
| Misses before the first reply | 0 in 8 of 9 (1 GB), 7 of 9 (10 GB) | – |
| Map pages loaded on the boot's path | 0 | – |
| The sleep, Hermes still running | 3.8–4.9 s (about 2 s of it waiting for `/data`, as on krun) | – |

**Where the first reply's time goes.** One wake of each, at 1 GB, from
the driver's start call (Hermes' own `gateway.log` and its profile's
`agent.log`, kept for this by a run of two wakes each, their timestamps
aligned to the driver's clock; both clocks keep NTP time):

| Step | FPAC (8.52 s) | Its own disk (7.85 s) |
| -- | -- | -- |
| The driver's start call reaching the Worker (from this machine) | 0.21 s | 0.21 s |
| The container's start, to its first exec | 0.63 s | 0.62 s |
| FPAC's open (one R2 round) and mount, before Hermes' entrypoint | 0.97 s | – |
| Hermes' boot: pre-init, s6, its Python preload and 67 plugins (done at) | 5.07 s | 3.89 s |
| The gateway: its socket, then running | 4.70 → 5.64 s | 3.51 → 4.97 s |
| The bridge connected to the API (the first message waits for the gateway) | 4.04 s | 2.69 s |
| The first message at the gateway | 5.70 s | 4.68 s |
| The first turn's agent init | 1.02 s | 1.23 s |
| The turn's setup | 0.40 s | 0.21 s |
| The first model call (an instant scripted model, through the relay) | 1.1 s | 1.0 s |
| The reply, ready, then at the harness | 8.48 → 8.52 s | 7.76 → 7.85 s |
| A warm turn at the harness (0.12 s inside Hermes) | 0.31 s | 0.47 s |

Against preview's 5–6 s to a first reply on a snapshot wake (Cloudflare's
start about 0.8 s, the real model about 1 s, Hermes' boot and cold first
turn about 4 s; fragment's wake study), the other ~3 s here are: FPAC's
mount in series, about 1.0 s (its own disk has none; the platform handing
the checkpoint to the open would move it beside the container's start,
leftovers below); the driver's start call from this machine, 0.2 s
(fragment's computer starts inside Cloudflare); Hermes itself, 1 to
1.5 s slower than fragment's one-second study measured its boot (1.4 s),
gateway (1 s) and first turn (2 s), its Python boot and first-turn init
being CPU work on these 2-vCPU containers; and the relay, whose first
model call takes 0.5–1.2 s to a model that answers at once (a new
connection and its first request, crossing the internet each way), and
adds about 0.2 s to a warm turn (0.31 s at the harness, 0.12 s inside
Hermes). The spike skips fragment's computer-side steps (the hold, the
restore and its gate, the platform's API beside the container) and adds
the relay, the driver's call and the mount.

The harness's 19th Hermes start never replied, twice, on its own disk:
the own-disk rows stop at seven (a harness or relay limit, not FPAC's;
not pinned down).

### C. A noisy JavaScript install (`npm.json`)

Run only in disposable Cloudflare containers of the spike's Worker: the
npm image (node 22.23.5, pinned by digest) is the only one a start may
give the internet, and only its starts had it; no secret or token was in
it; each run's disk was deleted and its container stopped after. Nothing
from the registry ran on this machine. Three variants on the same
container size (2 vCPU, 6 GiB, 20 GB): the container's own disk; FPAC with
`node_modules` shipped; FPAC with `node_modules` on the container's own
disk (a bind mount), installed again at each wake. Three runs each, of
two workloads.

Medians of three (each first run slower: its npm cache and the image
cold on that machine). "Shipped" is the egress at costs.md's $0.026 a GB
written; a sleep is right after the second install.

**The heavy workload** (crates/bench/npm-heavy: Next.js 16.4.5, React,
MUI, aws-cdk-lib, the S3 client, Firebase, ESLint, Jest, TypeScript, each
top-level version published before 2026-11-05 and the tree resolved
`--before=2026-11-05`; 61,845 files, 947 MB, 755 packages), at d1df490:

| | Its own disk | FPAC, shipped | FPAC, local-only |
| -- | -- | -- | -- |
| `npm ci`, cold | 24.7 s | 24.4 s | 25.1 s |
| `npm ci` again, the disk warm | 23.8 s | **39.1 s** (4 GiB unshipped reached; 61 waits) | 23.6 s |
| Shipped | – | 3.2 GB ($0.082) | 7 MB |
| The sleep right after (its flush) | – | **40.6 s** (40.0 s) | 4.6 s (2.5 s) |
| The next wake: the mount; installing again | –; 25.5 s | 1.0 s; none | 0.9 s; 26.1 s |
| `node` requiring all 589 loadable packages: cold; warm | 3.2 s; 3.0 s | **50.7 s** (168 misses); 2.8 s | 3.3 s; 3.0 s |

**cloudflare/sandbox-sdk** at 61cac81b (its own lockfile; 7,030 files,
418 MB), at 7384d39:

| | Its own disk | FPAC, shipped | FPAC, local-only |
| -- | -- | -- | -- |
| `npm ci`, cold | 7.5 s | 7.2 s | 7.6 s |
| `npm ci` again | 5.9 s | 7.6 s | 5.9 s |
| Shipped | – | 970 MB ($0.025) | 12 MB |
| The sleep right after (its flush) | – | 22.6 s (20.4 s) | 6.5 s (4.3 s) |
| The next wake: the mount; installing again | –; 7.8 s | 1.0 s; none | 1.6 s; 7.3 s |
| `node` requiring 148 packages: cold; warm | 1.9 s; 1.9 s | **17.8 s**; 1.9 s | 1.8 s; 1.8 s |

**What it says.** An install is no slower on FPAC: its fsyncs return at
once and its writes go to the cache file at the local disk's speed. What
costs is everything after. Shipped, a tree is a stream of new extents:
3.2 GB for two installs of the heavy one, a sleep of 40 s while it ships,
and the next wake's cold start reads tens of thousands of small files
scattered across the store, at a miss each, 50.7 s against 3.2 s; the
pack (96 MB) cannot hold it, and read-ahead does not help scattered
reads. Local-only, the tree costs a reinstall at each wake that uses it
(26 s for the heavy one, 7 s for sandbox-sdk) and nothing else: no
egress, a sleep of 4.6 s, a cold start at the local disk's speed.
**Keep `node_modules` local-only, as `target/` and `~/.cache` are**
(fragment's image work; the npm cache itself stays on the container's
disk too, so a reinstall downloads again: a shared registry cache in the
platform would take that to the copy alone).

### D. Disks bigger than the container's 20 GB

docs/large-disks.md: the options with their numbers, and a
recommendation.

### A. Sealed disks (`encryption.json`)

Built (AES-256-GCM-SIV, every object and the cache file; a checkpoint
authenticated, not sealed, as decided) and measured plain against sealed
on the same build, on krun and on Cloudflare. **Passes**: what a person
feels barely moves.

| On Cloudflare | Plain | Sealed |
| -- | --: | --: |
| The cipher, one core (seal, 64 KiB) | – | 2.2 GB/s (5.1 on this machine) |
| 4 KiB write + `O_SYNC`, p50 | 0.12 ms | 0.21 ms |
| Office save at rest, p50 | 0.54 ms | 0.73 ms |
| SQLite 10k transactions, `FULL` | 4.1 s | 6.2 s |
| 4 GiB written with fsync; shipped behind it | 318; 103 MB/s | 273; 97 MB/s |
| Daemon CPU per GB shipped (user) | 2.8 s | 4.2 s |
| A wake to the boot read done, 1 GB, p50 | 1,989 ms | 1,907 ms |
| Lives killed at random, then checked | – | 10 of 10 (20 of 20 on krun) |

Two size bugs it found are fixed with tests (a full sealed pack past the
object limit; a full sealed page past the page limit). What a TEE would
still need (rollback, sizes and patterns) is in encryption.md.

### B. Fuzzing (`fuzz/`)

Four cargo-fuzz targets, one per gate where bytes FPAC did not make come
in: `format` (objects as R2 hands them back), `fuse` (requests from
`/dev/fuse`), `http` (the remote's answers), `head` (the head's wire form
and the rules that move it). Each ran 20 minutes on this machine with no
finding, and each found its planted bug (fuzz/README.md). CI runs each for
30 seconds on every push. **Passes.**

### 6. The cleaner (`clean.json`)

Compaction, a life of the platform's own, and deletes planned from the
stored head and the checkpoints kept before it (design.md, The cleaner).
On Cloudflare, a disk with 512 MiB of files three quarters rewritten in
place (1.2 GB stored):

| | |
| -- | -- |
| A cleaner killed 30 s into its pass (its first head taken) | the next life: every file, SQLite, git and `e2fsck -f -n` checked |
| A pass to its end | 2,056 extents (128 MiB) moved out of 69 segments; live segments 126 → 73; 65 s |
| The delete pass, two lives later | 98 objects (742 MB) deleted, planned in 2 s; the store 1,407 → 666 MB |
| The life after the deletes | every file, SQLite, git and `e2fsck -f -n` checked |

The simulator stops a cleaner at every step and opens the disk after
each; a delete plan that keeps too little fails its test (a planted one
did). **Passes**, slowly: the pass reads one extent at a time.

The store builds for `wasm32-unknown-unknown`, the platform's Worker's
target, so compaction can run there, beside R2, with no container egress.
On this spike it runs as `fpac clean` in a small container.

## Leftovers, as PRs (FPAC's repo unless named)

1. **The office save and the sequential write beside the shipping, on
   Cloudflare** (item 1's lines): profile on Cloudflare first; then
   4 KiB-granular dirty tracking (a journal block's write stops being a
   64 KiB read-modify-write), or the cache file with direct I/O, or one
   hash per shipped byte instead of two. One or two PRs.
2. **A first miss of 4 MiB** for file-by-file scans (read-back 64–75 MB/s
   toward 176–194). One PR.
3. **A sleep's pack beside its last segments**, a put round fewer. One PR.
4. **The live segments' list out of the checkpoint** (read by the cleaner
   only), so a 100 GB open fetches 60 KB, not 614 KB. One PR.
5. **Pin the agent's home**, and **tell the agent its working set against
   the cache** (large-disks.md). Two PRs, and one in fragment's skill.
6. **The mount beside the container's start** (≤ 150 ms): the platform
   hands the checkpoint and the pack's head to the open. Fragment's route
   (plan PR list).
7. **The harness's 19th Hermes start that never replied** (outside this
   repository).
8. **Compaction reads each segment whole** (one get, its live extents
   taken from it), not one extent at a time: 65 s for 128 MiB now. One PR.
9. **The cleaner in the platform's Worker**: the remote gate over the R2
   binding, compaction as a Durable Object's alarm after a sleep, the
   delete pass from the stored head. Fragment's cell, two PRs.
10. **A sealed disk's compaction**, by a life that holds its key (its
    pages are sealed). One PR.

## Gaps

- The npm runs measure one container per run; Cloudflare's machines vary
  (a first start of the npm image took 10.9 s, its image cold there).
- The krun numbers with R2 modelled are a stand-in's; its put pipe caps
  shipping at 50 MB/s, so "shipped behind ≥ 50 MB/s" cannot pass there.
- This machine's own-disk numbers on krun vary run to run (the random
  writes 0.8–1.8 M IOPS), so krun ratios against them are loose; the
  absolute FPAC times hold.
- Hermes' first reply crosses the relay twice per call (STAND-INS for the
  API and the model); fragment's real API and model would differ.

## Decided (the owner, 2026-11-20)

- **The sleep line holds for a disk at rest.** The 4 GiB unshipped budget
  stays; right after a burst a sleep may take as long as shipping 4 GiB
  takes (52 and 82 s after a 100 GB fill). A wake cancels a sleep, so a
  person who comes back never waits on it.
- **A sealed checkpoint's list of the objects it names is in the clear,**
  authenticated by its seal but not encrypted, so the platform's cleaner
  can plan from it (A, item 6).
