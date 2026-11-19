//! One file served over FUSE: the answer to each request, as a pure
//! function of the request and a [`Backing`] (the gate to the bytes). The
//! threads that read /dev/fuse and write the answers are in `linux.rs`;
//! everything here is tested on the host with a backing in memory.
//!
//! The file system holds a root and one regular file, `disk`, of a fixed
//! size. A loop device opens `disk`, and ext4 lives on the loop device, so
//! the only calls that matter are reads, writes, syncs and hole punches
//! (the loop device's discard). Everything a directory tree would need is
//! refused: nothing else may use this mount.

use crate::abi::{self, errno, opcode, Attr};
use crate::limits;

/// What a backing failed to do: an operating error, answered to the
/// kernel as EIO (the loop device and ext4 above it handle that).
#[derive(Debug, thiserror::Error)]
#[error("the backing failed: {0}")]
pub struct BackingError(pub String);

/// The bytes behind the file. A backing is shared by every serving thread.
pub trait Backing: Send + Sync {
    /// Fills `buf` from `offset`; never-written ranges read as zeros.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), BackingError>;
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), BackingError>;
    /// Everything written so far survives a crash once this returns.
    fn sync(&self) -> Result<(), BackingError>;
    /// The range reads as zeros from now on, and its space may be freed.
    fn punch(&self, offset: u64, length: u64) -> Result<(), BackingError>;
}

/// What a serving thread does with one request.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    /// Send these bytes (header included).
    Reply(Vec<u8>),
    /// Send a header, then the first `len` bytes of the thread's read
    /// buffer.
    Read { header: [u8; abi::OUT_HEADER_BYTES], len: usize },
    /// The kernel expects no answer (forget, interrupt).
    Nothing,
    /// The session is over (destroy): answer, then stop.
    Stop(Vec<u8>),
}

/// The one file's fixed facts, set at the mount.
#[derive(Debug, Clone, Copy)]
pub struct File {
    pub size_bytes: u64,
}

impl File {
    pub fn new(size_bytes: u64) -> File {
        assert!(size_bytes > 0);
        assert!(size_bytes.is_multiple_of(u64::from(limits::BLOCK_BYTES)), "the file is whole blocks");
        assert!(size_bytes <= limits::VOLUME_BYTES_MAX);
        File { size_bytes }
    }

    fn attr(&self, node: u64) -> Option<Attr> {
        match node {
            abi::ROOT_ID => Some(Attr { ino: abi::ROOT_ID, size: 0, mode: 0o040_755, nlink: 2, blksize: limits::BLOCK_BYTES }),
            abi::FILE_ID => Some(Attr { ino: abi::FILE_ID, size: self.size_bytes, mode: 0o100_600, nlink: 1, blksize: limits::BLOCK_BYTES }),
            _ => None,
        }
    }
}

fn reply(unique: u64, body: &[u8]) -> Answer {
    let mut v = Vec::with_capacity(abi::OUT_HEADER_BYTES + body.len());
    v.extend_from_slice(&abi::out_header(abi::OUT_HEADER_BYTES + body.len(), 0, unique));
    v.extend_from_slice(body);
    Answer::Reply(v)
}

fn error(unique: u64, e: i32) -> Answer {
    assert!(e > 0);
    Answer::Reply(abi::out_header(abi::OUT_HEADER_BYTES, -e, unique).to_vec())
}

