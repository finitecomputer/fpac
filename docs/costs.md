# What FPAC costs, and what falls over

Three questions, answered on 2026-11-19 from the spike's own Worker,
container and bucket on Cloudflare, and from krun. Prices are Cloudflare's
list prices on the Workers Paid plan, read that day from its developer
docs. Every number says whether it was **measured** or **estimated**.

## The answers

1. **Intercepted traffic is billed as container egress, in one direction
   only.** What the container sends through the intercept (FPAC's puts)
   is counted byte for byte as the container's billed `txBytes`, at
   $0.025/GB past the account's 1 TB a month. What it receives (FPAC's
   reads) is not counted at all. The intercept's calls are not Worker
   requests and use no Worker CPU. R2 bills its operations, and egress
   from R2 is free. (Measured, from Cloudflare's own analytics; the
   invoice was not read.)
2. **A person's disk costs about $0.02 a GB-month to keep, and about
   $0.026 per GB written.** Reads cost almost nothing. At 20 GB that is
   $0.45 to $10 a month; at 500 GB, $10 to $20. Light and typical users
   are mostly storage; only heavy writers (a Rust build a day) make
   movement matter. The units to meter are **GB-month stored** and **GB
   written**.
3. **A working set bigger than the cache makes a large build slow, but
   nothing breaks.** On krun, with R2's costs modelled, a 10.9 GB
   build-like target directory wrote 7–13× slower than the local disk:
   writes wait for shipping once 256 MiB is unshipped. With a 3 GiB cache,
   or after any wake, reading it back took 19 minutes against 12–15 s
   locally, because every byte came from the store again. Today FPAC has
   no cache bound at all, so a volume larger than the container's disk
   would fail; one is needed before disks pass 20 GB. The fix that makes
   builds "feel like just a computer" is keeping build and cache paths
   on local scratch, never shipped.

## 1. Is intercepted traffic billed?

### What the docs say

From the Containers, Workers, R2 and Durable Objects pricing pages and
the Containers outbound-traffic page, read on 2026-11-19:

- **Containers** bill memory, CPU and disk while a container runs, and
  egress per GB: $0.025 in North America and Europe (1 TB a month
  included), $0.04 or $0.05 elsewhere (500 GB included). "Charges stop
  after the container instance goes to sleep." The docs **do not say
  what counts as egress**, and the outbound-traffic page, which documents
  intercepts and bindings, says nothing about billing either way.
- **Workers** bill requests ($0.30 a million past 10 million) and CPU
  ($0.02 a million ms past 30 million). "There are no additional charges
  for data transfer (egress) or throughput (bandwidth)." Subrequests are
  not billed. The docs are silent on whether a container's call that a
  Worker answers counts as a request.
- **R2** Standard: $0.015 a GB-month; Class A (PutObject) $4.50 a million;
  Class B (GetObject, ranged or not) $0.36 a million; DeleteObject free;
  egress free.
- **Durable Objects**: $0.15 a million requests, duration $12.50 a
  million GB-s while active, SQLite storage $0.20 a GB-month.
- The largest container disk is 20 GB (`standard-4`).

So the docs leave the question open. We measured it.

### What Cloudflare recorded (`docs/evidence/billing.json`)

Cloudflare's GraphQL Analytics API, filtered to the spike's own container
application, Worker and buckets, over windows when the spike alone moved
known amounts:

| Window (UTC) | What the spike moved | Container's billed `txBytes` (usage dataset) | Container `rxBytes` (metrics dataset) | R2 | Worker invocations |
| -- | -- | -- | -- | -- | -- |
| 21:47–21:50, a 10 GB fill | puts of 11.02 GB in all (21:46:30–21:50) | **9.85 GB** | 0.01 GB | 1,231 PutObject, 10.18 GB uploaded | 49: the driver's own calls, **not the 1,231 puts** |
| 20:33–20:35, 50,000 checked reads | ranged gets of 1 MiB | **0.06 GB** (the requests' heads) | **27.97 GB** | 36,599 GetObject, 26.52 GB downloaded | 11: the driver's polls, **not the 36,599 reads** |
| The whole day, 16:00–24:00 | every spike run | **58.3 GB** | 301.2 GB | 60.6 GB uploaded, 292.7 GB downloaded; 9,323 PutObject, 1,101,039 GetObject | 404,713: the outside route's 400,000 (no container) and the driver's; none of the container's 700,000 gets |

- **Bytes the container sends through the intercept are billed egress.**
  The usage dataset, whose fields are the price list's (memory, CPU, disk,
  `txBytes`), counted about one byte for each byte put to R2: 58.3 GB
  against 60.6 GB uploaded over the day, and the fill's minutes match
  R2's upload minute by minute.
