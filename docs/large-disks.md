# Disks bigger than the container's 20 GB

**In plain words.** A Cloudflare container's own disk is at most 20 GB, and
it is the only fast storage a computer has. FPAC can hold a disk of any
size in R2 and keep its working part in that 20 GB as a cache. That works
as long as what the agent uses fits the cache. When it does not, every
byte the cache dropped comes back from R2: a scan comes back at tens of
MB/s instead of hundreds, and a scattered read waits about 0.1 s per
miss. The fix is not a bigger disk (there is none to buy) but keeping the
cache for what matters: build outputs and package caches local and never
shipped, the agent's own files kept, scans read ahead, and the agent told
when it is over.

**The recommendation:** keep the container's disk as a bounded cache with
scan-resistant eviction (built: S3-FIFO, item 1b); make build outputs,
package caches and `node_modules` local-only (fragment's image work; the
owner decided the first two, and C measured the third); pin the
agent's home; tell the agent its working set against the cache; and
measure a real large build on Cloudflare next. Bigger instance disks and
a memory tier are not worth building for now.

## What the cache is today

The cache file's bound is what the container's disk has free at the
mount, less 4 GiB for unshipped writes' copies and 2 GiB of headroom
(design.md, The cache). On a 20 GB container with Hermes' image that is
about 10 to 12 GB. It gives that room back as local scratch takes it,
down to a floor of 4 GiB and the next pack, and takes it again when
scratch is deleted. Past it, clean extents leave by S3-FIFO: a one-pass
scan goes through a small queue and out, and what is used again stays.
Never dropped: what is unshipped, what a write is reading, and the next
pack's boot extents. Scans are followed, 64 MiB fetched ahead on eight
connections.

## The options

### A scan-resistant eviction policy, and where its cliff is

Built (item 1b). Measured on krun with R2's costs modelled, a 10.9 GB
build directory against a 3 GiB cache (`thrash.json`):

| | Write 10.9 GB | Read it back | After a wake |
| -- | -- | -- | -- |
| The container's own disk | 25–40 s | 9–22 s | (gone) |
| FPAC before 1b (FIFO, one miss at a time) | 296 s | 1,137 s (9.6 MB/s) | 1,126 s |
| FPAC now | 136 s | 171 s (64 MB/s) | 146 s (75 MB/s) |

**The cliff.** The policy keeps a hot set through a scan, but nothing
keeps a working set larger than the cache: the part that does not fit is
read from R2 every time. How steep depends on the reads:

- **Sequential** (a build reading its outputs, a model's weights):
  64–75 MB/s with one reader of 4 MiB files on krun, each file's first
  read a miss before its window starts; one long stream reaches the
  176–194 MB/s four parallel gets did on Cloudflare (P5). Against a local
  disk's 500 MB/s to 2 GB/s: 3 to 30 times slower.
- **Scattered** (a database larger than the cache, `git status` on a huge
  tree): one R2 round trip per miss, about 0.1 s (97 ms p50 first byte on
  Cloudflare, `path.json`); 1 MiB of read-ahead per miss helps only where
  neighbours are read next. Hundreds of times slower than a local disk.
- **Writes** are not on the cliff: up to 4 GiB goes at the local disk's
  speed, then at the shipping rate (100 MB/s on Cloudflare behind a
  burst).

### Larger instance disks

Not on offer. Cloudflare's limits (developers.cloudflare.com/containers,
limits and pricing pages, 11-12 and 11-13) cap one instance at 4 vCPU,
12 GiB and **20 GB of disk** (standard-4's), "subject to your account
limits"; the account's concurrent disk is 300 TB and can be raised, a
single instance's is not said to be. Disk is billed by what is
provisioned, about $0.18 per GB-month while a container runs ($0.00000007
per GB-second), twelve times R2's $0.015, so even a larger disk would be
a cache, not a home. There are no persistent volumes; snapshots (20 GB at
most) are the only kept files.

### Memory as an extra cache tier

Already in use, twice over: the guest's page cache holds what ext4 read,
and the cache file's own pages hold the same extents again. A container's
memory is billed by what is provisioned too, $0.0000025 per GiB-second
(about $6.6 per GiB-month while it runs, 36 times its disk's), so memory
bought for caching costs far more than the disk it would stand in for. Cheaper than a tier: read the
cache file with direct I/O so its pages stop duplicating ext4's, which
gives that memory back to the agent. Worth a PR; not a tier.

### Per-path policies

- **Local-only scratch** (decided; fragment's image work): `target/`,
  `~/.cache`, `/tmp` and package caches on the container's own disk,
  never shipped. Builds write at the local disk's speed and nothing they
  make competes for the cache or costs egress; the price is a rebuild
  after each wake.
- **Pinned hot sets:** the agent's home (its databases, its sessions,
  Hermes' state) never evicted, as the pack's boot extents already are.
  The daemon sees blocks, not paths, so a pin is an extent list from the
  guest (a file's extents by `FIEMAP`) or a second, unbounded FPAC disk
  for the home. A few hundred MB; one PR.
- **`node_modules`: local-only** (C, results.md). Shipped, a heavy tree
  (61,845 files, 947 MB) cost 3.2 GB of egress for two installs, a 40 s
  sleep, and a 50.7 s cold start at the next wake (scattered small files,
  a miss each); local-only, a reinstall of 26 s at the wake that needs it,
  and nothing else.

### Telling the agent

The daemon knows the working set against the bound: drops per minute,
misses per minute, what a pass reads back. It can say so where the agent
looks (a line in its status, a skill that reads `/run/fpac/status.json`):
"your build reads 30 GB through a 10 GB cache; move `target/` to local
scratch". The agent can act on that; it cannot act on a slow disk it
does not understand. One PR in the daemon, one in fragment's skill.

## What to measure next

1. A real large build on Cloudflare: a Rust workspace's `cargo build`
   with `target/` shipped, and local-only, against real R2.
2. The cliff as a curve: working set at 50%, 100%, 150% and 300% of the
   cache, sequential and scattered, on Cloudflare.
3. A first miss of 4 MiB instead of 1 MiB, and parallel readers, for the
   file-by-file scan.
4. A week of a real agent's misses and drops, to size the pins.