/// The answer to `message`. `read_buf` is the thread's own buffer, made
/// once at its start, at least `limits::FUSE_READ_BYTES_MAX` long.
///
/// A request we cannot decode means we read the kernel's ABI wrong: that
/// is a bug, not input to tolerate, so it panics (and the seatbelt takes
/// the process down; the disk above it fails its I/O rather than
/// continuing on a misread request).
pub fn answer(file: &File, backing: &dyn Backing, message: &[u8], read_buf: &mut [u8]) -> Answer {
    assert!(read_buf.len() >= limits::FUSE_READ_BYTES_MAX as usize);
    let h = abi::in_header(message).expect("a request the kernel sent decodes");
    let u = h.unique;
    match h.opcode {
        opcode::INIT => init(u, &abi::init_in(message).expect("init decodes")),
        opcode::LOOKUP => {
            let name = abi::lookup_name(message).expect("lookup decodes");
            if h.nodeid == abi::ROOT_ID && name == abi::FILE_NAME {
                let mut b = Vec::with_capacity(abi::ENTRY_OUT_BYTES);
                abi::entry_out(&file.attr(abi::FILE_ID).expect("the file"), &mut b);
                reply(u, &b)
            } else {
                error(u, errno::ENOENT)
            }
        }
        opcode::GETATTR => match file.attr(h.nodeid) {
            Some(a) => attr_reply(u, &a),
            None => error(u, errno::ENOENT),
        },
        opcode::SETATTR => {
            // The file's size is the volume's, set at the mount; the kernel
            // may still send times or modes, which we accept and ignore.
            let s = abi::setattr_in(message).expect("setattr decodes");
            let resize = s.valid & abi::FATTR_SIZE != 0 && s.size != file.size_bytes;
            match (file.attr(h.nodeid), resize) {
                (Some(_), true) => error(u, errno::EPERM),
                (Some(a), false) => attr_reply(u, &a),
                (None, _) => error(u, errno::ENOENT),
            }
        }
        opcode::OPEN => match h.nodeid {
            abi::FILE_ID => {
                let mut b = Vec::with_capacity(abi::OPEN_OUT_BYTES);
                abi::open_out(abi::open_flag::DIRECT_IO | abi::open_flag::PARALLEL_DIRECT_WRITES, &mut b);
                reply(u, &b)
            }
            abi::ROOT_ID => error(u, errno::EISDIR),
            _ => error(u, errno::ENOENT),
        },
        opcode::READ => read(file, backing, u, &abi::read_in(message).expect("read decodes"), read_buf),
        opcode::WRITE => write(file, backing, u, &abi::write_in(message).expect("write decodes")),
        opcode::FSYNC => match backing.sync() {
            Ok(()) => reply(u, &[]),
            Err(_) => error(u, errno::EIO),
        },
        opcode::FLUSH | opcode::RELEASE => reply(u, &[]),
        opcode::FALLOCATE => fallocate(file, backing, u, &abi::fallocate_in(message).expect("fallocate decodes")),
        opcode::STATFS => {
            let mut b = Vec::with_capacity(abi::STATFS_OUT_BYTES);
            abi::statfs_out(file.size_bytes / u64::from(limits::BLOCK_BYTES), limits::BLOCK_BYTES, &mut b);
            reply(u, &b)
        }
        opcode::FORGET | opcode::BATCH_FORGET | opcode::INTERRUPT => Answer::Nothing,
        opcode::DESTROY => match reply(u, &[]) {
            Answer::Reply(v) => Answer::Stop(v),
            _ => unreachable!("reply is a Reply"),
        },
        // Directory listings, links, extended attributes, locks: none of
        // them is ours to serve. ENOSYS tells the kernel not to ask again.
        _ => error(u, errno::ENOSYS),
    }
}

fn attr_reply(unique: u64, a: &Attr) -> Answer {
    let mut b = Vec::with_capacity(abi::ATTR_OUT_BYTES);
    abi::attr_out(a, &mut b);
    reply(unique, &b)
}