- **Bytes it receives are not.** The usage dataset has no field for
  them; the metrics dataset saw 301 GB arrive, and billed `txBytes`
  stayed at the requests' heads while it did.
- **The intercept's calls are not Worker requests.** 1.1 million
  intercepted calls left no trace in the Worker's invocations, while the
  outside route's 400,000 calls to the same Worker were all counted.
- **Not verified:** the invoice. The spike's token gets 403 from the
  billing API, and no new token was asked for. The account's billing page
  for 2026-11-19 should show about 58 GB of container egress for this
  application, inside the 1 TB included.

**What follows for FPAC:** every byte it ships costs $0.025/GB once an
account passes 1 TB a month, plus R2's $4.50 a million puts; every byte it
reads costs only R2's $0.36 a million gets. Moving the cleaner (the
compaction of half-empty segments) into the platform's Worker keeps its
rewrites off container egress, since a Worker's traffic to R2 is free.

## 2. What storage and movement cost a person

### Inputs

| Input | Value | Source |
| -- | -- | -- |
| Stored per GB live, fresh | 1.03× | **Measured**: a 10 GB fill stored 11.02 GB of objects for 10.74 GB written |
| Stored per GB live, steady | 1.3× | **Estimated**: a cleaner holding garbage to 30% (not built; without it, rewrites pile up) |
| A wake and its sleep | 23 MB shipped, 10 puts, 80 gets | **Measured** on Hermes (krun): boot 5–7.8 MB, pack 14 MB, last flush and checkpoint about 2 MB |
| A warm turn | 2 MB shipped, 1 put | **Measured** on Hermes: 1.9–4.0 MB |
| A GB written in bulk | 1.03 GB shipped, about 124 puts | **Measured** shipping (8 MiB segments) |
| A GB read cold | about 950 gets, no egress | **Measured**: a miss fetches 1 MiB |
| Shipped bytes billed as egress | 1.0× | **Measured** (section 1) |
| The Durable Object's part | a head post per wake and checkpoint | Under $0.001 a month at any mix |

The usage mixes are **estimated**: light is one wake a day and ten turns
a wake; typical is five wakes a day, twenty turns a wake, 20 GB a month
written in bulk (installs, clones) and 20 GB read cold; heavy is twenty
wakes a day, fifty turns a wake, 300 GB written and read a month (a large
Rust build every day).

### Per agent per month, list prices, no free tier

Movement is the same at any disk size. Shipped = wakes × 23 MB + turns ×
2 MB + bulk GB × 1.03; puts = wakes × 10 + turns + bulk GB × 124; gets =
wakes × 80 + cold GB × 954; then egress at $0.025/GB, puts at $4.50 and
gets at $0.36 a million:

| Mix | Shipped | Puts | Gets | Egress | R2 Class A | R2 Class B | Movement |
| -- | -- | -- | -- | -- | -- | -- | -- |
| Light | 2.3 GB | 723 | 3,354 | $0.06 | $0.003 | $0.001 | **$0.06** |
| Typical | 30.1 GB | 6,964 | 31,073 | $0.75 | $0.03 | $0.01 | **$0.79** |
| Heavy | 383 GB | 72,963 | 334,102 | $9.58 | $0.33 | $0.12 | **$10.02** |

Storage, and the totals:

| Live data | Stored (1.3×) | Storage | Total, light | Total, typical | Total, heavy |
| -- | -- | -- | -- | -- | -- |
| 20 GB | 26 GB | $0.39 | **$0.45** | **$1.18** | **$10.41** |
| 100 GB | 130 GB | $1.95 | **$2.01** | **$2.74** | **$11.97** |
| 500 GB | 650 GB | $9.75 | **$9.81** | **$10.54** | **$19.77** |

- **What scales with use:** egress and puts scale with bytes written;
  gets with cold reads and wakes; storage with what the person keeps. A
  wake costs about $0.0006 (its pack and boot writes). Nothing scales with
  time asleep except storage.
- **What it replaces:** today's sleep copies all of `/data` (36–110 s of
  container time at 5–10 GB, and the whole disk written each time). A
  FPAC sleep ships only what changed, in 2–3 s.
