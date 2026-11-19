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

Answers `200` with an empty body and two headers:

- `x-fpac-disk`: the disk's name.
- `x-fpac-epoch`: this life's epoch, a decimal integer, at least 1.

(Phase P3 adds the head: the checkpoint this life opens from.)

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

`fpac-testkit::conformance::run` is the suite. Every remote answers all
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

## 2. The guest's promise

- **`/data` is ext4**, made and mounted by the guest's own kernel, on a
  loop device over one FUSE file. `statfs` says ext4; `/proc/self/mountinfo`
  shows ext4 on `/dev/loopN`. Programs that check their filesystem (SQLite
  in WAL mode, for one) see what they see on a local disk.
- **fsync returns when the data is on the container's own disk**, as on
  any local disk. The remote is about a second behind (`flush_ms`).
- **What a crash may lose:** writes a program synced in the last flush
  interval plus the put in flight (about a second); writes it never
  synced, the kernel's writeback delay as well (set to 5 s in the guest).
  Nothing older.
- **What R2 holds is always a real moment of the disk**: the disk as it
  stood at some flush, as after a power cut at that instant. ext4 replays
  its journal and SQLite its WAL, as after a power cut.

## 3. The platform's part

- It mints the epoch at each start, larger than any earlier, and hands
  the route the disk and the epoch.
- It keeps the head (the current checkpoint) in its own state, and moves
  it only forward (design.md, The head).
- It deletes objects no kept checkpoint names, at its own pace, never in
  reaction to a guest's call.
- It registers the intercept before the container starts, where the
  runtime allows it, so the guest's first call never races it.
