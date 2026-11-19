//! The face on Linux: /dev/fuse read by our own threads, the loop device
//! configured by its ioctls, and ext4 made and mounted with every option
//! named here (docs/design.md, The face). Nothing in this file decides
//! anything; `serve.rs` answers each request.
//!
//! Every privileged call this crate makes is in this file: `mount`,
//! `umount2`, `mknod`, the loop device's ioctls, and the programs
//! `mke2fs` and `e2fsck` (docs/spike.md, Musts).

use std::ffi::CString;
use std::fs::{File as StdFile, OpenOptions};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::limits;
use crate::serve::{answer, Answer, Backing, BackingError, File};

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("{what}: {source}")]
    Os { what: String, source: std::io::Error },
    #[error("{path} is missing")]
    NodeMissing { path: PathBuf },
    #[error("{program} exited {status}: {stderr}")]
    Command { program: String, status: String, stderr: String },
}

fn os(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> DeviceError {
    let what = what.into();
    move |source| DeviceError::Os { what, source }
}

fn last(what: impl Into<String>) -> DeviceError {
    DeviceError::Os { what: what.into(), source: std::io::Error::last_os_error() }
}

fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_encoded_bytes()).expect("a path has no NUL")
}

/// A backing on a plain local file: the probe's stand-in for the store
/// (named so in every result). Never-written ranges are holes.
pub struct LocalFile {
    file: StdFile,
}

impl LocalFile {
    /// Creates (or truncates) a sparse file of `size_bytes`.
    pub fn create(path: &Path, size_bytes: u64) -> Result<LocalFile, DeviceError> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).custom_flags(libc::O_CLOEXEC).open(path).map_err(os(format!("create {}", path.display())))?;
        file.set_len(size_bytes).map_err(os("size the backing"))?;
        Ok(LocalFile { file })
    }
}

impl Backing for LocalFile {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), BackingError> {
        self.file.read_exact_at(buf, offset).map_err(|e| BackingError(e.to_string()))
    }
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), BackingError> {
        self.file.write_all_at(data, offset).map_err(|e| BackingError(e.to_string()))
    }
    fn sync(&self) -> Result<(), BackingError> {
        self.file.sync_data().map_err(|e| BackingError(e.to_string()))
    }
    fn punch(&self, offset: u64, length: u64) -> Result<(), BackingError> {
        let (o, l) = (i64::try_from(offset).expect("bounded by the volume"), i64::try_from(length).expect("bounded by the volume"));
        // SAFETY: a plain syscall on a file we own.
        let r = unsafe { libc::fallocate(self.file.as_raw_fd(), libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, o, l) };
        if r == 0 {
            Ok(())
        } else {
            Err(BackingError(std::io::Error::last_os_error().to_string()))
        }
    }
}

/// Makes a device node if it is missing (a container's /dev may lack one
/// the kernel has). Answers whether it made it.
pub fn ensure_node(path: &Path, block: bool, major: u32, minor: u32) -> Result<bool, DeviceError> {
    if path.exists() {
        return Ok(false);
    }
    let kind = if block { libc::S_IFBLK } else { libc::S_IFCHR };
    // SAFETY: mknod with a valid C string.
    let r = unsafe { libc::mknod(cstr(path).as_ptr(), kind | 0o600, libc::makedev(major, minor)) };
    if r == 0 {
        Ok(true)
    } else {
        Err(last(format!("mknod {}", path.display())))
    }
}

/// One file served over FUSE at `target/disk`, by `FUSE_THREADS` threads.
pub struct FuseMount {
    target: PathBuf,
    threads: Vec<JoinHandle<()>>,
    /// Whether each thread could mark itself an I/O flusher.
    pub io_flusher: Vec<bool>,
}

/// `PR_SET_IO_FLUSHER` (Linux 5.6): a thread on the block layer's write
/// path is spared the reclaim that would wait on its own writes.
const PR_SET_IO_FLUSHER: libc::c_int = 57;
/// `FUSE_DEV_IOC_CLONE`: another descriptor on the same session, so
/// threads read requests in parallel.
// The request's type is the C library's (`unsigned long` in glibc, `int`
// in musl): `libc::Ioctl`.
const FUSE_DEV_IOC_CLONE: libc::Ioctl = 0x8004_e500u32 as libc::Ioctl;

fn open_fuse() -> Result<OwnedFd, DeviceError> {
    let f = OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC).open("/dev/fuse").map_err(os("open /dev/fuse"))?;
    Ok(f.into())
}

impl FuseMount {
    pub fn path(&self) -> PathBuf {
        self.target.join("disk")
    }

