//! The store's gates to the physical world (docs/design.md, Testing):
//! the remote and the cache file. The clock is a number the caller passes
//! in. Everything behind them is pure, and the simulator shims both.

use fpac_remote::{Put, RemoteError};

/// The remote, as the contract has it (docs/contract.md).
pub trait RemoteGate {
    /// `buf.len()` bytes of `name` from `offset`: the bytes read (fewer
    /// only at the object's end), or None when it is absent.
    fn get_range(&mut self, name: &str, offset: u64, buf: &mut [u8]) -> Result<Option<usize>, RemoteError>;
    /// All of `name`, checked against the checksum stored with it.
    fn get(&mut self, name: &str, sink: &mut dyn FnMut(&[u8])) -> Result<Option<u64>, RemoteError>;
    /// Put-if-absent.
    fn put(&mut self, name: &str, body: &[u8]) -> Result<Put, RemoteError>;
    /// The open's next reads, named at once: a gate that can fetch in
    /// parallel may, and answer the calls that follow from what it
    /// fetched (each with the same checks as a call of its own). Every
    /// round trip an open waits for is about 100 ms on R2 (docs/spike.md,
    /// P5), so the open asks for its pages, its first segment headers and
    /// its pack's head in one round. A gate that can't ignores it.
    fn prefetch(&mut self, _asks: &[Ask]) {}
}

/// One read an open will make.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Ask {
    /// All of an object (`get`).
    Whole(String),
    /// `len` bytes from `offset` (`get_range`).
    Range { name: String, offset: u64, len: usize },
}

impl RemoteGate for fpac_remote::Remote {
    fn get_range(&mut self, name: &str, offset: u64, buf: &mut [u8]) -> Result<Option<usize>, RemoteError> {
        fpac_remote::Remote::get_range(self, name, offset, buf)
    }
    fn get(&mut self, name: &str, sink: &mut dyn FnMut(&[u8])) -> Result<Option<u64>, RemoteError> {
        fpac_remote::Remote::get(self, name, sink)
    }
    fn put(&mut self, name: &str, body: &[u8]) -> Result<Put, RemoteError> {
        fpac_remote::Remote::put(self, name, body)
    }
}

/// The cache file on the container's own disk: one extent's bytes at a
/// time, by its number. Its contents die with the container.
pub trait CacheGate {
    fn read(&mut self, extent: u64, buf: &mut [u8]) -> std::io::Result<()>;
    fn write(&mut self, extent: u64, bytes: &[u8]) -> std::io::Result<()>;
    /// The extent reads as zeros from now on.
    fn punch(&mut self, extent: u64) -> std::io::Result<()>;
}

/// A cache in memory: tests and the simulator.
#[derive(Default)]
pub struct MemCache {
    pub extents: std::collections::BTreeMap<u64, Vec<u8>>,
}

impl CacheGate for MemCache {
    fn read(&mut self, extent: u64, buf: &mut [u8]) -> std::io::Result<()> {
        match self.extents.get(&extent) {
            Some(b) => buf.copy_from_slice(b),
            None => buf.fill(0),
        }
        Ok(())
    }
    fn write(&mut self, extent: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.extents.insert(extent, bytes.to_vec());
        Ok(())
    }
    fn punch(&mut self, extent: u64) -> std::io::Result<()> {
        self.extents.remove(&extent);
        Ok(())
    }
}