- **The container's own time** (memory, CPU, disk while it runs) is the
  same with or without FPAC, except that the sleep is shorter.

### What to meter

1. **GB-month stored**: the bytes under the person's disk in R2,
   sampled daily. Cost floor $0.0195 a GB-month (R2's $0.015 with 30%
   slack). This is the line that makes "basically unlimited storage" a
   price rather than a promise: a TB kept is $19.50 a month.
2. **GB written**: the bytes the daemon ships (its `bytes_shipped`).
   Cost floor about $0.026 a GB (egress $0.025 and puts). Reads need no
   meter: they cost $0.00034 a GB.

The account's 1 TB of included egress and R2's free tier are per account,
so at fleet scale they round to nothing per person.

## 3. A working set bigger than the cache (`docs/evidence/thrash.json`)

**What FPAC does today:** it keeps every extent a life touches in its
cache file on the container's own disk, and never drops one. A 20 GB
volume therefore fits a 20 GB container disk only while nothing else
uses that disk. A larger volume (the goal is 100 GB or more) would fill
the container's disk and fail writes with I/O errors. So a cache bound is
needed before volumes grow past 20 GB. To measure what a bound costs, the
daemon gained `--cache-max <bytes>`, a measurement aid: past the bound it
drops clean extents first in, first out, and never one that is unshipped
or that a partial write is reading.

**The job:** a large Rust build's target directory, modelled as 2,560
files of 4 MiB and 10,000 files of 16 KiB (10.9 GB): written, synced, the
page cache dropped, every file read back by one reader. It ran on the
container's own disk, on FPAC with the cache unbounded, and on FPAC with
a 3 GiB cache (28% of the job), then after a sleep and a wake.

**Where:** krun (2 vCPU, 6 GiB, a 20 GB disk). Behind it is the folder
stand-in with R2's costs added: 90 ms on each object call, and puts
sharing 50 MB/s. On Cloudflare a 1 MiB ranged get took 86–94 ms, and a
container sustained 47–51 MB/s of puts. **The R2 costs are modelled, not
R2 itself.**

| | Own disk | FPAC, cache unbounded | FPAC, cache 3 GiB | FPAC after a wake |
| -- | -- | -- | -- | -- |
| Write 10.9 GB | 23.4–40.3 s | **298 s** (7.4×) | **296 s** (12.6×) | – |
| `sync` | 1.6–2.6 s | 20.2 s | 18.8 s | – |
| Writes that waited for the store | – | 5,908 | 5,826 | – |
| Read every file back | 12.0–14.9 s | **15.6 s** (1.05×), no miss | **1,137 s** (95×), 17,500 misses | **1,126 s**, 18,010 misses |
| Fetched from the store to read 10.9 GB | – | 0 | up to 18.4 GB | up to 18.9 GB |
| A sleep right after | – | 3.6 s | 4.3 s | 1.0 s |
| Errors (FUSE, ext4) | – | none | none | none |

The two own-disk columns are the same job in each run's guest. Run A
measured 40.3, 1.6 and 14.9 s; run B measured 23.4, 2.6 and 12.0 s.

- **Wall time against local.** Writing a big target directory is **7 to
  13 times slower**. Once 256 MiB is unshipped, every write waits for
  shipping, so the job wrote at about 36 MB/s against 270–465 MB/s
  locally. Reading back costs nothing extra while the cache holds the
  data. **Once it has been dropped, or after any wake, read-back is 95
  times slower**, at 9.6 MB/s, one 1 MiB miss at a time.
- **Writes stall, yes.** In both runs the unshipped budget sat at its
  limit of 4,096 extents (256 MiB) through the whole write phase: 5,826 to
  5,908 waits. Cloudflare's tools run showed the same thing (`tools.json`):
  423 waits, an office save at 10.4 ms against 1.2 ms local, and a 4 KiB
  `O_SYNC` write that waited up to 5.4 s.
- **The miss rate on read-back.** With the cache unbounded there were
  none. With 3 GiB, every byte came from the store again, and some of it
  more than once: 17,500 misses of at most 1 MiB each, up to 18.4 GB, to
  read 10.9 GB. Why the extra is there was not pinned down. The likely
  causes are overlapping fetches by the four FUSE threads, and the bound
  dropping read-ahead or ext4's metadata before it is used. A few misses
  (7) even came during the write phase: ext4's own metadata, pushed out
  by the build's writes.
