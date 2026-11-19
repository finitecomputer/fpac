# The first spike: FPAC working on Cloudflare

This is a **spike**: it proves or disproves one design on real hardware
and records the evidence. It is written to the engineering style
(design.md, "How the engineering style applies"), so what it proves can
become the product without a rewrite. It merges nowhere until the
project's owner decides.

The design is design.md's; what the remote answers is contract.md's;
why it is built and not adopted is library.md's.

## Problem

A long-lived computer's files must outlast its sleeps. Today, on
Cloudflare Containers, a wake restores the person's `/data` before the
agent may read it, and a sleep copies all of it. Both grow with the disk.
Measured on fragment's preview deployment on 2026-11-18: 138–145 s to wake
after an hour asleep at 5–10 GB, and 36–110 s to save a sleep at 5–10 GB.

The claim to prove: **a lazy disk under ext4, its blocks in R2, wakes in
the same time at any size and adds nothing in series to the agent's own
start.**

## Acceptance

The spike is done when Results holds each of these, measured (ten
samples where cheap: median and range), from the evidence file the
driver (`fpac-bench`) wrote:

1. **The probe** (`docs/evidence/probe.json`). The kernel's version;
   `/dev/fuse`; a loop device over a FUSE file with direct I/O; ublk;
   NBD; an ext4 mount. In three places: a Cloudflare container at a
   computer's size (2 vCPU, 6 GiB, 20 GB), a krun microVM, and the
   Docker container local workerd starts.
2. **The path** (`docs/evidence/path.json`). Through the intercept to R2:
   a ranged get's first byte and MB/s; puts of 64 KiB and 8 MiB; a 50 MiB
   stream; put-if-absent's three cases.
3. **A wake that doesn't grow** (`docs/evidence/wake.json`). Mounted
   within 150 ms. Wake to a file read back, on a fresh container each
   time, at 1, 5 and 10 GB, and again after an hour asleep: the same at
   every size, within 0.3 s.
4. **Nothing flushed is lost** (`docs/evidence/crash.json`). Fifty rounds:
   the container killed mid-write, then a wake, `e2fsck -f` clean,
   SQLite's `integrity_check` ok, `git fsck` ok, and every write a flush
   acknowledged present.
5. **The agent** (`docs/evidence/hermes.json`). Ten wakes on FPAC against
   ten on the container's own disk: a first reply within 0.1 s of it; no
   misses before it from the second wake on; SQLite's `journal_mode`
   still WAL; the filesystem the agent sees is `ext4`; the syncs per boot
   and per turn counted.
6. **Tools** (`docs/evidence/tools.json`). design.md's benchmark rows for
   git, npm and SQLite, against the container's own disk.
7. **Conformance** (in `path.json`). The remote's suite passes against the
   stand-in Worker and against the test kit's remote.
8. **The build is clean.** `cargo clippy --workspace --all-targets -- -D
   warnings` and `cargo test --workspace`, on Linux and on macOS.
9. **Nothing else moved.** The account's Workers, buckets and container
   applications listed before and after: only the spike's own differ,
   and `reset` leaves none.

## Constraints

### Musts

- **Separate everything.** Its own Worker, bucket and container
  application, each named in a local config outside this repository
  and carrying the project's prefix, on a development account, from a
  config with no DNS token. No account id, resource name, host name,
  token or key is committed: `fpac-bench` reads them from
  `$FPAC_SPIKE_CONFIG` (crates/bench/src/config.rs).
- **A budget.** One Cloudflare container at a time (one Durable Object
  holds it); at most 20 GB in the bucket; bulk test data deleted once
  its numbers are written.
- **The engineering style, from the first commit.** Pinned and audited
  dependencies, each named with why (design.md, Dependencies).
- **One reset.** `fpac-bench reset` is the only cleanup. It deletes, by
  the names in the local config: the bucket's objects, the Worker, its
  container application, its registry images, the bucket, and the local
  work directory.
