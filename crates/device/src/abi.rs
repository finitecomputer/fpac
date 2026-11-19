//! The FUSE kernel protocol, as much of it as one file needs
//! (`include/uapi/linux/fuse.h`, protocol 7.31 and later). Every message is
//! the kernel's native byte order; the targets we build for are
//! little-endian, asserted below. Decoding refuses a short or oversized
//! message as a value, never by reading past it: what arrives on
//! /dev/fuse is input from the kernel, but a mismatch would mean we read
//! the ABI wrong, so the server treats it as corruption and stops.

/// The protocol's major version; the kernel refuses any other.
pub const KERNEL_VERSION: u32 = 7;
/// The least minor version we speak: 7.31 brought `max_pages` (writes of
/// more than 128 KiB) and is in every kernel we target (5.4 and later).
pub const KERNEL_MINOR_MIN: u32 = 31;
/// The minor version we answer with (the kernel takes the lesser of its
/// own and ours).
pub const KERNEL_MINOR: u32 = 31;

const _: () = assert!(cfg!(target_endian = "little"), "the FUSE protocol is the kernel's byte order; only little-endian targets are built");

pub const ROOT_ID: u64 = 1;
/// The one file's node id.
pub const FILE_ID: u64 = 2;
/// The one file's name in the root.
pub const FILE_NAME: &[u8] = b"disk";
/// The one file handle we hand out: every open is the same file.
pub const FILE_HANDLE: u64 = 1;

pub mod opcode {
    pub const LOOKUP: u32 = 1;
    pub const FORGET: u32 = 2;
    pub const GETATTR: u32 = 3;
    pub const SETATTR: u32 = 4;
    pub const OPEN: u32 = 14;
    pub const READ: u32 = 15;
    pub const WRITE: u32 = 16;
    pub const STATFS: u32 = 17;
    pub const RELEASE: u32 = 18;
    pub const FSYNC: u32 = 20;
    pub const FLUSH: u32 = 25;
    pub const INIT: u32 = 26;
    pub const INTERRUPT: u32 = 36;
    pub const DESTROY: u32 = 38;
    pub const BATCH_FORGET: u32 = 42;
    pub const FALLOCATE: u32 = 43;
}

pub mod init_flag {
    /// Reads may be in flight at once.
    pub const ASYNC_READ: u32 = 1 << 0;
    /// Writes larger than a page.
    pub const BIG_WRITES: u32 = 1 << 5;
    /// The kernel may send direct I/O asynchronously (the loop device's).
    pub const ASYNC_DIO: u32 = 1 << 15;
    /// `max_pages` in the answer is honored.
    pub const MAX_PAGES: u32 = 1 << 22;
}

pub mod open_flag {
    /// Bypass the page cache for this file: ext4 above it has its own, and
    /// a second copy of every block in FUSE's would be memory spent twice.
    pub const DIRECT_IO: u32 = 1 << 0;
    /// Direct writes may run in parallel (the loop device issues many).
    pub const PARALLEL_DIRECT_WRITES: u32 = 1 << 6;
}

pub mod fallocate_mode {
    pub const KEEP_SIZE: u32 = 0x01;
    pub const PUNCH_HOLE: u32 = 0x02;
    pub const ZERO_RANGE: u32 = 0x10;
}

/// `setattr`'s `valid` bit for a size change.
pub const FATTR_SIZE: u32 = 1 << 3;

