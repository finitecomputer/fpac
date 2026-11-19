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

*Filled as each phase exits.*