- **Every privileged action is listed here, and there are no others:**
  - in a guest (a krun microVM or a Cloudflare container), as root:
    `mount` and `umount2` (FUSE, then ext4), `mknod` of `/dev/fuse`,
    `/dev/loop-control` and `/dev/loopN` when the kernel has the driver
    and `/dev` lacks the node, the loop device's ioctls, `mke2fs`,
    `e2fsck`, `PR_SET_IO_FLUSHER`, `mlockall`, and writing
    `/proc/sys/vm/drop_caches` (the sniff test);
  - on the driver's machine: the krun engine's socket, as fragment's
    e2e uses it; `docker build`, `docker save`, and `docker run` with
    nothing added (no `--privileged`, capability or device);
  - on Cloudflare: deploying and deleting the Worker, making and deleting
    its bucket and the bucket's objects, deleting its container
    application and registry images.
- **Host safety.** No loop device, ublk, NBD or ext4 mount is ever made
  on a kernel the driver's machine runs itself. Those run only inside
  krun microVMs and Cloudflare containers. A container that shares the
  host's kernel (the Docker place) runs the probe check-only.

### Must-nots

- Touch any other Worker, bucket, object, container application or
  config on the account; a production deployment; a personal account.
- Put a credential in the guest. It speaks plain HTTP to the
  intercepted host and to nothing else.
- Depend on fragment's crates from FPAC's.
- Report a stand-in's number as the product's.

### Preferences

- A loop device over a FUSE file first, since FUSE is the surface
  Cloudflare shows. ublk if both kernels have it.
- `/dev/fuse` by hand over a crate; a pure-Rust compressor; a static
  musl binary.
- Linux-only code behind `cfg(target_os = "linux")`, so the workspace
  builds and tests on macOS.
- Debug on krun first (a start takes 0.4 s and costs nothing), then run
  on Cloudflare.

### Escalations (stop and ask the owner)

- No block device on Cloudflare. That result goes to Cloudflare, and the
  fallbacks of library.md are the owner's to choose between.
- The face needs a privilege or a kernel feature the container doesn't
  have by default.
- The path can't carry a 50 MiB pack in about a second, or a put takes
  seconds.
- Copyleft code would have to be linked in.
- The agent behaves differently on the disk: WAL off, or another
  filesystem type seen.
- Anything that needs another Worker, an account setting (a bigger
  container disk), a secret, or more than the budget.

## Layout

| Crate | What | Tested on the host |
| -- | -- | -- |
| `fpac-device` (`crates/device`) | The face: one file served over `/dev/fuse` by hand, a loop device over it with direct I/O, ext4 made and mounted (Linux only behind `cfg`) | The FUSE protocol and every answer, pure, with a backing in memory |
| `fpac-remote` (`crates/remote`) | The gate to the remote: `open`, ranged `get`, `put` (put-if-absent with SHA-256), over plain HTTP/1.1 written by hand | The HTTP exchange against canned answers; names; checksums |
| `fpac-testkit` (`crates/testkit`) | The folder remote (a stand-in, named so), its faults, and the conformance suite | The suite against the folder remote; a lost answer; a 503 |
| `fpac-probe` (`crates/probe`) | The guest's half of the driver: `probe`, `path`, `conformance`, `idle`; one static binary | – (it runs in guests) |
| `fpac-bench` (`crates/bench`) | The evidence driver: each place, each scenario, its JSON, `deploy`, `inventory`, `reset` | none: it is the driver |
| `worker/` | The stand-in Worker (JavaScript: workers-rs has no Containers) | Against Cloudflare, by the driver |
| `images/tiny` | The tiny image: Debian slim, e2fsprogs, git, sqlite3, node, `fpac-probe` | – |

Still to come, in phases P3 and P4 (design.md, Phases): `fpac-format`,
`fpac-store` and the `fpac` binary.

## Phases

Each ends with its tests green, clippy clean, its evidence written, and
a commit pushed.

| Phase | What | Exit |
| -- | -- | -- |
| P1, the design | contract.md, library.md, design.md, this document | The open questions answered; the crates and phases named |
| P2, the probe and the path | Acceptance 1, 2 and 7 | `probe.json` and `path.json` from Cloudflare, krun and workerd's Docker; the face chosen. **No block device on Cloudflare stops the work here** |
| P3, the store | `fpac-format`, `fpac-store`, the remote's retries | Every mutation valid, invalid, replayed and restarted; the simulator's crashes all recovered |
| P4, the device and the binary | `fpac`: mount, flush, status | SQLite, git and npm on a mounted disk in a krun microVM; `crash.json` |
| P5, on krun, then on Cloudflare | Acceptance 3 to 6, 8 and 9 | `wake.json`, `hermes.json`, `tools.json`. Then it stops for the owner, with the note for Cloudflare |