pub const IN_HEADER_BYTES: usize = 40;
pub const OUT_HEADER_BYTES: usize = 16;
pub const ATTR_BYTES: usize = 88;
pub const ENTRY_OUT_BYTES: usize = 40 + ATTR_BYTES;
pub const ATTR_OUT_BYTES: usize = 16 + ATTR_BYTES;
pub const INIT_IN_BYTES_MIN: usize = 16;
pub const INIT_OUT_BYTES: usize = 64;
pub const OPEN_OUT_BYTES: usize = 16;
pub const READ_IN_BYTES: usize = 40;
pub const WRITE_IN_BYTES: usize = 40;
pub const WRITE_OUT_BYTES: usize = 8;
pub const FSYNC_IN_BYTES: usize = 16;
pub const FALLOCATE_IN_BYTES: usize = 32;
pub const SETATTR_IN_BYTES: usize = 88;
pub const STATFS_OUT_BYTES: usize = 80;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct InHeader {
    pub len: u32,
    pub opcode: u32,
    pub unique: u64,
    pub nodeid: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AbiError {
    #[error("a message of {got} bytes is shorter than its {need}-byte header or body")]
    Short { got: usize, need: usize },
    #[error("a message says it is {said} bytes, but {got} arrived")]
    Length { said: u32, got: usize },
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut x = [0u8; 8];
    x.copy_from_slice(&b[at..at + 8]);
    u64::from_ne_bytes(x)
}

/// The header of one request, checked against the bytes that arrived.
pub fn in_header(message: &[u8]) -> Result<InHeader, AbiError> {
    if message.len() < IN_HEADER_BYTES {
        return Err(AbiError::Short { got: message.len(), need: IN_HEADER_BYTES });
    }
    let h = InHeader { len: u32_at(message, 0), opcode: u32_at(message, 4), unique: u64_at(message, 8), nodeid: u64_at(message, 16) };
    if h.len as usize == message.len() {
        Ok(h)
    } else {
        Err(AbiError::Length { said: h.len, got: message.len() })
    }
}

fn body(message: &[u8], need: usize) -> Result<&[u8], AbiError> {
    let b = &message[IN_HEADER_BYTES..];
    if b.len() < need {
        return Err(AbiError::Short { got: b.len(), need });
    }
    Ok(b)
}

#[derive(Debug, PartialEq, Eq)]
pub struct InitIn {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
}

pub fn init_in(message: &[u8]) -> Result<InitIn, AbiError> {
    let b = body(message, INIT_IN_BYTES_MIN)?;
    Ok(InitIn { major: u32_at(b, 0), minor: u32_at(b, 4), max_readahead: u32_at(b, 8), flags: u32_at(b, 12) })
}

#[derive(Debug, PartialEq, Eq)]
pub struct ReadIn {
    pub offset: u64,
    pub size: u32,
}

pub fn read_in(message: &[u8]) -> Result<ReadIn, AbiError> {
    let b = body(message, READ_IN_BYTES)?;
    Ok(ReadIn { offset: u64_at(b, 8), size: u32_at(b, 16) })
}

/// A write's offset and its data, borrowed from the request.
pub struct WriteIn<'a> {
    pub offset: u64,
    pub data: &'a [u8],
}

pub fn write_in(message: &[u8]) -> Result<WriteIn<'_>, AbiError> {
    let b = body(message, WRITE_IN_BYTES)?;
    let size = u32_at(b, 16) as usize;
    let data = &b[WRITE_IN_BYTES..];
    if data.len() == size {
        Ok(WriteIn { offset: u64_at(b, 8), data })
    } else {
        Err(AbiError::Short { got: data.len(), need: size })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct FallocateIn {
    pub offset: u64,
    pub length: u64,
    pub mode: u32,
}

pub fn fallocate_in(message: &[u8]) -> Result<FallocateIn, AbiError> {
    let b = body(message, FALLOCATE_IN_BYTES)?;
    Ok(FallocateIn { offset: u64_at(b, 8), length: u64_at(b, 16), mode: u32_at(b, 24) })
}

#[derive(Debug, PartialEq, Eq)]
pub struct SetattrIn {
    pub valid: u32,
    pub size: u64,
}

pub fn setattr_in(message: &[u8]) -> Result<SetattrIn, AbiError> {
    let b = body(message, SETATTR_IN_BYTES)?;
    Ok(SetattrIn { valid: u32_at(b, 0), size: u64_at(b, 16) })
}

/// A lookup's name, without its terminating NUL.
pub fn lookup_name(message: &[u8]) -> Result<&[u8], AbiError> {
    let b = body(message, 1)?;
    match b.iter().position(|&c| c == 0) {
        Some(end) => Ok(&b[..end]),
        None => Err(AbiError::Short { got: b.len(), need: b.len() + 1 }),
    }
}

/// An answer's header: `len` covers the header and every byte after it.
pub fn out_header(len: usize, error: i32, unique: u64) -> [u8; OUT_HEADER_BYTES] {
    assert!(len >= OUT_HEADER_BYTES);
    assert!(error <= 0, "FUSE errors are negative errnos");
    let mut h = [0u8; OUT_HEADER_BYTES];
    h[0..4].copy_from_slice(&u32::try_from(len).expect("an answer fits u32").to_ne_bytes());
    h[4..8].copy_from_slice(&error.to_ne_bytes());
    h[8..16].copy_from_slice(&unique.to_ne_bytes());
    h
}

/// A node's attributes, as `fuse_attr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attr {
    pub ino: u64,
    pub size: u64,
    pub mode: u32,
    pub nlink: u32,
    pub blksize: u32,
}

