//! `fpac`: the disk's daemon in the guest (docs/design.md). Linux only:
//! the face is FUSE, the loop device and ext4 (`daemon.rs`).

#[cfg(target_os = "linux")]
mod daemon;
#[cfg(target_os = "linux")]
mod disk;
#[cfg(target_os = "linux")]
mod flush;

#[cfg(target_os = "linux")]
fn main() {
    daemon::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("fpac runs in a Linux guest");
    std::process::exit(2);
}
