# What FPAC is built on

FPAC builds a small block store and leans on the kernel for everything
POSIX: ext4, the page cache, the loop device, FUSE. This document is the
case for that, the candidates it was weighed against, and the audit of
each piece we wrote by hand instead of taking from a crate.

## Build or adopt

| Candidate | What it is | Verdict | Why |
| -- | -- | -- | -- |
| **Our own lazy disk under ext4** | The kernel's ext4 over a small block store of ours, served through one FUSE file and a loop device | **Yes** | No maintained tool gives a boot pack, a credential-free path through the platform's egress, and one object write per flush. What is ours is small: a map, a log and a cut. Everything POSIX stays the kernel's. The shape is old: s3backer has served a FUSE file under a loop device since 2008 |
| **ZeroFS** | Rust. A filesystem on an LSM in the bucket, with NFS, 9P, NBD and FUSE clients | **No** | AGPL-3.0 (the owner: avoid AGPL if possible). Besides the licence: it speaks S3 with keys in the guest, always encrypts (a passphrase in the guest), and has no boot pack. Kept as a design to read: the writer's epoch in every key comes from it |
| **SlateDB** | The LSM under ZeroFS. One writer, fenced | No | A good engine for keys and values. For 64 KiB blocks an LSM rewrites data level by level, and it scatters the boot's blocks by key, where we want them in boot order |
| **JuiceFS** | A FUSE client with a metadata engine (Redis, SQL, TiKV), 4 MiB blocks in the bucket | No | Every create and rename is a round trip to the engine. A Durable Object as the engine would be ours to write, far from the container |
| **mountpoint-s3, tigrisfs, geesefs, s3fs** | One file, one object | No | Not POSIX enough (no rename, random write, lock or hard link in some), one object write per file, and SQLite's WAL unsafe on them |
| **Cloudflare's dofs + computerd** | A filesystem in a Durable Object's SQLite, mirrored into the container by a FUSE daemon | No, for `/data` | A full mirror: every byte in the Durable Object's SQLite (a 10 GB cap), and a fresh container is sent the whole tree again. Experimental by its own word. A fit for a different job: a small workspace shared between an isolate and a sandbox |
| **Our own POSIX filesystem on FUSE** | – | No | Thousands of lines of rename, link, lock and mmap rules to get right, and the agent would see a FUSE home. Some programs (SQLite in WAL mode among them) check what filesystem they are on and change their habits |

## The face: which block device

The probe (spike.md, Acceptance 1) answers which face each place offers.
The order of preference:

1. **A loop device over one FUSE file, with direct I/O.** FUSE is the
   surface Cloudflare's own examples use. The loop device turns the file
   into a block device the kernel's ext4 mounts. Direct I/O keeps one
   copy of each block in memory (ext4's), not two.
2. **ublk**, if both kernels have it: a block device served from user
   space over io_uring, with no file in between.
3. **NBD**, the same over a socket.
4. **`fuse2fs`**, if no block device is possible: e2fsprogs' ext4 in user
   space over the same store. It keeps the format, at the cost of a FUSE
   home.

The store under the face is the same in every case; only the face
changes.

## Written by hand, and why

The engineering style asks that every dependency be audited before it is
added, and that bespoke code say why it exists. Each of these was a
choice between a crate and a few hundred lines of ours.

### FUSE: `/dev/fuse` by hand, not `fuser`

- **What we need:** one file in one directory. About a dozen requests:
  `INIT`, `LOOKUP`, `GETATTR`, `SETATTR` (accepted, never a resize),
  `OPEN`, `READ`, `WRITE`, `FSYNC`, `FLUSH`, `RELEASE`, `FALLOCATE` (the
  loop device's discard), `STATFS`, and the forgets. Everything else is
  answered `ENOSYS`.
- **`fuser`** (MIT, 0.18.0) is current, but its README says it no longer
  takes pull requests and that future releases are written mainly by a
  coding agent. It brings a general filesystem trait, its own mount
  helper path, and options we would have to keep in step with.
- **By hand** (`crates/device/src/abi.rs`, `serve.rs`, `linux.rs`): about
  400 lines with tests. The protocol's structs are fixed by the kernel's
  ABI (7.31 and later; every kernel we target has it). The answers are a
  pure function of the request and the backing, tested on any host. The
  threads read cloned session descriptors (`FUSE_DEV_IOC_CLONE`), so
  requests are served in parallel. The mount is one `mount(2)` with
  every option named.
- **Decision:** by hand, as sandcastle does its seccomp and macrofiche its
  Landlock.

### The loop device: its ioctls, not `losetup`

`LOOP_CTL_GET_FREE`, `LOOP_CONFIGURE` (direct I/O, autoclear, 4 KiB
blocks, in one call), `LOOP_GET_STATUS64` to read back whether the kernel
took direct I/O (it may refuse it silently), and `LOOP_CLR_FD`. About 100
lines; the structs' sizes are asserted at compile time.

### HTTP/1.1: by hand, not `hyper`

The guest's one client speaks to one host: requests with a known-length
body, answers by length or chunked. About 200 lines, bounded at every
step (`crates/remote/src/http.rs`). No async runtime in the guest: the
daemon's threads block on the network the way the FUSE threads block on
`/dev/fuse`. The driver reuses the same exchange over rustls.

### ext4: the e2fsprogs programs

`mke2fs` and `e2fsck` run as programs, never linked (they are GPL; the
image ships them as Debian packages). Every option is named at the call
site (`crates/device/src/linux.rs`).

## What similar systems taught

- **ZeroFS:** the writer's epoch in every object's key, so an old life
  can only write where the new one never reads past its open; segments
  as the unit of upload.
- **s3backer:** a FUSE file under a loop device works, and has for years.
  The known traps are memory pressure on the write path (answered by
  `PR_SET_IO_FLUSHER` on the serving threads, `mlockall`, and direct I/O)
  and a daemon that dies with I/O in flight (the loop device then fails
  I/O, and the kernel's ext4 handles that as a dead disk).
- **computerd:** what a full mirror costs: their own figure is 3,000 files
  sent again in 3.7 s at every fresh start, and an `npm install` twice as
  slow on its mount as on the container's disk.
