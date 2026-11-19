//! The folder remote passes the conformance suite, and its faults behave
//! as the contract's client expects (docs/contract.md).

use std::net::TcpListener;

use fpac_remote::{Put, Remote};
use fpac_testkit::conformance::{noise, run};
use fpac_testkit::folder::{Folder, Listen};

fn scratch(name: &str) -> std::path::PathBuf {
    let base = std::env::var_os("TMPDIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let d = base.join(format!("fpac-testkit-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn start(name: &str) -> (fpac_testkit::folder::Running, Remote) {
    let running = Folder::new(&scratch(name), "disk-1", 7).start(Listen::Tcp(TcpListener::bind("127.0.0.1:0").unwrap()));
    let remote = Remote::new("127.0.0.1", running.port.unwrap());
    (running, remote)
}

// Goal: the folder remote answers every case of the suite. Method: run
// it through the product's client.
#[test]
fn folder_remote_conforms() {
    let (_r, mut remote) = start("conform");
    let cases = run(&mut remote, "t1");
    for c in &cases {
        assert!(c.pass, "{}: {}", c.name, c.detail);
    }
    assert_eq!(cases.len(), 12);
    assert_eq!(remote.connects, 1, "one kept-alive connection");
}

// Goal: a put whose answer is lost, sent again, is recognized as our own
// (Same), never as another writer. Method: the remote drops the next
// put's answer; the client reconnects once and sends it again.
#[test]
fn a_lost_put_answer_replays_as_same() {
    let (r, mut remote) = start("lost");
    let body = noise(10_000, 3);
    assert_eq!(remote.put("e7/s1", &body).unwrap(), Put::Created);
    r.folder.faults.lose_put_answer.store(1, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(remote.put("e7/s2", &body).unwrap(), Put::Same, "the first try landed, its answer was lost, the retry sees our bytes");
    assert_eq!(remote.connects, 2);
}

// Goal: a remote that is down is an operating error the caller can wait
// out, not a refusal. Method: one 503, then the call again.
#[test]
fn unavailable_is_retryable() {
    let (r, mut remote) = start("down");
    r.folder.faults.unavailable.store(1, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(remote.put("e7/s1", b"x"), Err(fpac_remote::RemoteError::Unavailable(_))));
    assert_eq!(remote.put("e7/s1", b"x").unwrap(), Put::Created);
}

// Goal: an answer cut short mid-body is an error, never bytes: neither a
// short read nor the rest filled from anything else. Method: the remote
// sends half of a ranged get's body and closes; the call must fail, and
// the next one must answer right on a new connection.
#[test]
fn a_cut_body_is_an_error_not_bytes() {
    let (r, mut remote) = start("cut");
    let body = noise(200_000, 4);
    assert_eq!(remote.put("e7/s1", &body).unwrap(), Put::Created);
    r.folder.faults.cut_get_body.store(1, std::sync::atomic::Ordering::SeqCst);
    let mut buf = vec![0u8; 65536];
    assert!(matches!(remote.get_range("e7/s1", 4096, &mut buf), Err(fpac_remote::RemoteError::Unavailable(_))));
    assert_eq!(remote.get_range("e7/s1", 4096, &mut buf).unwrap(), Some(65536));
    assert_eq!(buf[..], body[4096..4096 + 65536]);
}

// Goal: the volume check reads right through a remote that answers right
// (so a wrong read it reports elsewhere is the remote's, not its own).
// Method: 2,000 reads on 8 connections against the folder remote, both
// patterns; every one right, none failed.
#[test]
fn rangecheck_against_a_right_remote_finds_nothing() {
    use fpac_testkit::rangecheck::{run, Check, Dial, Mode, Pattern, ReadWrite};
    struct Tcp(u16);
    impl Dial for Tcp {
        fn dial(&self) -> std::io::Result<Box<dyn ReadWrite>> {
            Ok(Box::new(std::net::TcpStream::connect(("127.0.0.1", self.0))?))
        }
        fn host(&self) -> &str {
            "127.0.0.1"
        }
        fn headers(&self, _: &str, _: &str) -> Vec<(String, String)> {
            Vec::new()
        }
    }
    let (r, mut remote) = start("rangecheck");
    let object = noise(8 << 20, 6);
    assert_eq!(remote.put("e7/obj", &object).unwrap(), Put::Created);
    for pattern in [Pattern::Aligned, Pattern::Unaligned] {
        let check = Check { path: "/o/e7/obj", object: &object, reads: 1000, threads: 8, mode: Mode::Stream, pattern, size_max: 256 << 10, seed: 9 };
        let o = run(&check, &Tcp(r.port.unwrap()), &|_, _, _| {});
        assert_eq!((o.reads, o.right, o.wrong, o.failed), (1000, 1000, 0, 0), "{pattern:?}: {:?}", o.failures);
    }
}