fn init(unique: u64, i: &abi::InitIn) -> Answer {
    // A newer major: the kernel asks again with ours, which we answer
    // with our major alone (the protocol's own negotiation).
    if i.major > abi::KERNEL_VERSION {
        let mut b = Vec::new();
        abi::init_out(&abi::InitOut { minor: abi::KERNEL_MINOR, max_readahead: 0, flags: 0, max_background: 0, max_write: 0, max_pages: 0 }, &mut b);
        return reply(unique, &b);
    }
    if i.major < abi::KERNEL_VERSION || i.minor < abi::KERNEL_MINOR_MIN {
        return error(unique, errno::EPROTO);
    }
    let wanted = abi::init_flag::ASYNC_READ | abi::init_flag::BIG_WRITES | abi::init_flag::ASYNC_DIO | abi::init_flag::MAX_PAGES;
    let flags = wanted & i.flags;
    // Without MAX_PAGES the kernel caps a request at 128 KiB; we serve
    // that too, only slower.
    let mut b = Vec::with_capacity(abi::INIT_OUT_BYTES);
    abi::init_out(
        &abi::InitOut {
            minor: abi::KERNEL_MINOR,
            max_readahead: i.max_readahead,
            flags,
            max_background: limits::FUSE_BACKGROUND_MAX,
            max_write: limits::FUSE_WRITE_BYTES_MAX,
            max_pages: limits::FUSE_PAGES_MAX,
        },
        &mut b,
    );
    reply(unique, &b)
}

fn read(file: &File, backing: &dyn Backing, unique: u64, r: &abi::ReadIn, buf: &mut [u8]) -> Answer {
    assert!(r.size <= limits::FUSE_READ_BYTES_MAX, "the kernel asked for more than max_read");
    if r.offset >= file.size_bytes {
        return Answer::Read { header: abi::out_header(abi::OUT_HEADER_BYTES, 0, unique), len: 0 };
    }
    let len = u64::from(r.size).min(file.size_bytes - r.offset) as usize;
    match backing.read_at(r.offset, &mut buf[..len]) {
        Ok(()) => Answer::Read { header: abi::out_header(abi::OUT_HEADER_BYTES + len, 0, unique), len },
        Err(_) => error(unique, errno::EIO),
    }
}

fn write(file: &File, backing: &dyn Backing, unique: u64, w: &abi::WriteIn<'_>) -> Answer {
    assert!(w.data.len() <= limits::FUSE_WRITE_BYTES_MAX as usize, "the kernel sent more than max_write");
    let end = w.offset.checked_add(w.data.len() as u64);
    let inside = matches!(end, Some(e) if e <= file.size_bytes);
    if !inside {
        return error(unique, errno::EFBIG);
    }
    match backing.write_at(w.offset, w.data) {
        Ok(()) => {
            let mut b = Vec::with_capacity(abi::WRITE_OUT_BYTES);
            abi::write_out(u32::try_from(w.data.len()).expect("bounded by max_write"), &mut b);
            reply(unique, &b)
        }
        Err(_) => error(unique, errno::EIO),
    }
}