impl Attr {
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&self.ino.to_ne_bytes());
        out.extend_from_slice(&self.size.to_ne_bytes());
        out.extend_from_slice(&self.size.div_ceil(512).to_ne_bytes()); // blocks
        out.extend_from_slice(&[0u8; 24]); // atime, mtime, ctime
        out.extend_from_slice(&[0u8; 12]); // their nanoseconds
        out.extend_from_slice(&self.mode.to_ne_bytes());
        out.extend_from_slice(&self.nlink.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes()); // uid: root's
        out.extend_from_slice(&0u32.to_ne_bytes()); // gid
        out.extend_from_slice(&0u32.to_ne_bytes()); // rdev
        out.extend_from_slice(&self.blksize.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes()); // flags
        assert_eq!(out.len() - start, ATTR_BYTES);
    }
}

/// How long the kernel may cache a lookup or attributes, in seconds: as
/// long as it likes, since nothing else changes this file system.
pub const VALID_S: u64 = 3600;

pub fn entry_out(attr: &Attr, out: &mut Vec<u8>) {
    out.extend_from_slice(&attr.ino.to_ne_bytes()); // nodeid
    out.extend_from_slice(&0u64.to_ne_bytes()); // generation
    out.extend_from_slice(&VALID_S.to_ne_bytes()); // entry_valid
    out.extend_from_slice(&VALID_S.to_ne_bytes()); // attr_valid
    out.extend_from_slice(&[0u8; 8]); // their nanoseconds
    attr.encode(out);
}

pub fn attr_out(attr: &Attr, out: &mut Vec<u8>) {
    out.extend_from_slice(&VALID_S.to_ne_bytes());
    out.extend_from_slice(&[0u8; 8]); // nanoseconds, dummy
    attr.encode(out);
}

pub struct InitOut {
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
    pub max_background: u16,
    pub max_write: u32,
    pub max_pages: u16,
}

pub fn init_out(i: &InitOut, out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(&KERNEL_VERSION.to_ne_bytes());
    out.extend_from_slice(&i.minor.to_ne_bytes());
    out.extend_from_slice(&i.max_readahead.to_ne_bytes());
    out.extend_from_slice(&i.flags.to_ne_bytes());
    out.extend_from_slice(&i.max_background.to_ne_bytes());
    out.extend_from_slice(&i.max_background.to_ne_bytes()); // congestion_threshold
    out.extend_from_slice(&i.max_write.to_ne_bytes());
    out.extend_from_slice(&1u32.to_ne_bytes()); // time_gran: 1 ns
    out.extend_from_slice(&i.max_pages.to_ne_bytes());
    out.extend_from_slice(&0u16.to_ne_bytes()); // map_alignment
    out.resize(start + INIT_OUT_BYTES, 0); // flags2 and the unused tail
}

pub fn open_out(open_flags: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&FILE_HANDLE.to_ne_bytes());
    out.extend_from_slice(&open_flags.to_ne_bytes());
    out.extend_from_slice(&0u32.to_ne_bytes()); // backing_id
}

pub fn write_out(size: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&size.to_ne_bytes());
    out.extend_from_slice(&0u32.to_ne_bytes());
}