    /// Mounts `file` at `target` (an empty directory) and starts serving.
    pub fn mount(target: &Path, file: File, backing: Arc<dyn Backing>) -> Result<FuseMount, DeviceError> {
        let session = open_fuse()?;
        // Each option named: the root's mode, root as the owner, others
        // allowed (the loop device opens the file as the kernel), and
        // reads as large as writes.
        let data = format!("fd={},rootmode=40000,user_id=0,group_id=0,allow_other,max_read={}", session.as_raw_fd(), limits::FUSE_READ_BYTES_MAX);
        let (src, fstype, data) = (CString::new("fpac").unwrap(), CString::new("fuse.fpac").unwrap(), CString::new(data).unwrap());
        // SAFETY: mount with valid C strings.
        let r = unsafe { libc::mount(src.as_ptr(), cstr(target).as_ptr(), fstype.as_ptr(), libc::MS_NOSUID | libc::MS_NODEV, data.as_ptr().cast()) };
        if r != 0 {
            return Err(last(format!("mount fuse at {}", target.display())));
        }
        let mut fds = Vec::with_capacity(limits::FUSE_THREADS);
        for _ in 1..limits::FUSE_THREADS {
            let clone = open_fuse()?;
            let mut id = u32::try_from(session.as_raw_fd()).expect("a descriptor is small");
            // SAFETY: the ioctl reads one u32 from a pointer we own.
            let r = unsafe { libc::ioctl(clone.as_raw_fd(), FUSE_DEV_IOC_CLONE, &mut id as *mut u32) };
            if r != 0 {
                return Err(last("clone /dev/fuse"));
            }
            fds.push(clone);
        }
        fds.push(session);
        let (tx, rx) = std::sync::mpsc::channel();
        let threads = fds
            .into_iter()
            .map(|fd| {
                let (backing, tx) = (backing.clone(), tx.clone());
                std::thread::spawn(move || {
                    // SAFETY: prctl on the calling thread.
                    let flusher = unsafe { libc::prctl(PR_SET_IO_FLUSHER, 1, 0, 0, 0) } == 0;
                    tx.send(flusher).expect("the mount waits for this");
                    serve(fd, file, backing.as_ref());
                })
            })
            .collect::<Vec<_>>();
        let io_flusher = (0..threads.len()).map(|_| rx.recv().expect("each thread says")).collect();
        Ok(FuseMount { target: target.to_path_buf(), threads, io_flusher })
    }

    /// Unmounts and waits for the threads. The loop device over the file
    /// must be gone first; until it is the mount is busy, which is retried
    /// for at most a second.
    pub fn unmount(self) -> Result<(), DeviceError> {
        umount(&self.target, Duration::from_secs(1))?;
        for t in self.threads {
            t.join().expect("a serving thread ends when the session does");
        }
        Ok(())
    }
}

/// One thread's loop. Bounded by the session: once it is unmounted, every
/// read answers ENODEV and the loop ends.
fn serve(fd: OwnedFd, file: File, backing: &dyn Backing) {
    let mut message = vec![0u8; limits::FUSE_BUFFER_BYTES];
    let mut data = vec![0u8; limits::FUSE_READ_BYTES_MAX as usize];
    loop {
        // SAFETY: read into a buffer we own, of its own length.
        let n = unsafe { libc::read(fd.as_raw_fd(), message.as_mut_ptr().cast(), message.len()) };
        if n < 0 {
            match std::io::Error::last_os_error().raw_os_error() {
                // interrupted, or a request the kernel withdrew
                Some(libc::EINTR) | Some(libc::ENOENT) | Some(libc::EAGAIN) => continue,
                Some(libc::ENODEV) => return,
                e => panic!("reading /dev/fuse: errno {e:?}"),
            }
        }
        let n = usize::try_from(n).expect("non-negative");
        match answer(&file, backing, &message[..n], &mut data) {
            Answer::Reply(v) => send(fd.as_raw_fd(), &[&v]),
            Answer::Read { header, len } => send(fd.as_raw_fd(), &[&header, &data[..len]]),
            Answer::Nothing => {}
            Answer::Stop(v) => {
                send(fd.as_raw_fd(), &[&v]);
                return;
            }
        }
    }
}

fn send(fd: RawFd, parts: &[&[u8]]) {
    let iov: Vec<libc::iovec> = parts.iter().map(|p| libc::iovec { iov_base: p.as_ptr() as *mut libc::c_void, iov_len: p.len() }).collect();
    let total: usize = parts.iter().map(|p| p.len()).sum();
    // SAFETY: the iovecs point into slices that outlive the call.
    let n = unsafe { libc::writev(fd, iov.as_ptr(), i32::try_from(iov.len()).expect("few parts")) };
    if n < 0 {
        match std::io::Error::last_os_error().raw_os_error() {
            // the request was interrupted, or the session ended: no one
            // waits for this answer
            Some(libc::ENOENT) | Some(libc::ENODEV) => return,
            e => panic!("answering /dev/fuse: errno {e:?}"),
        }
    }
    assert_eq!(usize::try_from(n).expect("non-negative"), total, "an answer is written whole");
}