## Evaluation

- Each acceptance line is filled from the driver's JSON, with medians,
  ranges, versions and commits, and goes into Results as it is.
- **Honesty.** Every stand-in is named in the evidence it produced: the
  Worker stands in for a platform's route; the folder remote stands in
  for the route and for R2 on krun; the probe's FUSE file is backed by a
  plain local file, not the store. No stand-in's number is reported as
  the product's.
- Deviations from the plan are listed in Results with their reasons.

## Results

Measured 2026-11-19. The driver's machine: a 32-thread AMD workstation,
Linux 7.2, Docker 29.9, sandcastle's engine with libkrun's kernel 6.12.115.
Every number below is quoted from `docs/evidence/` at commit `8f27c5c`
unless it says otherwise; the probe's timings are one sample per place
(acceptance 3 times the mount ten times, in P5).

### Deviations from the plan, and why

1. **The face's FUSE half and the remote's client came forward into P2.**
   The probe needs a FUSE file to put a loop device on, and the path is
   measured through the product's own client, not a harness.
2. **A fifth crate, `fpac-probe`,** the guest's half of the driver, apart
   from `fpac-bench`, so the guest's binary is static, small (about 1 MB)
   and carries no TLS or signing code.
3. **The device is exercised only in krun microVMs and Cloudflare
   containers,** never in a privileged container on the driver's machine
   (the plan said "a privileged container"): nothing attaches a loop
   device or mounts ext4 on a kernel the driver's machine runs itself.
4. **The driver's calls to the Worker are signed** by a local ECDSA P-256
   key whose public half is a plain var: no secret is set on the account.
5. **`ranges.json`, a byte-checked ranged-read scenario,** added after a
   wrong read was seen (below), with a diagnostic mode in the stand-in.
6. **Two buckets:** one where R2 placed it (WNAM) and one with an eastern
   hint (ENAM), to see how far the path travels.
7. **The Worker updates its Durable Object at once
   (`code_update_strategy: immediate`), and the driver waits for the
   deployed version before it measures:** one run measured the previous
   version for a few minutes after a deploy.
8. **The stream is 50 MiB** (52.4 MB), not 50 MB.
9. **The stand-in's `/null` route** answers 64 KiB from memory, outside
   the contract, to time the intercept with no store behind it.

### P2, acceptance 1: the probe (`probe.json`)

| | Cloudflare container | krun microVM | Docker, as local workerd starts it |
| -- | -- | -- | -- |
| Kernel | **6.18.64-cloudflare-microvm-2026.11.44** | 6.12.115 (libkrunfw) | the host's, 7.2.15 |
| CPUs, memory | 2, 6.25 GiB | 2, 5.8 GiB | the host's |
| The root filesystem | btrfs, 20 GB | overlay | overlay |
| `CAP_SYS_ADMIN`, `CAP_MKNOD`, `CAP_SYS_RESOURCE` | held | held | `CAP_MKNOD` only |
| `CAP_IPC_LOCK` | not held (`mlockall` of a small process works within the 8 MiB `RLIMIT_MEMLOCK`) | held | not held |
| `/dev/fuse` | present, opens | present, opens | **absent** |
| `/dev/loop-control`, loop in `/proc/devices` | present | present | **absent** |
| ublk (`/dev/ublk-control`), NBD | absent | absent | absent |
| `PR_SET_IO_FLUSHER` | allowed | allowed | refused |
| One file over `/dev/fuse` (4 threads) | **works**: mounted in 2.0 ms | works: 0.3 ms | not tried (check-only) |
| A loop device over it, direct I/O, 4 KiB blocks | **works, direct I/O taken**: 8.1 ms | works, direct I/O taken: 0.1 ms | – |
| `mke2fs` (2 GiB, nothing zeroed) | 183 ms | 1,610 ms (an earlier run: 45 ms; the machine was loaded) | – |
| ext4 mount | **16.9 ms** | 7.7 ms | – |
| statfs reads ext4; a file survives unmount and mount; `e2fsck -f -n` clean | yes | yes | – |
| 256 MiB written and synced, then read cold: the container's own disk | 160 / 437 MB/s | 88 / 1,965 MB/s | – |
| The same through ext4 on the loop device on the FUSE file (a local file behind it, a stand-in for the store) | **119 / 411 MB/s (74% / 94%)** | 340 / 2,956 MB/s | – |

