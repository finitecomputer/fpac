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

**A spike, not production.** All five phases have run: the design (P1),
the probe and the path (P2), the store (P3), the daemon (P4) and the
wakes on krun and Cloudflare (P5). See [docs/spike.md](docs/spike.md),
Results, and the evidence in `docs/evidence/`. No number here comes from
a stand-in unless it says so.

What it showed (2026-11-19):

| | |
| -- | -- |
| The face, on Cloudflare (kernel 6.18.64) and krun (6.12.115) | ext4 on a loop device over one FUSE file, with direct I/O, works on both; local workerd's Docker container has no face |
| Nothing flushed is lost | 150 crash rounds on krun and 20 on Cloudflare: `e2fsck`, SQLite and `git fsck` clean, every acknowledged write present |
| A wake on Cloudflare, from the second on | the mount in 0.65–1.24 s and the boot's files read with no miss, the same at 1, 5 and 10 GB (it took 3.3–8.9 s and grew with the data before the open fetched its map in one round). The plan's 150 ms mount needs the platform to hand the guest its checkpoint (spike.md, proposals) |
| Hermes on krun | first reply a median 2.9 s against 5.7 s on the container's own disk; WAL kept; ext4 on `/dev/loop0` |
| Tools | git and SQLite within the plan's lines; a write larger than 256 MiB goes at R2's shipping rate (47–51 MB/s), which misses them |
| Wrong bytes | two wrong ranged reads on Cloudflare in P2; none in 1,050,000 checked reads since. Every extent is checked and fetched again on a mismatch |

## Layout

| Crate | What |
| -- | -- |
| `crates/device` | `fpac-device`: the face. One file served over `/dev/fuse` by hand, a loop device over it with direct I/O, ext4 made and mounted (Linux only; the protocol is pure) |
| `crates/remote` | `fpac-remote`: the gate to the remote (open, ranged get, put-if-absent, the head) over plain HTTP/1.1 |
| `crates/format` | `fpac-format`: the store's objects (segments, map pages, checkpoints, packs) and its limits |
| `crates/store` | `fpac-store`: the store's logic, pure, behind two gates; the head's rules; the simulator |
| `crates/fpac` | `fpac`: the daemon in the guest (`fpac mount`, `fpac status`); one static binary |
| `crates/testkit` | `fpac-testkit`: the folder remote (a stand-in) and the conformance suite |
| `crates/probe` | `fpac-probe`: the guest's half of the evidence driver; one static binary |
| `crates/bench` | `fpac-bench`: the evidence driver, `deploy`, `inventory`, `reset` |
| `worker/` | The spike's stand-in Worker: the remote's route on Cloudflare (JavaScript) |
| `images/tiny` | The spike's tiny image |

## Build and test

Rust 1.99.0, pinned by `rust-toolchain.toml` (rustup installs it), on
Linux or macOS:

```sh
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release --target x86_64-unknown-linux-musl -p fpac-probe -p fpac
```

The release binary, named by its SHA-256, and how to build the same
bytes again: docs/release.md.

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
cargo run --release -p fpac-bench -- wake --place krun --gb 10 --write --label 10gb
cargo run --release -p fpac-bench -- crash --place cloudflare --rounds 10 --write
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
- [docs/costs.md](docs/costs.md): what a disk costs a person on
  Cloudflare (intercepted puts are billed egress; reads are not), and what
  a working set larger than the cache does.

## License

MIT (`LICENSE`).
