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
10. **P5's samples are fewer than ten:** three wakes per size and place
    (two from the second wake on), ten crash rounds on Cloudflare rather
    than fifty (fifty on krun, three times), one run of the tools per
    place. Each Cloudflare wake at 10 GB costs a 10 GB fill.
11. **Hermes ran on krun only:** on Cloudflare it needs fragment's fake
    API behind the intercept, which the stand-in Worker does not carry.
12. **`npm ci` was not run:** the guests have no internet.
13. **The folder stand-in can delay each object call**
    (`FPAC_FOLDER_OBJECT_MS`), to count a wake's round trips on krun; every
    result it shaped says so.
14. **A wake's time is given two ways:** the daemon's own (opened,
    mounted, the boot's files read, from its start) and the driver's
    (from asking for a container to the files read), since on Cloudflare
    the driver's own calls cross the internet several times a step.

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

### P4: the daemon, on krun (commits `ac0a939`, `2baa60d`)

`fpac mount` (crates/fpac) is the disk in the guest: it opens the disk
from the remote, serves one FUSE file on four threads, puts a loop
device with direct I/O over it and ext4 on that (making ext4 on a new
disk), cuts about once a second and ships eight segments at once,
checkpoints after 128 segments or five minutes, and on SIGTERM unmounts,
makes its last flush, the pack and a checkpoint, posts the head and
exits. It writes its state each second to `/run/fpac/status.json`.

What krun taught it, each now in design.md:

- **fsync returns at once and is counted** (Q4's answer): the cache file
  dies with the container, so syncing it protects nothing.
- **The guest's writeback is set to 5 s** at the mount (Linux's default
  is 30 s): a page only written reaches the device, and so the store,
  within 5 s.
- **A sleep freezes `/data` when it stays busy** past 2 s of unmounting
  (Hermes keeps files open): `FIFREEZE` makes the device consistent with
  files open.
- **`mlockall` with `MCL_ONFAULT`**: locking every page at once cost about
  100 ms of the mount; locking as pages are touched costs nothing.
- **The pack carries the life's newest written extents** after the
  boot's reads, and **reads of pack extents wait for them** rather than
  fetching them twice: Hermes' first wakes had 13 to 50 misses before
  this, 0 to 2 after.
- **A crash test needs its programs to sync:** git does not fsync by
  default, so a crash test's git repository lost objects until it set
  `core.fsync=all`, as on any disk. Not a bug of the disk.

### P5: the wakes, on krun and on Cloudflare (commit `079dfc2`)

Every number here is from `docs/evidence/` written at `a6d8f4b` (the
code of `079dfc2` and its evidence), unless it says otherwise.

**A bug, found by the 1 GB wake on krun** (`e60f6bc`): a full pack was
1,536 extents (96 MiB) plus its 24,632-byte header, which the route
refused (413), so a sleep after a large write shipped no pack. The
simulator's remote now refuses what the route refuses, and
`a_full_pack_fits_one_object` failed first with the same byte count. The
pack now holds 1,535 extents, and every kind of object is asserted
against the contract's `OBJECT_BYTES_MAX` at compile time.

**The open took seconds on Cloudflare, and grew with the data.** The
first wakes there (`795074e`, kept in `wake.json` as
`*-pages-one-at-a-time`) opened in 2.5 to 12.8 s. The open fetched the
checkpoint's map pages one at a time, about 100 ms each on R2, and a map
page holds every mapped extent of its 256 MiB, so their count grows with
the data: 21 pages at 1 GB, 36 at 5 GB, 51 at 10 GB. The open now reads
in **two rounds**: the checkpoint, then at once its pages, the replay's
first 16 segment headers and the pack's head (its header and first 64
extents, which hold an ext4 mount's own reads). The simulator checks it
(`an_open_reads_in_two_rounds`). To see the rounds on krun, the folder
stand-in can delay each object call by R2's 90 ms
(`*-object-90ms` in `wake.json`, named as such).

#### Acceptance 3: a wake that doesn't grow (`wake.json`)

From the second wake on (two wakes per size; the first wake after the
fill is the disk's first wake and is below). Opened, mounted and the
boot's files read are the daemon's own times from its start; wake to
files read is the driver's, from asking for a container to the files
read, and on Cloudflare it carries the driver's own calls over the
internet (several per step).

| Place | Size | Map pages | Opened | Mounted | Boot's files read | Wake to files read (driver) |
| -- | -- | -- | -- | -- | -- | -- |
| krun, folder stand-in | 1 GB | 21 | 17–19 ms | **86–87 ms** | 62–64 ms | 315–316 ms |
| | 5 GB | 36 | 31–34 ms | **96–104 ms** | 62–64 ms | 312–315 ms |
| | 10 GB | 51 | 63–79 ms | **178–198 ms** | 110–123 ms | 500–520 ms |
| krun, each object call 90 ms | 1 GB | 22 | 211–212 ms | **252–254 ms** | 119–122 ms | 522 ms (and 1,563 with a slow engine start) |
| | 10 GB | 49 | 320–321 ms | **370–375 ms** | 148–180 ms | 863–878 ms |
| **Cloudflare**, stand-in Worker and R2 | 1 GB | 22 | 566–1,149 ms | **652–1,240 ms** | 247–334 ms | **1.66–2.54 s** |
| | 5 GB | 31 | 667–909 ms | **761–1,013 ms** | 448–569 ms | **1.92–2.09 s** |
| | 10 GB | 50 | 690–895 ms | **789–989 ms** | 251–486 ms | **1.97–2.00 s** |
| Cloudflare before the round (`795074e`) | 1 GB | – | 2.5–3.8 s | 3.3–4.6 s | 0.27–0.60 s | 4.6–5.6 s |
| | 5 GB | – | 3.9–5.1 s | 4.5–5.8 s | 0.25–2.0 s | 6.8–7.4 s |
| | 10 GB | – | 6.3–8.2 s | 7.5–8.9 s | 0.21–0.25 s | 8.9–9.9 s |

- **On Cloudflare the wake no longer grows from 1 to 10 GB** (driver's
  wake to files read 1.66–2.54 s at every size, against 4.6–9.9 s
  before), with no miss on the boot's path. **It does not meet the
  plan's lines:** the mount takes 0.65–1.24 s, not 150 ms, and two
  samples per size spread wider than 0.3 s.
- **Where the Cloudflare mount's time goes:** the round of 39 to 67
  parallel gets takes 395–888 ms there, against 104–197 ms on krun with
  90 ms per call; so it is R2's tail under that many parallel calls (or
  the intercept's, for that many new connections), not the count of
  round trips. Fewer calls in the round is the fix (below).
- **The map still loads whole at the open:** pages grow with the data,
  so the round grows too (krun with the delay: 211 ms at 1 GB, 321 ms at
  10 GB; 67 asks at 10 GB is two waves of 64 connections). At 100 GB it
  would be about 400 pages. **Lazy map pages** remove it: the open loads
  the checkpoint only, a page comes on the first read of one of its
  extents, and the pack's own sums stand for its extents' pages.
- **The first wake of a disk** misses 52 to 73 times on its boot's path
  (10.6 s to read the boot's files on Cloudflare; 4.1–4.2 s on krun with
  the delay): its first life
  made those files rather than read them, so its pack holds the fill's
  newest extents. From the second wake on the pack holds what the boot
  read.
- **After an hour asleep** (`cloudflare-10gb-after-an-hour`, 3,600 s
  with no container): the 10 GB disk's wake took what it took right away.
  The run sleeps before the disk's first wake, so the like-for-like
  compare is with the first wake of `cloudflare-10gb`: opened 941 ms
  against 1,006, mounted 2,064 ms against 2,182, 75 misses on the boot's
  path against 70 (the first-wake effect below), the driver's wake to
  files read 12.7 s against 13.4 s. The two wakes right after it: mounted
  in 855–985 ms, no miss. Today's restore takes 138–145 s after an hour
  at 5–10 GB. An hour asleep before a disk's *second* wake was not run.
- **Sleeps** after a wake: 2.1–3.0 s on Cloudflare (the pack, the
  checkpoint and the head are three puts of 0.3 to 1 s each), 0.2–0.7 s
  on krun. After a fill, the first sleep ships what the fill wrote
  faster than R2 took it: 9.3–11.1 s on Cloudflare. The fill itself ran
  at 48–51 MB/s on Cloudflare and 84 MB/s on krun, bounded by shipping
  (below, the tools).

#### Acceptance 4: nothing flushed is lost (`crash.json`)

| Place | Rounds | Failed | Opened after a crash, p50 (range) | Mounted, p50 (range) |
| -- | -- | -- | -- | -- |
| krun | 50 (and 50 at `ac0a939`, 50 at `795074e`) | **0** | 28 ms (15–53) | 565 ms (215–1,334) |
| Cloudflare | 10 (and 10 at `795074e`) | **0** | 874 ms (264–1,402) | 7.8 s (2.5–11.8) |

Each round: a writer commits SQLite rows (acknowledged only after a
sync), git commits and files; the container is killed at a random
moment; the next wake runs `e2fsck -f -n`, SQLite's `integrity_check`,
`git fsck`, and checks every acknowledged row is present. A wake after a
crash replays 8 to 39 segments on Cloudflare (9 to 88 on krun), and ext4
replays its journal; on
Cloudflare that recovery's reads are misses (a crash leaves no pack of
them), hence the seconds. Ten rounds on Cloudflare, not fifty: each costs
a container start and about 25 s.

#### Acceptance 5: the agent (`hermes.json`)

The real Hermes image (fragment's at `bc00639ba`) in krun microVMs, its
fake fragment API and instant scripted model behind the engine's
intercepts (stand-ins), `/data` mounted by `fpac` before the image's own
entrypoint, the folder stand-in behind it. Ten wakes each; numbers after
the first wake. FPAC on the final daemon (`079dfc2`); the container's
own disk from the earlier run (`2baa60d`), whose FPAC side measured a
first reply of 4,441 ms (3,413–5,472).

| | FPAC | The container's own disk |
| -- | -- | -- |
| First reply, median (range), n = 9 | **2,866 ms** (2,543–5,220) | 5,662 ms (2,930–7,455) |
| A warm turn | 97 ms (95–98) | 869 ms (352–1,749) |
| Misses before the first reply | 0 in seven wakes, 1 and 2 in two | – |
| fsyncs per boot; per warm turn | 178 (166–185); 66 (0–80) | – |
| `journal_mode` | WAL, all six databases | WAL |
| The file system Hermes sees | ext4 on `/dev/loop0` | ext4 on `/dev/vdd` |
| Sleep | 2.36–2.46 s (2 s waiting for `/data` to be let go, then a freeze) | – |

The first reply is no later than on the container's own disk, and earlier
by a median 2.8 s; part of that is FPAC's fsync returning at once (178
syncs a boot) where the engine's disk syncs for real. "No misses from the
second wake on" holds in seven of nine wakes. Hermes was run on krun only:
on Cloudflare it needs the fake API behind the intercept, which the
stand-in Worker does not carry.

#### Acceptance 6: tools (`tools.json`)

Both disks in one guest: FPAC's `/data` and a folder on the container's
own disk. Times in ms; the ratio is FPAC's over the own disk's, and the
pass line is section 10's (design.md).

| Row | Cloudflare FPAC / own | ratio | krun FPAC / own | ratio | Line | Holds |
| -- | -- | -- | -- | -- | -- | -- |
| `git clone`, 7,260 files | 30,664 / 31,121 | 0.99 | 1,170 / 1,633 | 0.72 | ≤ 1.25× | yes |
| `git status` warm | 97 / 120 | 0.81 | 29 / 37 | 0.78 | ≤ 1.25× | yes |
| `git status` cold | 1,071 / 260 | 4.1 | 686 / 300 | 2.3 | ≤ 3 s | yes |
| `git checkout`, 7,260 files change | 28,530 / 28,928 | 0.99 | 160 / 181 | 0.88 | ≤ 1.25× | yes |
| `rm -rf` the checkout | 163 / 414 | 0.39 | 60 / 68 | 0.88 | ≤ 1.25× | yes |
| SQLite, 10k transactions, `FULL` | 4,118 / 15,584 | 0.26 | 1,416 / 23,974 | 0.06 | ≤ 1.25× | yes (fsync returns at once) |
| SQLite, the same, `NORMAL` | 265 / 373 | 0.71 | 48 / 86 | 0.56 | ≤ 1.25× | yes |
| SQLite, 1 M rows, one transaction | 1,439 / 1,546 | 0.93 | 594 / 1,534 | 0.39 | ≤ 1.25× | yes |
| SQLite, 100k point reads, warm | 27 / 30 | 0.90 | 12 / 13 | 0.92 | ≤ 1.25× | yes |
| `grep -rl`, cold; warm | 1,486 / 406; 63 / 61 | 3.7; 1.03 | 701 / 304; 15 / 14 | 2.3; 1.07 | ≤ 20 s; ≤ 1.25× | yes |
| `find`, cold | 82 / 46 | 1.8 | 33 / 10 | 3.3 | ≤ 22 s | yes |
| A 30,000-entry directory, cold; warm | 13 / 53; 11.5 / 12.8 | 0.24; 0.90 | 4.3 / 19.8; 3.7 / 4.1 | 0.22; 0.90 | ≤ 2 s; ≤ 1.25× | yes |
| Small files: create, write, close (each) | 0.032 / 0.113 | 0.28 | 0.010 / 0.039 | 0.26 | ≤ 1.25× | yes |
| `stat` warm (each) | 0.003 / 0.003 | 1.0 | 0.000 / 0.001 | – | ≤ 1.25× | yes |
| A scratch file, p50 | 0.094 / 0.112 | 0.84 | 0.028 / 0.056 | 0.50 | ≤ 1.25× | yes |
| 4 KiB `O_SYNC` write, p50 | 0.26 / 1.21 | 0.21 | 0.08 / 3.99 | 0.02 | ≤ 1.5× | yes |
| An office save (temp file, fsync, rename), p50 | **10.4 / 1.2** | **8.4** | 0.31 / 1.13 | 0.28 | ≤ 1.5× | **no on Cloudflare** |
| Random 4 KiB writes, 10 s (IOPS) | 40,437 / 15,968 | 2.5× | 807,210 / 1,112,361 | 0.73× | ≥ 0.7× | yes |
| Sequential write of 1 GiB and an fsync (MB/s) | **46.9 / 464.4** | **0.10×** | **67.9 / 162.0** | **0.42×** | ≥ 0.7×, shipped ≥ 50 MB/s | **no** |
| Sequential read, cold (MB/s; the cache file, not R2) | 855 / 1,799 | 0.48× | 1,113 / 7,776 | 0.14× | ≥ 80 MB/s | yes |
| `npm ci` | not run: the guests have no internet | | | | | – |

**What fails, and why: writes wait for the store behind a burst.** A cut
takes every dirty extent, and past `DIRTY_BYTES_MAX` (256 MiB) a write
waits for the next cut (`full_waits`: 423 on Cloudflare, 259 on krun in
this run). So a write larger than 256 MiB goes at the shipping rate, 47
to 51 MB/s to R2, and small syncs behind it wait too: the office save's
10.4 ms p50 and a 4 KiB `O_SYNC` write's worst case of 5.4 s (1.4 s on
krun) are those waits. The fix is a dirty limit set by the cache file
rather than by memory (the cut copies dirty extents under the store's
lock today), with shipping behind it.

