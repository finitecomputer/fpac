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