An earlier run on Cloudflare, same code path: own disk 173 / 668 MB/s,
through the face 144 / 541 MB/s (83% / 81%); mounts 2.0, 3.5, 92 and
16.4 ms.

**The face chosen: a loop device over one FUSE file with direct I/O,**
on both Cloudflare and krun. Neither kernel has ublk or NBD. Local
workerd's Docker container offers no face: it has no `/dev/fuse`, no
loop device and no `CAP_SYS_ADMIN`, and workerd adds nothing to a
computer's container (read from a live one: not privileged, no added
capability or device; only its network sidecar has `NET_ADMIN`). So the
disk's end-to-end lanes run on krun and on Cloudflare, never on local
workerd: the blocker named, as the style asks.

### P2, acceptance 2: the path (`path.json`)

The guest in a Cloudflare container, plain HTTP to the intercepted host,
the stand-in Worker's handler (on the container's machine), an R2
binding. The intercept registered before the container's start, which
Cloudflare allowed. The Durable Object and its container ran in DFW.

| | Pass line | WNAM bucket | ENAM bucket |
| -- | -- | -- | -- |
| A 64 KiB ranged get, first byte, p50 / p99 (n = 200) | ≤ 30 / ≤ 150 ms | **111 / 202 ms** | **106 / 172 ms** |
| of which R2's own call (`server-timing`) | – | 98 / 188 ms | 96 / 163 ms |
| The intercept alone (64 KiB from the handler's memory), p50 / p99 | – | **7.4 / 20 ms** | 6.6 / 10 ms |
| A 1 MiB ranged get, whole, p50 / p99 (n = 100) | – | 151 / 420 ms | 140 / 344 ms |
| A 50 MiB stream, one connection (5 runs) | ≥ 80 MB/s | **25–50 MB/s** (1.0–2.1 s) | 21–49 MB/s |
| The same as 4 ranged gets in parallel (3 runs) | – | 55–57 MB/s (0.9–1.0 s) | 33–110 MB/s (0.5–1.6 s) |
| A 64 KiB put, p50 / p99 (n = 30) | – | 330 / 438 ms | 579 / 1,006 ms |
| An 8 MiB put, p50 / p99 (n = 10) | ≤ 500 ms | **709 / 825 ms** (12 MB/s) | 931 / 1,960 ms |
| A 50 MiB put | – | 2.75 s | 2.97 s |
| Put-if-absent: new, the same again, other bytes | 201, 200, 409 | Created, Same, Conflict | Created, Same, Conflict |

Earlier runs of the same path (WNAM, before the commit) varied: four
parallel ranged gets reached 113–150 MB/s in one run, a single stream
81 MB/s at best.

**Every network pass line is missed, and R2 is the cause:** the
intercept costs about 5–7 ms; R2's own calls cost about 100 ms for a
ranged get and 300–900 ms for a put, wherever the bucket was placed.
The plan's escalation ("the path can't carry a 50 MB pack in about a
second, or a put takes seconds") is at its edge: a 50 MiB pack takes 0.9
to 2.1 s on one stream and 0.5 to 1.6 s on four.

The krun place's path (`path.json`, `krun`) runs against the folder
remote on a unix socket: a stand-in for the route and for R2, never the
product's number. Through the engine's intercept it measured a 64 KiB put
in 6.5 ms and a 50 MiB stream at about 1.16 GB/s.

### P2, acceptance 7: conformance (`path.json`)

12 of 12 cases pass against the stand-in Worker (both buckets), against
the folder remote from inside a krun guest, and in this workspace's
tests.

### Wrong bytes from a ranged read (`ranges.json`)

**A ranged read on Cloudflare can come back the right length with the
wrong bytes.** Seen twice in about 13,600 byte-checked ranged reads
through the stand-in's streamed path:

- `ranges.json`, `cloudflare-wnam` (commit `8f27c5c`): 1 wrong in 2,000.
  A 64 KiB read at offset 13,463,552 was right for its first 8 KiB; from
  there on it held the same object's bytes from 28 KiB further on (offset
  13,500,416). It was read 1,421st, on one kept-alive connection, mid-run.