#### Acceptance 7: conformance (`path.json`)

17 of 17 cases pass against the stand-in Worker from inside a Cloudflare
container (`cloudflare-p5`), against the folder remote from inside a
krun guest, and in this workspace's tests. On the same run the path
measured a 64 KiB ranged get at 86 ms p50 (187 p99) to first byte, an
8 MiB put at 557 ms p50, and 50 MiB as four parallel ranged gets in
0.27–0.30 s (176–194 MB/s; P2 measured 55).

#### Acceptance 8: the build is clean

`cargo clippy --workspace --all-targets -- -D warnings` clean and `cargo
test --workspace` passing at `079dfc2`: on Linux (41 tests; Rust 1.99)
and on macOS (arm64, 40 tests: one is Linux-only; Rust 1.98). The
simulator's release run: 11 passed, 1 ignored (a debugging aid that
replays one seed).

#### What P5 changes in the design (proposals; the owner decides)

1. **Lazy map pages.** The open loads the checkpoint alone; a map page
   is fetched on the first read of one of its extents; the pack's header
   (sealed by the checkpoint) stands for its own extents, and replayed
   segments override both. Then the open no longer grows with the data
   (51 pages at 10 GB, about 400 at 100 GB), and the round drops from 39
   to 67 calls to about 17.
