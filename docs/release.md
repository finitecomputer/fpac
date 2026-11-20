# The release binary

`fpac` ships as one static binary for x86_64 Linux (musl), and its name
is its SHA-256: `fpac-<64 hex digits>`. An image that runs it fetches
that name and checks the bytes against it, so what runs is exactly what
was built and tested, and a changed binary is a changed name.

## How it is built

CI's `release` job (`.github/workflows/ci.yml`) builds it on every push:

- with the toolchain `rust-toolchain.toml` pins, from `Cargo.lock`
  (`--locked`);
- linked by the toolchain's own `rust-lld`, against the toolchain's own
  musl, so nothing of the build machine's C toolchain goes in;
- with the build's paths remapped (`/cargo`, `/fpac`), so no machine's
  home or checkout path is in the bytes;
- twice, into two target folders, and refused if the two differ.

The binary and `fpac.sha256` are kept as the run's artifact, named by
the digest. A tag `v*` publishes them as a GitHub release.

## Building it again

On Linux, from the same commit:

```sh
RUSTFLAGS="-C target-feature=+crt-static --remap-path-prefix=$HOME/.cargo=/cargo --remap-path-prefix=$PWD=/fpac" \
  cargo build --locked --release --target x86_64-unknown-linux-musl -p fpac
sha256sum target/x86_64-unknown-linux-musl/release/fpac
```

The digest matches CI's: the same build in two folders on one machine,
and on this machine against CI, gave the same bytes (results.md, item 8).
A Mac with the same toolchain builds a binary with other bytes
(the same size within 8 bytes, code laid out differently): Rust's output
is not yet the same across host platforms, so the release is built on
Linux.

## What CI checks

On Linux and macOS: Clippy with warnings denied, every test of the
workspace, and the store built for `wasm32-unknown-unknown`, the
platform's Worker's target (design.md, The cleaner). The daemon, the
device and the probe are Linux's alone and build to a stub on macOS, so
there the tests are the store's simulator, the formats, the remote and
the folder remote. On Linux: the release binary, as above, and each fuzz
target for 30 seconds.
