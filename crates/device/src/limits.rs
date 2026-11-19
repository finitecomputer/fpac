//! The device's limits, each a constant with its reason (docs/design.md,
//! Limits). Relations between them are asserted at compile time.

/// The block size of the loop device and of ext4 on it.
pub const BLOCK_BYTES: u32 = 4096;
/// The largest write the kernel may send in one request (`max_write`).
/// 1 MiB is FUSE's own ceiling at 256 pages of 4 KiB.
pub const FUSE_WRITE_BYTES_MAX: u32 = 1 << 20;
/// The largest read: the same, so a buffer of one size serves both.
pub const FUSE_READ_BYTES_MAX: u32 = FUSE_WRITE_BYTES_MAX;
/// `max_pages`: a request's size in pages.
pub const FUSE_PAGES_MAX: u16 = (FUSE_WRITE_BYTES_MAX / 4096) as u16;
/// Requests the kernel may have in flight in the background.
pub const FUSE_BACKGROUND_MAX: u16 = 64;
/// A serving thread's buffer for one request: the largest write and its
/// headers. The kernel refuses a read of /dev/fuse into anything smaller.
pub const FUSE_BUFFER_BYTES: usize = FUSE_WRITE_BYTES_MAX as usize + 4096;
/// Threads reading /dev/fuse: the loop device keeps several requests in
/// flight, and two vCPUs are the computer's.
pub const FUSE_THREADS: usize = 4;
/// The largest volume the file may stand for: 100 GB is the goal, and the
/// map in pages is built for it (docs/design.md); 1 TiB leaves room.
pub const VOLUME_BYTES_MAX: u64 = 1 << 40;
/// Tries to take a free loop device: another process may take the one
/// the kernel named between our ask and our attach.
pub const LOOP_ATTACH_TRIES_MAX: u32 = 4;

const _: () = assert!(FUSE_WRITE_BYTES_MAX.is_multiple_of(BLOCK_BYTES));
const _: () = assert!(FUSE_PAGES_MAX as u32 * 4096 == FUSE_WRITE_BYTES_MAX);
const _: () = assert!(FUSE_BUFFER_BYTES >= crate::abi::IN_HEADER_BYTES + crate::abi::WRITE_IN_BYTES + FUSE_WRITE_BYTES_MAX as usize);