- A path run before the commit (its file replaced by a later run, so its
  detail was not kept): a 1 MiB read at offset 30,408,704, the right
  length, wrong bytes.
- No wrong read in 3,000 more streamed reads (`cloudflare-wnam-streamed`),
  nor in the 600 of the final path runs, nor in 4,500 reads where the
  handler read each range whole before sending it
  (`cloudflare-wnam-diagnose`, `-diagnose-2`). At the rate seen, 4,500
  reads would show none about half the time, so where the bytes go wrong
  (R2's body stream, the handler's response, or the intercept) is **not
  yet known**.
- Every whole-object get (checked by its stored SHA-256) was right.

The store checks every extent against its checksum after every read and
fetches a wrong one again (design.md); this finding makes that a
requirement with evidence behind it, and it goes to Cloudflare.

**A second finding, not about bytes: right after a deploy, the egress
handler ran the previous version.** In `cloudflare-wnam-diagnose`, the
first 25 reads (of 3,000) were answered without the diagnostic header the
new version adds, though the driver had waited until the Durable Object
itself answered with the new version (the 20 the file records are
indices 0 to 19 of the run's fixed sequence of reads). A platform that changes the route's
contract must expect a container's first calls after a deploy to reach
the previous code.

### What P2 changes in the design (proposals; the owner decides)

- **A miss costs about 100 ms, not 30–50.** The boot pack and the
  background fill carry more weight, and read-ahead on a miss should be
  wider: a 1 MiB range costs little more than 64 KiB (first byte 118 ms
  against 111).
- **Puts cost 300–900 ms each, nearly flat to 8 MiB.** The flush ships
  segments several at a time, and a segment should be as large as the
  second's writes allow; a one-second cadence holds for an agent's turn
  (a few MB), and a burst like `npm ci` ships behind the work in
  parallel.
- **The pack is fetched as parallel ranged gets,** not one stream.
- **Every extent is checked against its checksum after every read**
  (already the design), and a wrong one is fetched again: a wrong read
  was seen.
- **`mlockall` on Cloudflare** needs the daemon to raise its own
  `RLIMIT_MEMLOCK` first (it holds `CAP_SYS_RESOURCE`, not
  `CAP_IPC_LOCK`).

### P3: the store (2026-11-19, commit `340ac36`)

The owner's word after P2: go on, with the three changes proposed above
(the pack as parallel ranged gets, read-ahead of 1 MiB, several segments
shipped at once), and a checksum on every extent with a fetch again on a
mismatch as a hard requirement, tested in the simulator.

- **`fpac-format`**: segments, map pages, checkpoints and packs; every
  object sealed with the SHA-256 of its header; every extent with its
  sum64. Six tests, among them every cut length and every flipped byte of
  each header refused as a value.
- **`fpac-store`**: open and replay, reads with read-ahead and checks,
  writes, the cut, landing in any order, checkpoints of changed pages,
  the pack, fencing, the head's rules (design.md). Behind two gates and
  a clock passed in.
- **The simulator** (`crates/store/tests/sim.rs`), release build:

  | Scenario | What it showed |
  | -- | -- |
  | A calm remote, 60 seeds × 12 lives × 150 operations, crashing anywhere | every reopen is the last fully landed flush; no acknowledged flush lost |
  | A stormy remote: down 3%, put answers lost 3%, **one ranged get in ten the right length with the wrong bytes** | 17,269 reads, **none wrong**, 0 failed after their tries; 25,641 wrong answers served; 467 crashes recovered |
  | A crash at every step of one run, with multi-segment flushes in flight and landing in any order | **2,751 of 2,751** crash points recovered |
  | A remote that always lies about one segment | the read fails after 3 tries; never bytes |
  | A cache that lies | caught by the extent's checksum, fetched again |
  | The pack | after a sleep, the next wake's 20 boot reads make no remote call; a stale pack entry is ignored |
  | Another writer's bytes at a name | the life is fenced |
  | The head's rules | each case: new disk, new life, later checkpoint, the same again, older, conflicting, not running |

- **A bug the simulator found** while it was being hardened (it first
  passed too easily: three of five planted bugs went unnoticed, so it
  grew pipelined flushes, multi-segment cuts and random landing order):
  a flush of several segments moved the map a segment at a time, so a
  checkpoint taken between them held half a flush. The test failed
  (seed 13, life 7: wrong bytes at reopen) before the fix and passes
  after it: the map now moves a whole cut at a time.
- **Planted bugs, each caught:** no extent checksum (3 tests fail);
  cuts ignored on replay (3); landing a segment at a time (3); no
  fetch again on a wrong extent (1); an unchecked cache (1).
- **Deviations:** checkpoints and packs are numbered within their life
  (`c<n>`, `p<n>`), not by the segment they cover, so two checkpoints of
  one life can never share a name; what a checkpoint covers is its
  `end`. A chain link is the SHA-256 of a segment's header (which seals
  its extents' sums), so a replay checks the chain from headers alone.
  The cleaner's compaction of live segments is left to P5.

### P3: hunting the wrong bytes (`rangecheck.json`, `rangecheck-outside.json`)

The owner asked for the two wrong reads of P2 to be pinned down: hashes
at each hop, our own reader checked first, and each variant run at
100,000 reads or more.

- **Our reader first.** A test (`bodies_survive_any_read_sizes`, in
  `crates/remote/src/http.rs`) serves 6,000 answers of up to 1.5 MiB, by
  length or chunked, on kept-alive connections, and delivers them in
  pieces of random size (1 byte, 7 bytes, 8 KiB, 300 KiB at most); every
  body arrives exactly. Bytes left after an answer were already refused
  (an assertion); they are now a typed error that drops the connection.
  Nothing in the reader explains a read right for 8 KiB and shifted after.
- **Hashes at each hop.** The stand-in's handler has three modes: `stream`
  passes R2's body through; `tee` hashes the bytes as R2 yields them and
  as it sends them; `buffer` reads the range whole, hashes it, then sends
  it. The guest checks every read against the object's known pattern. A
  wrong read records its key, range, `content-range`, length, the first
  offset that differs, where the wrong bytes came from in the object, and
  the handler's own record (colo, `cf-ray`, version, time). None was
  recorded: no read was wrong.
- **Outside, with no container:** this machine signs each call to the
  Worker's fetch handler, which reads R2 the same way.

| Variant (16 threads, one 50 MiB object, DFW) | Reads | Wrong | First byte p50 / p99 |
| -- | -- | -- | -- |
| Through the intercept, `stream`, aligned ranges to 1 MiB | 100,000 | **0** | 97 / 195 ms |
| The same, `tee` | 100,000 | **0** | 94 / 179 ms |
| The same, `buffer` (`arrayBuffer`) | 100,000 | **0** | 88 / 174 ms |
| The same, `stream`, unaligned offsets and sizes | 100,000 | **0** | 80 / 145 ms |
| Outside, `stream`, aligned to 64 KiB | 100,000 | **0** | 110 / 213 ms |
| Outside, `tee` | 100,000 | **0** | 108 / 201 ms |
| Outside, `stream`, unaligned | 100,000 | **0** | 115 / 217 ms |
| The product's client (`fpac_remote::Remote`, kept-alive, a thread each), 1 MiB reads at 64 KiB offsets, a whole checked get every 200 | 100,000 (512 whole) | **0** | – |
| A bucket made minutes before (the two wrong reads came within half an hour of their buckets' making): through the intercept, `stream` | 100,000 | **0** | 32 / 107 ms |
| The same bucket, outside | 100,000 | **0** | 51 / 145 ms |
| The same bucket, the product's client, a whole get every 100 | 50,000 (506 whole) | **0** | – |

**1,050,000 byte-checked ranged reads and 1,018 whole gets, none wrong.**
At P2's rate (2 in about 13,600) this many would have shown about 150.
So the wrong reads are **not reproduced**, and there is no repro and no
report for Cloudflare. What changed between them and now: the stand-in
(its diagnostic modes) and the driver; the guest's reader only in the
way above. A platform condition that has since passed is as likely as
anything of ours, and the evidence can't tell them apart. The store keeps
the checksum on every extent and fetches a wrong one again (P3's
simulator), so either way a wrong read never reaches ext4.

A run of the product's client with 4 threads and a whole get every 20
reads stopped at 15,000 reads (none wrong) when its container went away
mid-run; the driver's poll then hung on the dead container and was
stopped by hand, so that run wrote no file. The new bucket's first byte
(32 ms p50 against 97) is one sample of one bucket.