/// Unmounts `target`, retrying while it is busy for at most `wait`.
pub fn umount(target: &Path, wait: Duration) -> Result<(), DeviceError> {
    let t0 = Instant::now();
    loop {
        // SAFETY: umount2 with a valid C string.
        if unsafe { libc::umount2(cstr(target).as_ptr(), 0) } == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EBUSY) || t0.elapsed() > wait {
            return Err(DeviceError::Os { what: format!("umount {}", target.display()), source: e });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

const LOOP_CTL_GET_FREE: libc::Ioctl = 0x4C82;
const LOOP_CONFIGURE: libc::Ioctl = 0x4C0A;
const LOOP_CLR_FD: libc::Ioctl = 0x4C01;
const LOOP_GET_STATUS64: libc::Ioctl = 0x4C05;
const LO_FLAGS_AUTOCLEAR: u32 = 4;
const LO_FLAGS_DIRECT_IO: u32 = 16;
/// The loop device's block major.
pub const LOOP_MAJOR: u32 = 7;

#[repr(C)]
struct LoopInfo64 {
    lo_device: u64,
    lo_inode: u64,
    lo_rdevice: u64,
    lo_offset: u64,
    lo_sizelimit: u64,
    lo_number: u32,
    lo_encrypt_type: u32,
    lo_encrypt_key_size: u32,
    lo_flags: u32,
    lo_file_name: [u8; 64],
    lo_crypt_name: [u8; 64],
    lo_encrypt_key: [u8; 32],
    lo_init: [u64; 2],
}

#[repr(C)]
struct LoopConfig {
    fd: u32,
    block_size: u32,
    info: LoopInfo64,
    reserved: [u64; 8],
}

const _: () = assert!(std::mem::size_of::<LoopInfo64>() == 232);
const _: () = assert!(std::mem::size_of::<LoopConfig>() == 304);

fn zero_info() -> LoopInfo64 {
    LoopInfo64 { lo_device: 0, lo_inode: 0, lo_rdevice: 0, lo_offset: 0, lo_sizelimit: 0, lo_number: 0, lo_encrypt_type: 0, lo_encrypt_key_size: 0, lo_flags: 0, lo_file_name: [0; 64], lo_crypt_name: [0; 64], lo_encrypt_key: [0; 32], lo_init: [0; 2] }
}

/// A loop device over a file.
pub struct Loop {
    pub path: PathBuf,
    pub number: u32,
    /// Whether the kernel took direct I/O (it may refuse it silently).
    pub direct_io: bool,
    device: StdFile,
}

impl Loop {
    /// Takes a free loop device and points it at `backing` with direct
    /// I/O and `BLOCK_BYTES` blocks. `make_node`: make /dev/loopN if this
    /// container's /dev lacks it.
    pub fn attach(backing: &Path, make_node: bool) -> Result<Loop, DeviceError> {
        let control = OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC).open("/dev/loop-control").map_err(os("open /dev/loop-control"))?;
        let file = OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC).open(backing).map_err(os(format!("open {}", backing.display())))?;
        for _ in 0..limits::LOOP_ATTACH_TRIES_MAX {
            // SAFETY: an ioctl with no argument.
            let n = unsafe { libc::ioctl(control.as_raw_fd(), LOOP_CTL_GET_FREE) };
            if n < 0 {
                return Err(last("LOOP_CTL_GET_FREE"));
            }
            let number = u32::try_from(n).expect("non-negative");
            let path = PathBuf::from(format!("/dev/loop{number}"));
            if make_node {
                ensure_node(&path, true, LOOP_MAJOR, number)?;
            }
            if !path.exists() {
                return Err(DeviceError::NodeMissing { path });
            }
            let device = OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC).open(&path).map_err(os(format!("open {}", path.display())))?;
            let mut info = zero_info();
            info.lo_flags = LO_FLAGS_DIRECT_IO | LO_FLAGS_AUTOCLEAR;
            let config = LoopConfig { fd: u32::try_from(file.as_raw_fd()).expect("small"), block_size: limits::BLOCK_BYTES, info, reserved: [0; 8] };
            // SAFETY: the ioctl reads a LoopConfig we own.
            let r = unsafe { libc::ioctl(device.as_raw_fd(), LOOP_CONFIGURE, &config as *const LoopConfig) };
            if r != 0 {
                let e = std::io::Error::last_os_error();
                // taken by someone else between the ask and the attach
                if e.raw_os_error() == Some(libc::EBUSY) {
                    continue;
                }
                return Err(DeviceError::Os { what: format!("LOOP_CONFIGURE {}", path.display()), source: e });
            }
            let mut got = zero_info();
            // SAFETY: the ioctl writes a LoopInfo64 we own.
            let r = unsafe { libc::ioctl(device.as_raw_fd(), LOOP_GET_STATUS64, &mut got as *mut LoopInfo64) };
            if r != 0 {
                return Err(last("LOOP_GET_STATUS64"));
            }
            assert_eq!(got.lo_number, number, "the device we configured");
            return Ok(Loop { path, number, direct_io: got.lo_flags & LO_FLAGS_DIRECT_IO != 0, device });
        }
        Err(DeviceError::Os { what: "a free loop device".into(), source: std::io::Error::from_raw_os_error(libc::EBUSY) })
    }

    /// Detaches now. The device must no longer be mounted.
    pub fn detach(self) -> Result<(), DeviceError> {
        // SAFETY: an ioctl with no argument on a device we hold.
        let r = unsafe { libc::ioctl(self.device.as_raw_fd(), LOOP_CLR_FD) };
        if r == 0 {
            Ok(())
        } else {
            Err(last(format!("LOOP_CLR_FD {}", self.path.display())))
        }
    }
}

