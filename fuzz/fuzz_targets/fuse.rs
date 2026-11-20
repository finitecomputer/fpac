//! A request from /dev/fuse, decoded and answered.
//!
//! Invariants:
//! - no input panics a decoder;
//! - a request every decoder accepts is answered without a panic (`answer`
//!   panics only on what the decoders refuse: a misread kernel ABI);
//! - an answer's header names its own length and the request's unique,
//!   with an error zero or negative; a read answers at most what it asked;
//! - the backing is never asked for a byte past the file (the backing
//!   here panics if it is).

#![no_main]

use std::sync::Mutex;

use fpac_device::abi::{self, opcode};
use fpac_device::limits;
use fpac_device::serve::{answer, Answer, Backing, BackingError, File};
use libfuzzer_sys::fuzz_target;

const FILE_BYTES: u64 = 1 << 20;

struct Memory(Mutex<Vec<u8>>);

impl Memory {
    fn range(&self, offset: u64, len: u64) -> std::ops::Range<usize> {
        let end = offset.checked_add(len).expect("a range that does not wrap");
        assert!(end <= FILE_BYTES, "the backing asked past the file: {offset}+{len}");
        offset as usize..end as usize
    }
}

impl Backing for Memory {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), BackingError> {
        let r = self.range(offset, buf.len() as u64);
        buf.copy_from_slice(&self.0.lock().unwrap()[r]);
        Ok(())
    }
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), BackingError> {
        let r = self.range(offset, data.len() as u64);
        self.0.lock().unwrap()[r].copy_from_slice(data);
        Ok(())
    }
    fn sync(&self) -> Result<(), BackingError> {
        Ok(())
    }
    fn punch(&self, offset: u64, length: u64) -> Result<(), BackingError> {
        let r = self.range(offset, length);
        self.0.lock().unwrap()[r].fill(0);
        Ok(())
    }
}

fn header(v: &[u8]) -> (u32, i32, u64) {
    (u32::from_ne_bytes(v[0..4].try_into().unwrap()), i32::from_ne_bytes(v[4..8].try_into().unwrap()), u64::from_ne_bytes(v[8..16].try_into().unwrap()))
}

fuzz_target!(|data: &[u8]| {
    let mut m = data.to_vec();
    // most inputs say their own length, as the kernel's do
    if m.len() >= abi::IN_HEADER_BYTES && m[0] & 1 == 1 {
        let len = m.len() as u32;
        m[..4].copy_from_slice(&len.to_ne_bytes());
    }
    let Ok(h) = abi::in_header(&m) else { return };
    let decodes = match h.opcode {
        opcode::INIT => abi::init_in(&m).is_ok(),
        opcode::LOOKUP => abi::lookup_name(&m).is_ok(),
        opcode::SETATTR => abi::setattr_in(&m).is_ok(),
        opcode::READ => abi::read_in(&m).is_ok(),
        opcode::WRITE => abi::write_in(&m).is_ok(),
        opcode::FALLOCATE => abi::fallocate_in(&m).is_ok(),
        _ => true,
    };
    if !decodes {
        return;
    }
    let file = File::new(FILE_BYTES);
    let backing = Memory(Mutex::new(vec![0u8; FILE_BYTES as usize]));
    let mut buf = vec![0u8; limits::FUSE_READ_BYTES_MAX as usize];
    match answer(&file, &backing, &m, &mut buf) {
        Answer::Reply(v) | Answer::Stop(v) => {
            let (len, error, unique) = header(&v);
            assert_eq!(len as usize, v.len(), "the header names the answer's length");
            assert_eq!(unique, h.unique);
            assert!(error <= 0);
        }
        Answer::Read { header: hd, len } => {
            let (n, error, unique) = header(&hd);
            assert_eq!(n as usize, abi::OUT_HEADER_BYTES + len);
            assert_eq!((error, unique), (0, h.unique));
            let asked = abi::read_in(&m).expect("a read").size as usize;
            assert!(len <= asked && len <= buf.len());
        }
        Answer::Nothing => assert!(matches!(h.opcode, opcode::FORGET | opcode::BATCH_FORGET | opcode::INTERRUPT)),
    }
});