2. **The platform hands the guest its checkpoint and the pack's head at
   `/open`.** The route reads them while the container boots (a start
   takes 0.2–0.5 s), so the mount waits for no round trip to R2 after
   `/open`. The plan's 150 ms mount needs this: two R2 round trips are
   already 0.2 s at best, and their tail on Cloudflare is 0.4–0.9 s.
3. **A clean sleep says so in its head,** so the next open asks for no
   segment headers (16 of the round's calls today, all absent after a
   clean sleep).
4. **The dirty limit backed by the cache file, not memory.** A cut
   copies dirty extents under the store's lock, so the limit is 256 MiB
   and a larger write goes at R2's shipping rate (47–51 MB/s), with
   small syncs waiting behind it. Cutting by reference to the cache file
   (copy-on-write of an extent written again while it ships) lets writes
   go at the local disk's speed with shipping behind them.
5. **A pack at each checkpoint, not only at a sleep,** holding the
   journal's region: a wake after a crash spends 2.5–11.8 s on
   Cloudflare reading ext4's journal recovery as misses.
6. **A new disk's first life records its mount's reads** (extents it
   wrote and then read), so its first wake has a pack of the boot's
   path rather than of the newest writes.

#### Acceptance 9: nothing else moved

The account's Workers, R2 buckets and container applications were
listed before the spike's first deploy, after P5, after `reset`, and
after the last deploy (`fpac-bench inventory`; the lists stay on the
driver's machine, outside this repository, since they name the
account's other resources).

| | Workers | Buckets | Container applications |
| -- | -- | -- | -- |
| Before the first deploy | 3, none the spike's | 3, none the spike's | 2, none the spike's |
| After P5 | the same 3, and the spike's 1 | the same 3, and the spike's 3 (two of them a second location and a fresh bucket for the wrong-bytes hunt) | the same 2, and the spike's 1 |
| After `reset` | **identical to before** | **identical to before** | **identical to before** |

`reset` deleted the spike's objects (916 left by stopped runs; every
finished run had deleted its own), its Worker, its container
application, its 15 registry images and its three buckets. The Worker
was then deployed again, to stay (the brief's ask), with an empty bucket.