pub fn statfs_out(blocks: u64, bsize: u32, out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(&blocks.to_ne_bytes());
    out.extend_from_slice(&0u64.to_ne_bytes()); // bfree: the file is the space
    out.extend_from_slice(&0u64.to_ne_bytes()); // bavail
    out.extend_from_slice(&2u64.to_ne_bytes()); // files
    out.extend_from_slice(&0u64.to_ne_bytes()); // ffree
    out.extend_from_slice(&bsize.to_ne_bytes());
    out.extend_from_slice(&255u32.to_ne_bytes()); // namelen
    out.extend_from_slice(&bsize.to_ne_bytes()); // frsize
    out.resize(start + STATFS_OUT_BYTES, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(opcode: u32, nodeid: u64, body: &[u8]) -> Vec<u8> {
        let len = (IN_HEADER_BYTES + body.len()) as u32;
        let mut m = Vec::new();
        m.extend_from_slice(&len.to_ne_bytes());
        m.extend_from_slice(&opcode.to_ne_bytes());
        m.extend_from_slice(&7u64.to_ne_bytes());
        m.extend_from_slice(&nodeid.to_ne_bytes());
        m.extend_from_slice(&[0u8; 16]);
        m.extend_from_slice(body);
        m
    }

    // Goal: the header decodes, and a length that disagrees with the bytes
    // that arrived is refused. Method: one good request, then the same
    // request cut short and with a byte too many.
    #[test]
    fn header_checks_its_length() {
        let m = request(opcode::GETATTR, ROOT_ID, &[0u8; 16]);
        assert_eq!(in_header(&m).unwrap(), InHeader { len: 56, opcode: opcode::GETATTR, unique: 7, nodeid: ROOT_ID });
        assert_eq!(in_header(&m[..55]), Err(AbiError::Length { said: 56, got: 55 }));
        let mut long = m.clone();
        long.push(0);
        assert_eq!(in_header(&long), Err(AbiError::Length { said: 56, got: 57 }));
        assert_eq!(in_header(&m[..39]), Err(AbiError::Short { got: 39, need: IN_HEADER_BYTES }));
    }

    // Goal: a write's data is exactly the size it names. Method: a good
    // write, then one whose size says more than came.
    #[test]
    fn write_data_matches_its_size() {
        let mut b = vec![0u8; WRITE_IN_BYTES];
        b[8..16].copy_from_slice(&4096u64.to_ne_bytes());
        b[16..20].copy_from_slice(&3u32.to_ne_bytes());
        b.extend_from_slice(b"abc");
        let m = request(opcode::WRITE, FILE_ID, &b);
        let w = write_in(&m).unwrap();
        assert_eq!((w.offset, w.data), (4096, &b"abc"[..]));
        b[16..20].copy_from_slice(&4u32.to_ne_bytes());
        let m = request(opcode::WRITE, FILE_ID, &b);
        assert!(matches!(write_in(&m), Err(AbiError::Short { got: 3, need: 4 })));
    }

    // Goal: every answer we build is the size the kernel's struct is.
    // Method: encode each and count.
    #[test]
    fn answers_have_the_kernels_sizes() {
        let a = Attr { ino: FILE_ID, size: 1 << 30, mode: 0o100600, nlink: 1, blksize: 4096 };
        let mut v = Vec::new();
        entry_out(&a, &mut v);
        assert_eq!(v.len(), ENTRY_OUT_BYTES);
        v.clear();
        attr_out(&a, &mut v);
        assert_eq!(v.len(), ATTR_OUT_BYTES);
        v.clear();
        init_out(&InitOut { minor: 31, max_readahead: 0, flags: 0, max_background: 16, max_write: 1 << 20, max_pages: 256 }, &mut v);
        assert_eq!(v.len(), INIT_OUT_BYTES);
        v.clear();
        open_out(0, &mut v);
        assert_eq!(v.len(), OPEN_OUT_BYTES);
        v.clear();
        statfs_out(1, 4096, &mut v);
        assert_eq!(v.len(), STATFS_OUT_BYTES);
        assert_eq!(out_header(16, -2, 9)[4..8], (-2i32).to_ne_bytes());
    }

    // Goal: a lookup's name stops at its NUL, and one with no NUL is
    // refused. Method: both.
    #[test]
    fn lookup_name_needs_its_nul() {
        let m = request(opcode::LOOKUP, ROOT_ID, b"disk\0");
        assert_eq!(lookup_name(&m).unwrap(), b"disk");
        let m = request(opcode::LOOKUP, ROOT_ID, b"disk");
        assert!(lookup_name(&m).is_err());
    }
}

/// The errnos FUSE answers carry, as Linux numbers them: the guest is
/// Linux whatever host builds this, and macOS numbers some differently.
pub mod errno {
    pub const EPERM: i32 = 1;
    pub const ENOENT: i32 = 2;
    pub const EIO: i32 = 5;
    pub const EISDIR: i32 = 21;
    pub const EINVAL: i32 = 22;
    pub const EFBIG: i32 = 27;
    pub const ENOSYS: i32 = 38;
    pub const EPROTO: i32 = 71;
    pub const EOPNOTSUPP: i32 = 95;
}
