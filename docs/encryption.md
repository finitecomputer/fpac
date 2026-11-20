# Sealed disks

A disk can be **sealed**: everything FPAC stores and everything it caches
on the host's disk is encrypted and authenticated, so an agent's computer
in a TEE can mount its data from FPAC with nothing readable or changeable
outside it. It is behind a flag (`fpac mount --key-file <file>`); a disk is
sealed or plain for good, and each refuses the other's objects by their
magic. This file is the design, what it costs (measured), the key's
custody (the owner's decision), and what a TEE would still be missing.

## The cipher

**AES-256-GCM-SIV** (RFC 8452), from RustCrypto's `aes-gcm-siv` 0.12.1:
pure Rust, AES-NI and CLMUL found at run time, so the static guest uses
them where the CPU has them. Chosen over XChaCha20-Poly1305 for two
reasons:

- **Speed where we run.** One core sealing 64 KiB extents
  (`fpac-probe aead`, `docs/evidence/encryption.json`): see the table
  below. Both machines measured have AES-NI.
- **Misuse resistance.** The cache file reseals an extent on every write.
  GCM-SIV derives its nonce's effect from the message, so a repeated nonce
  only shows that the same bytes were sealed twice at the same place;
  XChaCha20's 192-bit nonce makes a collision as unlikely, but a repeat
  would leak the XOR of two extents and allow forgery. Our nonces come
  from each seal's place (below) and never repeat with other bytes; GCM-SIV
  makes that a defence in depth rather than the only line.

## What is sealed, and what each seal binds

Each seal's associated data is `fpacsiv1`, the SHA-256 of the disk's name,
then its place. The nonce is the first 12 bytes of the SHA-256 of that
associated data.

| Piece | Its place (bound) | Where its tag is |
| -- | -- | -- |
| A segment's image | segment (epoch, seq), extent, offset in the object | after its 64 KiB; the map keeps its first 8 bytes in place of sum64 |
| A segment's header | the segment's name | after the header, before the first image |
| A map page | its name | at its end |
| A checkpoint: **authenticated, not sealed** (decided 2026-11-20) | its name and all its bytes | at its end; its bytes stay readable |
| A pack's header | its name | after the header |
| A packed extent | the pack's name, its index, its extent | after its 64 KiB |
| An extent in the cache file | the life, the extent, this life's count of writes to it | in memory, with the count |

So no sealed piece can be spliced into another object, moved within one,
or swapped for another disk's; in the cache, an earlier write of the same
extent fails as surely as a changed byte (the count is in memory). Object
names, sizes and the clear prefixes (a segment's or pack's magic and
count) are not sealed. Nor is a checkpoint (decided by the owner,
2026-11-20): it names the map pages, the live segments and the packs a
disk keeps, with their checksums and its own place in the chain, all of
it object names or hashes of ciphertext. Kept readable and authenticated
by the key, it lets the platform's cleaner plan deletes from it without
the key (item 6), and the guest still refuses one changed in any byte.
Compaction reads and rewrites the sealed map pages and images, so a
sealed disk's cleaner life holds its key: the platform hands it over at
the cleaner's start as at any other (The key, below), and the life keeps
it in locked memory. Without it, the cleaner is refused at its open.

**What a read checks.** A fetched image opens at its place, and its tag
must be the one the map names: the tag replaces sum64, and a wrong ranged
read (R2 served two in P2) fails its seal and is fetched again, as a wrong
sum was. A page or checkpoint is checked by its SHA-256 (what names it);
a page opens under its name, and a checkpoint's tag is checked over its
bytes and its name. A cached clean extent whose seal fails is
dropped and fetched again; a dirty one (the cache is its only copy) is an
I/O error.

**The layouts** (crates/format/src/seal.rs): a segment is `FPACSEGS`, its
count, its plain header sealed (the header's own SHA-256 kept inside, so
the chain of links is unchanged), then each image and its tag; a pack is
`FPACPAKS` the same way; a page is `FPACSEAL`, its plain bytes sealed,
its tag; a checkpoint is `FPACAUTH`, its plain bytes, then a tag over
them (GCM-SIV over nothing, its associated data the name and the bytes).

## What it costs

Measured on Cloudflare (a `fpac-spike` container, 2 vCPU, against R2)
and on krun (this box, R2 modelled), plain and sealed on the same build,
one after the other (`docs/evidence/encryption.json`).

**The cipher, one core, 64 KiB extents (MB/s).** Both CPUs have AES-NI,
CLMUL, VAES and VPCLMULQDQ; Cloudflare's is an AMD EPYC 9684X.

| | Cloudflare | krun |
| -- | --: | --: |
| AES-256-GCM-SIV seal | 2,202 | 5,108 |
| AES-256-GCM-SIV open | 2,184 | 5,126 |
| XChaCha20-Poly1305 seal | 1,202 | 1,614 |
| SHA-256 (what plain already pays per object) | 1,759 | 2,772 |

**What a person feels, Cloudflare, plain against sealed.** Each run's own
local-disk column moved as much as the difference (a clone took 1.1 s in
one and 2.7 s in the other), so this compares the two FPAC columns.

| | Plain | Sealed |
| -- | --: | --: |
| 4 KiB write + O_SYNC, p50 (idle) | 0.12 ms | 0.21 ms |
| Office save, p50 (idle) | 0.54 ms | 0.73 ms |
| SQLite 10k transactions, `synchronous=FULL` | 4.1 s | 6.2 s |
| SQLite 10k transactions, `NORMAL` | 265 ms | 271 ms |
| SQLite 1M rows, one transaction | 1.33 s | 1.21 s |
| git clone, 7,260 files | 2.6 s | 2.2 s |
| git status, warm / cold | 95 ms / 78 ms | 91 ms / 911 ms |
| Small file, cold read each | 0.08 ms | 0.13 ms |
| 4 GiB written with fsync | 318 MB/s | 273 MB/s |
| Shipped to R2 behind it | 103 MB/s | 97 MB/s |
| 4 GiB read cold | 753 MB/s | 771 MB/s |
| Daemon CPU, user, per GB shipped | 2.8 s | 4.2 s |
| Daemon CPU, all, per GB shipped | 11.0 s | 13.1 s |

So sealing adds about 0.08 ms to each synced write (the cache file
reseals the 64 KiB extent), 0.05 ms to each cold small read (it opens
one), a seventh off sequential writes, and 1.4 s of one core per GB
shipped (the cache's seal and open, then the segment's seal). The CPU
rows are the daemon's whole run, serving ext4 as well as shipping.

**A wake, 1 GB disk, p50 of 3 wakes.**

| | Cloudflare plain | Cloudflare sealed | krun plain | krun sealed |
| -- | --: | --: | --: | --: |
| Wake to the boot read done | 1,989 ms | 1,907 ms | 531 ms | 605 ms |
| Open (head, pages) | 613 ms | 590 ms | 197 ms | 197 ms |
| Pack's first 64 extents installed | 9.7 ms | 16.5 ms | 2.0 ms | 8.2 ms |
| Sleep | 1,954 ms | 2,157 ms | 1,321 ms | 1,320 ms |

Opening the pack costs about 7 ms more per 4 MiB; the rest of a wake is
the container's start and R2's round trips, the same either way.

**Correctness, sealed.** 10 lives killed at random points on Cloudflare
and 20 on krun: every row acknowledged before a kill was found, no wrong
byte handed out, 0 failures. Two lives on Cloudflare (write, sleep, wake,
check files, SQLite, git and the head): all checks pass.

## The key

The daemon is handed a random 256-bit **data key** per disk at its mount
(`--key-file`, a file on the guest's tmpfs, never a command line, removed
by the daemon once read). It
seals everything, for the disk's whole life; it never changes. For now a
test key comes from the spike's local config.

Its custody is the platform's, not FPAC's. An agent's nostr key is kept
sealed by its platform and opened only to sign, so this does not design a
derivation from it; these are the options for the owner:

1. **A wrapping key derived from the agent's key, inside the platform's
   object that holds it.** The data key is stored wrapped (AES-KW or an
   AEAD) beside the disk's head; at a start, the object derives the
   wrapping key (HKDF of the agent's key, salted with the disk) and unwraps.
   Nothing new to keep, but the agent's key is then opened for something
   other than signing.
2. **A wrapping key from a signature.** The object signs a fixed message
   naming the disk, and the wrapping key is a hash of the signature. Keeps
   "opened only to sign" to the letter, but needs deterministic signing
   (BIP-340 with no auxiliary randomness), which is fragile to depend on.
3. **A wrapping key of its own**, random, sealed by the platform as its
   other secrets are, one per agent. Simplest; independent of the agent's
   key, so losing or rotating that key does not lose the disk.
4. **Released to an attested TEE only** (layered on any of the above): the
   object unwraps the data key only for a container whose attestation
   names the expected image.

**A suggestion:** the agent's fragment unwraps the data key once per
start, in parallel with the container's start, and hands it to the daemon
at its mount: nothing added in series to a wake. **Rotation** rewraps the
data key under a new wrapping key and never re-encrypts the data. A data
key that leaked has no remedy but a new disk (a copy, sealed anew).

## What a TEE would still be missing

- **Rollback.** Every object is authentic, but the platform could point
  the head at an older checkpoint, or the store serve an older valid head:
  the disk would open, intact and stale. Closing it needs the head signed
  with a counter the TEE checks against a counter it trusts; a fresh TEE
  remembers nothing, so that counter must live somewhere it can verify (a
  hardware monotonic counter, or the agent's own signed log).
- **Sizes and patterns.** Images are whole 64 KiB extents and nothing is
  compressed, so sizes show no compressibility today; a compressor would
  leak it per extent (pad to buckets, or seal before compressing nothing).
  What does show: how many extents each cut carried (a segment's count and
  size), when the disk writes, which ranges a wake reads, how large the
  boot's working set is (the pack), and the lives' epochs (names).
- **Where the key lives.** In the daemon's memory (locked: `mlockall`) and
  the cache's tags with it; the daemon removes the key file once it has
  read it.
  Outside a TEE the host can read guest memory, so sealing protects the
  store and the host's disk, not a running computer from its host.
- **Availability.** The store or the host can withhold or delete objects;
  that fails loudly (a missing object is corruption), never silently.
