# What FPAC answers: the contract

FPAC is a lazy disk. A guest (a container or a microVM) sees one ext4
filesystem at `/data`, made by its own kernel; the blocks under it live in
an object store as write-once objects. This document fixes what the two
sides of that promise are:

1. **The remote's calls** (section 1): what the platform's route must
   answer, over plain HTTP, to the guest's daemon. The spike's Worker and
   the test kit's folder remote both answer exactly this, and the
   conformance suite holds them to it.
2. **The guest's promise** (section 2): what a program on `/data` may
   count on, and what it may lose.
3. **The platform's part** (section 3): what the platform hands the route
   and keeps for itself.

Words: a **disk** is one volume, named by the platform. An **epoch** is
one life of a disk: a positive integer, minted by the platform at each
start, larger than every earlier one. An **object** is a named, immutable
blob under the disk. **Segments**, **checkpoints** and **packs** are the
kinds of object the store writes (design.md); the remote does not tell
them apart.

## 1. The remote's calls

The guest reaches the remote at one host, `fpac.fragment.internal`, port
80, plain HTTP/1.1. The platform intercepts that host and hands the
connection to its route, which knows the disk and the epoch from what the
platform gave it at the start (its props). **The guest sends neither and
holds no credential**: whatever it puts in a request, it can reach only
its own disk, and write only under its own epoch.

The client keeps one connection open between calls when the route
allows it, and repeats a call once on a fresh connection when a kept one
turns out closed before any answer arrived. Every call is safe to repeat.

### `GET /open`

Answers `200` with an empty body and four headers:

- `x-fpac-disk`: the disk's name.
- `x-fpac-epoch`: this life's epoch, a decimal integer, at least 1.
- `x-fpac-volume-bytes`: the volume's size.
- `x-fpac-head`: the disk's head, or `none` for a new disk: one line,
  `<epoch> <checkpoint epoch> <checkpoint seq> <sha256 hex>`, or
  `<epoch> - - -` for a disk with no checkpoint yet, followed by
  ` clean` when that life slept cleanly (its checkpoint covers everything
  it shipped, so the next open replays nothing). The head's epoch is
  older than this life's. The route answers it from what the platform
  gave it at the start: no storage read.

### `POST /head`

