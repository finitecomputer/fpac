# fpac

A lazy disk for long-lived computers. `/data` is an ordinary ext4
filesystem, made by the guest's own kernel, on a loop device over one
FUSE file. The blocks under it live in an object store (R2) as write-once
objects, fetched on first read and shipped about once a second. A wake
fetches one "boot pack" beside the agent's own start, so it takes the same
time at any disk size; a sleep is a flush. The guest reaches the store
through an intercepted host and holds no credential.

It is a sibling of sandcastle (the computers' engine) and macrofiche (the
files' git store), built the same way: its own workspace, a spike written
to the engineering style, evidence as files, one reset.

## Status

**A spike in progress, not production.** Phases P1 (the design) and P2
(the probe and the path) are done; the store (P3), the device and binary
(P4) and the wakes on krun and Cloudflare (P5) are next. See
[docs/spike.md](docs/spike.md), Results, and the evidence in
`docs/evidence/`. No number here comes from a stand-in unless it says so.

Measured in P2 (2026-11-19):

| | |
| -- | -- |
| The face on Cloudflare (kernel 6.18.64) and on krun (6.12.115) | a loop device over one FUSE file, with direct I/O, and ext4 on it: works on both; FUSE mount 2 ms, loop attach 8 ms, ext4 mount 17 ms on Cloudflare |
| Through the face on Cloudflare, 256 MiB written and synced, read cold | 74% and 94% of the container's own disk (a local file behind the FUSE file) |
| Local workerd's Docker container | no face: no `/dev/fuse`, no loop device, no `CAP_SYS_ADMIN` |
| The path on Cloudflare: guest → intercept → handler → R2 | a 64 KiB ranged get 111 ms to first byte (p50), of which R2 98 ms and the intercept about 7 ms; an 8 MiB put 709 ms; a 50 MiB stream 25–50 MB/s |

## Layout

| Crate | What |
| -- | -- |
| `crates/device` | `fpac-device`: the face. One file served over `/dev/fuse` by hand, a loop device over it with direct I/O, ext4 made and mounted (Linux only; the protocol is pure) |
| `crates/remote` | `fpac-remote`: the gate to the remote (open, ranged get, put-if-absent) over plain HTTP/1.1 |
| `crates/testkit` | `fpac-testkit`: the folder remote (a stand-in) and the conformance suite |
| `crates/probe` | `fpac-probe`: the guest's half of the evidence driver; one static binary |
| `crates/bench` | `fpac-bench`: the evidence driver, `deploy`, `inventory`, `reset` |
| `worker/` | The spike's stand-in Worker: the remote's route on Cloudflare (JavaScript) |
| `images/tiny` | The spike's tiny image |

## Build and test

Rust stable (1.99 tested), on Linux or macOS:

```sh
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release --target x86_64-unknown-linux-musl -p fpac-probe
```

The evidence (Linux, with Docker; the krun place needs a sandcastle
engine; the Cloudflare place needs a local config outside this
repository, `$FPAC_SPIKE_CONFIG`, naming the account, the Worker, the
bucket, a token file, the driver's key file and a Node 24 to run wrangler):

```sh
cargo run --release -p fpac-bench -- probe --place krun --write
cargo run --release -p fpac-bench -- probe --place docker --write
cargo run --release -p fpac-bench -- deploy
cargo run --release -p fpac-bench -- probe --place cloudflare --write
cargo run --release -p fpac-bench -- path --place cloudflare --write
cargo run --release -p fpac-bench -- reset
```

## Docs

- [docs/contract.md](docs/contract.md): what FPAC answers: the remote's
  calls, the guest's promise, the platform's part.
- [docs/library.md](docs/library.md): what it is built on, and what was
  written by hand and why.
- [docs/design.md](docs/design.md): the flows, the limits, the phases, the
  evidence, the debt, the risks.
- [docs/spike.md](docs/spike.md): the spike: its acceptance, constraints
  and results.

## License

MIT (`LICENSE`).
