//! `fpac-device`: the disk's face. One file, `disk`, served over FUSE by
//! our own threads reading /dev/fuse; a loop device over it with direct
//! I/O; ext4 on the loop device (docs/design.md, The face; docs/library.md,
//! why /dev/fuse by hand). The protocol (`abi`, `serve`) is pure and
//! tested on any host; what touches the kernel is in `linux`, built on
//! Linux only.

pub mod abi;
pub mod limits;
pub mod serve;

#[cfg(target_os = "linux")]
pub mod linux;