fn program(names: &[&str]) -> PathBuf {
    for n in names {
        let p = Path::new(n);
        if p.exists() {
            return p.to_path_buf();
        }
    }
    PathBuf::from(names[0])
}

fn run(cmd: &mut Command) -> Result<String, DeviceError> {
    let out = cmd.output().map_err(os(format!("run {:?}", cmd.get_program())))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(DeviceError::Command { program: cmd.get_program().to_string_lossy().into_owned(), status: out.status.to_string(), stderr: String::from_utf8_lossy(&out.stderr).chars().take(2000).collect() })
    }
}

/// Makes ext4 on `device`. The device reads zeros where nothing was
/// written (the store's promise), so nothing is zeroed ahead: not the
/// inode tables, not the journal, and no discard pass over the whole
/// device. 4 KiB blocks, no reserved blocks (one owner), a label.
pub fn mkfs_ext4(device: &Path) -> Result<(), DeviceError> {
    run(Command::new(program(&["/usr/sbin/mke2fs", "/sbin/mke2fs"]))
        .args(["-t", "ext4", "-F", "-q", "-b", "4096", "-m", "0", "-L", "fpac"])
        .args(["-E", "nodiscard,lazy_itable_init=1,lazy_journal_init=1,assume_storage_prezeroed=1"])
        .arg(device))
    .map(|_| ())
}

/// `e2fsck -f -n`: a full check that changes nothing. Ok with its report
/// when clean; a `Command` error when it found anything.
pub fn fsck_ext4(device: &Path) -> Result<String, DeviceError> {
    run(Command::new(program(&["/usr/sbin/e2fsck", "/sbin/e2fsck"])).args(["-f", "-n"]).arg(device))
}

/// Mounts ext4 from `device` at `target`: no access times, the journal
/// committed every second (the flush's own cadence), and discards passed
/// down, so freed blocks are freed in the store too.
pub fn mount_ext4(device: &Path, target: &Path) -> Result<(), DeviceError> {
    let (fstype, data) = (CString::new("ext4").unwrap(), CString::new("commit=1,discard,noinit_itable").unwrap());
    // SAFETY: mount with valid C strings.
    let r = unsafe { libc::mount(cstr(device).as_ptr(), cstr(target).as_ptr(), fstype.as_ptr(), libc::MS_NOATIME, data.as_ptr().cast()) };
    if r == 0 {
        Ok(())
    } else {
        Err(last(format!("mount ext4 at {}", target.display())))
    }
}

/// `FIFREEZE`: the file system at a mount point flushes everything and
/// stops taking writes, so the device under it holds a consistent file
/// system even while files there stay open.
const FIFREEZE: libc::Ioctl = 0xC004_5877u32 as libc::Ioctl;

/// Freezes the file system mounted at `dir` (see FIFREEZE).
pub fn freeze(dir: &Path) -> Result<(), DeviceError> {
    let d = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC).open(dir).map_err(os(format!("open {}", dir.display())))?;
    // SAFETY: an ioctl with an int argument on a descriptor we own.
    let r = unsafe { libc::ioctl(d.as_raw_fd(), FIFREEZE, 0) };
    if r == 0 {
        Ok(())
    } else {
        Err(last(format!("FIFREEZE {}", dir.display())))
    }
}

/// `statfs`'s type for `path`: 0xEF53 is ext4.
pub fn fs_type(path: &Path) -> Result<u64, DeviceError> {
    // SAFETY: statfs into a zeroed struct we own.
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::statfs(cstr(path).as_ptr(), &mut s) };
    if r == 0 {
        Ok(s.f_type as u64)
    } else {
        Err(last(format!("statfs {}", path.display())))
    }
}

