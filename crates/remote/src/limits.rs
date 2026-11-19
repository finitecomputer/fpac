//! The remote's limits (docs/contract.md, Limits). Each is checked where
//! its input arrives; a value past one is refused, never truncated.

/// One object's bytes: a segment's or a pack's largest. Below the
/// intercept's 100 MiB body limit on Cloudflare (asserted).
pub const OBJECT_BYTES_MAX: u64 = 96 << 20;
/// The intercept's request body limit (Cloudflare's documentation).
pub const INTERCEPT_BODY_BYTES_MAX: u64 = 100 << 20;
/// An object name's bytes: `e<epoch>/<name>`.
pub const NAME_BYTES_MAX: usize = 96;
/// An answer's status line and headers together.
pub const HEAD_BYTES_MAX: usize = 16 << 10;
/// An answer's headers.
pub const HEADERS_MAX: usize = 64;
/// A small answer body read whole (an error's text, `/open`).
pub const SMALL_BODY_BYTES_MAX: usize = 64 << 10;
/// How long one read or write on the connection may block. A stalled
/// remote is an operating error the caller retries, never a hang.
pub const IO_MS_MAX: u64 = 30_000;
/// The buffer between the socket and the parser.
pub const READ_BUFFER_BYTES: usize = 256 << 10;

const _: () = assert!(OBJECT_BYTES_MAX < INTERCEPT_BODY_BYTES_MAX);