fn fallocate(file: &File, backing: &dyn Backing, unique: u64, f: &abi::FallocateIn) -> Answer {
    use abi::fallocate_mode::{KEEP_SIZE, PUNCH_HOLE, ZERO_RANGE};
    let end = f.offset.checked_add(f.length);
    let inside = matches!(end, Some(e) if e <= file.size_bytes);
    if !inside {
        return error(unique, errno::EINVAL);
    }
    // The loop device sends a discard as a punch, and a write of zeros as
    // a zeroed range; both read back as zeros once punched. A plain
    // allocation (mode 0) is a no-op: space is never reserved ahead.
    let punched = match f.mode {
        m if m == PUNCH_HOLE | KEEP_SIZE || m == ZERO_RANGE || m == ZERO_RANGE | KEEP_SIZE => backing.punch(f.offset, f.length),
        0 | KEEP_SIZE => Ok(()),
        _ => return error(unique, errno::EOPNOTSUPP),
    };
    match punched {
        Ok(()) => reply(unique, &[]),
        Err(_) => error(unique, errno::EIO),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Memory(Mutex<Vec<u8>>);

    impl Backing for Memory {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), BackingError> {
            let m = self.0.lock().unwrap();
            buf.copy_from_slice(&m[offset as usize..offset as usize + buf.len()]);
            Ok(())
        }
        fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), BackingError> {
            self.0.lock().unwrap()[offset as usize..offset as usize + data.len()].copy_from_slice(data);
            Ok(())
        }
        fn sync(&self) -> Result<(), BackingError> {
            Ok(())
        }
        fn punch(&self, offset: u64, length: u64) -> Result<(), BackingError> {
            self.0.lock().unwrap()[offset as usize..(offset + length) as usize].fill(0);
            Ok(())
        }
    }

    fn request(op: u32, node: u64, body: &[u8]) -> Vec<u8> {
        let len = (abi::IN_HEADER_BYTES + body.len()) as u32;
        let mut m = Vec::new();
        m.extend_from_slice(&len.to_ne_bytes());
        m.extend_from_slice(&op.to_ne_bytes());
        m.extend_from_slice(&42u64.to_ne_bytes());
        m.extend_from_slice(&node.to_ne_bytes());
        m.extend_from_slice(&[0u8; 16]);
        m.extend_from_slice(body);
        m
    }

    fn rw_body(offset: u64, size: u32, data: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; abi::WRITE_IN_BYTES];
        b[8..16].copy_from_slice(&offset.to_ne_bytes());
        b[16..20].copy_from_slice(&size.to_ne_bytes());
        b.extend_from_slice(data);
        b
    }

    fn status(a: &Answer) -> i32 {
        let h = match a {
            Answer::Reply(v) | Answer::Stop(v) => &v[..16],
            Answer::Read { header, .. } => &header[..],
            Answer::Nothing => panic!("no answer"),
        };
        i32::from_ne_bytes([h[4], h[5], h[6], h[7]])
    }

    fn fixture() -> (File, Memory, Vec<u8>) {
        (File::new(1 << 20), Memory(Mutex::new(vec![0u8; 1 << 20])), vec![0u8; limits::FUSE_READ_BYTES_MAX as usize])
    }

    // Goal: what is written reads back, and reads past the end are short.
    // Method: write 3 bytes at 4096, read 8 there, then read at the end.
    #[test]
    fn write_then_read() {
        let (f, m, mut buf) = fixture();
        let w = answer(&f, &m, &request(opcode::WRITE, abi::FILE_ID, &rw_body(4096, 3, b"abc")), &mut buf);
        assert_eq!(status(&w), 0);
        let r = answer(&f, &m, &request(opcode::READ, abi::FILE_ID, &rw_body(4096, 8, &[])), &mut buf);
        assert_eq!(r, Answer::Read { header: abi::out_header(24, 0, 42), len: 8 });
        assert_eq!(&buf[..8], b"abc\0\0\0\0\0");
        let end = answer(&f, &m, &request(opcode::READ, abi::FILE_ID, &rw_body((1 << 20) - 4, 8, &[])), &mut buf);
        assert_eq!(end, Answer::Read { header: abi::out_header(20, 0, 42), len: 4 });
        let past = answer(&f, &m, &request(opcode::READ, abi::FILE_ID, &rw_body(1 << 20, 8, &[])), &mut buf);
        assert_eq!(past, Answer::Read { header: abi::out_header(16, 0, 42), len: 0 });
    }

    // Goal: a write past the file's end is refused and changes nothing.
    // Method: a write that straddles the end, and one whose end overflows.
    #[test]
    fn write_past_the_end_is_refused() {
        let (f, m, mut buf) = fixture();
        let w = answer(&f, &m, &request(opcode::WRITE, abi::FILE_ID, &rw_body((1 << 20) - 1, 2, b"xy")), &mut buf);
        assert_eq!(status(&w), -errno::EFBIG);
        let w = answer(&f, &m, &request(opcode::WRITE, abi::FILE_ID, &rw_body(u64::MAX, 2, b"xy")), &mut buf);
        assert_eq!(status(&w), -errno::EFBIG);
        assert!(m.0.lock().unwrap().iter().all(|&b| b == 0));
    }

    // Goal: a punch and a zeroed range read back as zeros; an unknown
    // mode is refused. Method: write, punch, read; then mode 0x8.
    #[test]
    fn punch_reads_zeros() {
        let (f, m, mut buf) = fixture();
        answer(&f, &m, &request(opcode::WRITE, abi::FILE_ID, &rw_body(0, 4, b"data")), &mut buf);
        let mut b = vec![0u8; abi::FALLOCATE_IN_BYTES];
        b[16..24].copy_from_slice(&4u64.to_ne_bytes());
        b[24..28].copy_from_slice(&(abi::fallocate_mode::PUNCH_HOLE | abi::fallocate_mode::KEEP_SIZE).to_ne_bytes());
        assert_eq!(status(&answer(&f, &m, &request(opcode::FALLOCATE, abi::FILE_ID, &b), &mut buf)), 0);
        assert_eq!(&m.0.lock().unwrap()[..4], &[0, 0, 0, 0]);
        b[24..28].copy_from_slice(&0x8u32.to_ne_bytes());
        assert_eq!(status(&answer(&f, &m, &request(opcode::FALLOCATE, abi::FILE_ID, &b), &mut buf)), -errno::EOPNOTSUPP);
    }

    // Goal: only `disk` exists, and its size cannot change. Method: look
    // up `disk` and another name; ask for a resize and for the same size.
    #[test]
    fn one_file_of_a_fixed_size() {
        let (f, m, mut buf) = fixture();
        assert_eq!(status(&answer(&f, &m, &request(opcode::LOOKUP, abi::ROOT_ID, b"disk\0"), &mut buf)), 0);
        assert_eq!(status(&answer(&f, &m, &request(opcode::LOOKUP, abi::ROOT_ID, b"other\0"), &mut buf)), -errno::ENOENT);
        let mut s = vec![0u8; abi::SETATTR_IN_BYTES];
        s[0..4].copy_from_slice(&abi::FATTR_SIZE.to_ne_bytes());
        s[16..24].copy_from_slice(&5u64.to_ne_bytes());
        assert_eq!(status(&answer(&f, &m, &request(opcode::SETATTR, abi::FILE_ID, &s), &mut buf)), -errno::EPERM);
        s[16..24].copy_from_slice(&(1u64 << 20).to_ne_bytes());
        assert_eq!(status(&answer(&f, &m, &request(opcode::SETATTR, abi::FILE_ID, &s), &mut buf)), 0);
        assert_eq!(status(&answer(&f, &m, &request(opcode::OPEN, abi::ROOT_ID, &[0u8; 8]), &mut buf)), -errno::EISDIR);
        assert_eq!(status(&answer(&f, &m, &request(99, abi::ROOT_ID, &[]), &mut buf)), -errno::ENOSYS);
    }

    // Goal: init takes what the kernel offers of what we want, refuses an
    // old protocol, and answers a newer major with ours. Method: three
    // inits.
    #[test]
    fn init_negotiates() {
        let (f, m, mut buf) = fixture();
        let body = |major: u32, minor: u32, flags: u32| {
            let mut b = Vec::new();
            for x in [major, minor, 131072, flags] {
                b.extend_from_slice(&x.to_ne_bytes());
            }
            b
        };
        let a = answer(&f, &m, &request(opcode::INIT, 0, &body(7, 41, u32::MAX)), &mut buf);
        let Answer::Reply(v) = a else { panic!() };
        let flags = u32::from_ne_bytes([v[28], v[29], v[30], v[31]]);
        assert_eq!(flags & abi::init_flag::MAX_PAGES, abi::init_flag::MAX_PAGES);
        assert_eq!(flags & (1 << 16), 0, "never the writeback cache");
        assert_eq!(status(&answer(&f, &m, &request(opcode::INIT, 0, &body(7, 26, u32::MAX)), &mut buf)), -errno::EPROTO);
        assert_eq!(status(&answer(&f, &m, &request(opcode::INIT, 0, &body(8, 0, 0)), &mut buf)), 0);
        assert_eq!(answer(&f, &m, &request(opcode::FORGET, abi::FILE_ID, &[0u8; 8]), &mut buf), Answer::Nothing);
    }
}
