# The fuzz targets

Each target narrows one of fpac's gates, a place where bytes it did not
make come in, and asserts its invariants: no input panics it, whatever it
accepts is canonical (refuse or round-trip), and a sealed object changed
under its seal is refused. The simulator (`crates/store/tests/sim.rs`)
stays the test of whole-system sequences; these test one decoder each.

| Target | The gate | Invariants |
| -- | -- | -- |
| `format` | segments' and packs' headers, map pages, checkpoints, as R2 hands them back | no panic; accepted ⇒ re-encodes to the same bytes; changed under its seal ⇒ refused. Most runs build a valid object, change bytes, and seal it again (reaching the layout checks) or not |
| `fuse` | a request from `/dev/fuse` | a request every decoder accepts is answered without a panic; the answer's header names its length and the request; a read answers at most what it asked; the backing is never asked past the file |
| `http` | the remote's answer, in pieces of any size | no panic; the body is at most its limit, and its Content-Length when it has one; a second exchange on the connection never panics |
| `head` | the head's wire form and the rules that move it | one spelling (raw input, and one-character edits of real heads); over any sequence of lives and posts only the running life moves the head, a life's checkpoint only goes forward, the clean mark is never lost |

Each was checked against a planted bug: a decoder that let a reserved
field through (`format`, found in seconds), a head decoder that took a
sign or a leading zero (`head`, once the one-character edits were added),
and a rule that let a clean head lose its mark (`head`).

## Running

On nightly, with `cargo-fuzz` 0.13.2 (`cargo install cargo-fuzz --locked
--version 0.13.2`):

```sh
cd fuzz
cargo +nightly fuzz run -O format -- -max_total_time=60
```

CI runs each target for 30 seconds on every push (`.github/workflows/ci.yml`,
the `fuzz` job). Longer runs go on a workstation, one target at a time;
on the box this was written on, inside `agent-work.slice`.