The body is a head's line (as above) for this life. The platform moves
the disk's head only by the head's rules (`fpac_store::head::decide`):
only the running life may post; a new life may name any checkpoint (it
opened from the old head and is the only writer); within a life the
checkpoint only moves forward, and the same checkpoint may gain the clean
mark (a sleep's last post) but never lose it. Answers:

- `200` with `x-fpac-head-answer: taken`, or `same` (the same head again:
  an earlier post whose answer was lost);
- `409` with the reason: another life's head, an older checkpoint, the
  same checkpoint with another body, a checkpoint from a later life.

A life posts its head before it ships anything, so a life that never
posted one shipped nothing, and an open replays one life's segments only:
the head life's, after the chain's end its checkpoint covers.

### Object names

`e<epoch>/<rest>`:

- `<epoch>`: decimal, no leading zero, at least 1, at most 20 digits.
- `<rest>`: a lowercase letter, then lowercase letters, digits, `.` and
  `-`.
- At most 96 bytes in all.

A name outside this grammar is answered `400`. The object lives at
`disks/<disk>/<name>` in the bucket; the guest never sees that key.

### `GET /o/<name>`

- With no `range`: `200`, the whole object, `content-length`, and
  `x-fpac-sha256` (the SHA-256 the put named, lowercase hex). The client
  checks the bytes against it and treats a mismatch as corruption.
- With `range: bytes=<first>-<last>` (the only form): `206`, the bytes
  from `first` to `last` or to the object's end, whichever is first, and
  `content-range`. A range that starts past the end is `416`.
- An absent object: `404`.

Any epoch of the disk may be read: an open replays earlier lives'
segments.

### `PUT /o/<name>`

Put-if-absent. The request carries the body, its `content-length`, and
`x-fpac-sha256`. The route:

- refuses a name whose epoch is not this life's: `403`;
- refuses a body over `OBJECT_BYTES_MAX` (96 MiB): `413`;
- refuses a body that does not match its `x-fpac-sha256`: `400` (R2
  checks it);
- stores the object only if nothing is at the name, and answers with the
  stored object's checksum in `x-fpac-sha256`:

| What was at the name | Status | `x-fpac-sha256` | The client reads it as |
| -- | -- | -- | -- |
| nothing | `201` | the body's | `Put::Created` |
| the same bytes | `200` | the body's | `Put::Same`: our own earlier put, whose answer was lost |
| other bytes | `409` | the stored object's | `Put::Conflict`: another writer. The daemon is fenced and stops |

There is no delete and no list. The platform deletes, on its own
schedule, from its own state (design.md, "Run at your own pace").

### Status codes the client reads

- `5xx`, a refused connection, a timeout: `RemoteError::Unavailable`,
  retried by the caller later.
- `4xx` other than those above: `RemoteError::Refused`, never retried.
- An answer that breaks this contract (a `201` naming another checksum, a
  missing header): `RemoteError::Protocol`.
- Bytes that fail their checksum: `RemoteError::Corrupt`.

### Limits

All in `crates/remote/src/limits.rs`, each checked where its input
arrives:

| Limit | Value | Why |
| -- | -- | -- |
| `OBJECT_BYTES_MAX` | 96 MiB | A pack's largest; under the intercept's 100 MiB request body limit (asserted) |
| `NAME_BYTES_MAX` | 96 | `e` + 20 digits + `/` + a kind and a sequence number, with room |
| `HEAD_BYTES_MAX`, `HEADERS_MAX` | 16 KiB, 64 | An answer's head |
| `SMALL_BODY_BYTES_MAX` | 64 KiB | An error's text or `/open`, read whole |
| `IO_MS_MAX` | 30 s | One read or write on the connection; a stalled remote is an operating error, never a hang |

### Conformance

`fpac-testkit::conformance::run` is the suite (19 cases). Every remote answers all
of it: the folder remote in this workspace's tests, the stand-in Worker
from inside a guest (`fpac-probe conformance`), and later the platform's
own route in its own end-to-end tests.

1. `open` names the disk and this life's epoch.
2. A put of a new name is created.
3. The same put again is the same.
4. Other bytes at that name conflict, naming the stored checksum.
5. A get streams the whole object, its checksum checked.
6. A ranged get answers exactly the range.
7. A range past the end answers the tail.
8. A range starting past the end is refused (`416`).
9. An absent name reads as absent (ranged).
10. An absent name streams as absent.
11. A put into another life's epoch is refused (`403`).
12. An empty object round-trips.
13. This life's head is taken.
14. The same head again is the same.
15. A later checkpoint of this life is taken.
16. The same checkpoint with another body is refused (`409`).
17. The same checkpoint marked clean (a sleep) is taken.
18. A clean head never loses its mark (`409`).
19. A head from a life that isn't running is refused (`409`).

## 2. The guest's promise

- **`/data` is ext4**, made and mounted by the guest's own kernel, on a
  loop device over one FUSE file. `statfs` says ext4; `/proc/self/mountinfo`
  shows ext4 on `/dev/loopN`. Programs that check their filesystem (SQLite
  in WAL mode, for one) see what they see on a local disk.
- **fsync returns at once.** The container's own disk dies with the
  container, so syncing the cache file would protect nothing: what
  protects a write is the next cut reaching the store, about a second
  later (`FLUSH_MS`). Every cut is a whole moment of the device (it takes
  every write made before it, at one instant), so the order ext4's
  journal relies on holds in the store.
- **What a crash may lose:** writes made in the last flush interval plus
  what is still shipping: about a second while shipping keeps up. During a
  burst, writes go at the local disk's speed with shipping behind them,
  up to `UNSHIPPED_BYTES_MAX` (4 GiB) unlanded: a crash then loses up to
  that backlog, about 85 s of writes at the 50 MB/s a Cloudflare container
  ships, and a sleep's flush takes as long. Writes a program never synced,
  the kernel's writeback delay as well (the daemon sets it to 5 s in the
  guest). Nothing older: a cut whose segments have all landed is never
  lost.
- **What R2 holds is always a real moment of the disk**: the disk as it
  stood at some flush, as after a power cut at that instant. ext4 replays
  its journal and SQLite its WAL, as after a power cut.

## 3. The platform's part

- It mints the epoch at each start, larger than any earlier, and hands
  the route the disk, the epoch, the volume and the head.
- It keeps the head in its own state, as typed columns, and moves it only
  by the head's rules.
- It deletes objects no kept checkpoint names, at its own pace, never in
  reaction to a guest's call.
- It registers the intercept before the container starts, where the
  runtime allows it, so the guest's first call never races it.