- **What a sleep costs at that point.** 3.6–4.3 s right after the job:
  the `sync` had shipped the backlog, and most of the sleep is the 100 MB
  pack. A sleep in the middle of a burst first ships up to the 256 MiB
  budget, about 7 s more at 50 MB/s (estimated). A wake after it mounted
  in 0.9 s, but the build's outputs were cold: reading them took 19
  minutes.
- **Found while measuring:** a bound that drops an extent between a
  partial write's fetch of it and the write over it makes the write fail.
  The write retries, gives up with an I/O error, ext4 aborts its journal,
  and the files vanish from the guest's view; the data in the store stays
  right. The aid now pins such extents, and 279,548 drops ran without an
  error. Whoever builds the product's eviction must keep that rule;
  `only_clean_extents_leave_the_cache` tests the store's half of it.
### Mitigations, as plan items (none built)

Each keeps "it feels like just a computer" in view: the agent should not
need to know there is a store behind `/data`.

| Plan item | What it fixes | What it costs | Size |
| -- | -- | -- | -- |
| **Build and cache paths stay local and are never shipped**: a scratch filesystem on the container's own disk for `target/`, `node_modules/.cache`, `~/.cache` and `/tmp`, set by the image (`CARGO_TARGET_DIR`, `npm_config_cache`, `XDG_CACHE_HOME`) | Builds write at the local disk's speed; nothing to ship, so no stall, no egress, no stored bytes, no thrash | The outputs are gone after each sleep: the first build after a wake is a full one (a large Rust build: minutes). Scratch shares the 20 GB container disk with the cache | 1 PR in the images; a line in the agent's guide |
| **A larger unshipped budget, held in the cache file** (the cut by reference, P5's proposal 4), say 2–4 GiB instead of 256 MiB | A burst of a few GB goes at local speed; shipping catches up behind it | A longer loss window on a crash (up to the budget ÷ 50 MB/s: 40–80 s of writes); a sleep must ship the backlog first (up to 40–80 s, as long as today's save); more cache disk held | 1 PR in the store, with simulator work |
| **A real cache bound with scan-resistant eviction** (2Q or CLOCK-Pro: a once-read scan does not push out what is used again), and never evicting what is unshipped or mid-write | A working set larger than the container's disk degrades instead of failing with ENOSPC; a build's one-pass reads don't evict the agent's own hot files | Re-reads of evicted data are misses at R2's speed | 1 PR in the daemon (this spike's `--cache-max` is a FIFO measurement aid only) |
| **Sequential read-ahead with parallel gets**: detect a sequential scan and fetch 8–16 MiB ahead on 4 connections | Cold read-back from about 4–11 MB/s (one 1 MiB miss at a time) toward the 176–194 MB/s four parallel gets reached in P5 | Bytes fetched and not used when a scan stops early (Class B gets only; reads are not billed egress) | 1 PR in the daemon |
| **Compression of segments** (a pure-Rust LZ4 or zstd) | Build outputs compress 2–4×: that much less egress, storage and shipping time | CPU on 2 vCPUs while shipping; the format gains a codec | 1 PR in the format and store |
| **The platform waits for the flush before a sleep, and says so** | A sleep mid-build ships its backlog rather than losing it | The sleep is as long as the backlog takes | Part of fragment's PR 8 (a sleep is a signal and a wait) |
| **A notice to the agent** when the unshipped backlog or cache pressure is high ("the disk is busy saving") | The agent can choose scratch paths or wait | Breaks "just a computer" a little; a last resort | 1 small PR, after the others |
| **A larger container disk** | More room for the cache | Not available: Cloudflare's largest instance disk is 20 GB (`standard-4`); it is an account setting to ask Cloudflare for | – |

**Recommended:** first, **build and cache paths on local scratch**. That
is the only item that makes a large build feel like a computer's, and it
costs one image PR plus a full rebuild after a wake. Then the **larger
unshipped budget**, so ordinary bursts (an install, a clone, a small
build) run at local speed. Then a **real cache bound with sequential
read-ahead**, before any volume grows past 20 GB.

## Gaps

- The invoice was not read (the token has no billing access); the
  analytics' usage dataset is the evidence that intercepted puts are
  billed egress.
- The thrash numbers are krun with R2's costs modelled. A Cloudflare run
  of the same job would take about an hour and was not made.
- The usage mixes, the 30% garbage slack and the cleaner's cost are
  estimates. The cleaner is not built.
- Compression, scratch paths and the larger budget are not measured.
