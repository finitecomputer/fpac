//! The head: its wire form, and the rules the platform moves it by
//! (crates/store/src/head.rs, the Worker's `decide` line for line).
//!
//! Invariants:
//! - the wire form has one spelling: whatever decodes encodes back to the
//!   same string (raw input, and one-character edits of real heads), and
//!   every head within the limits round-trips;
//! - over any sequence of lives and posts, the head is only ever moved by
//!   the running life; within a life its checkpoint only goes forward and
//!   is never dropped; the same checkpoint may gain the clean mark and
//!   never lose it; a post of the head itself is Same; a refusal changes
//!   nothing.

#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fpac_remote::limits::HEAD_NUMBER_MAX;
use fpac_store::head::{decide, Decision};
use fpac_store::{CheckpointRef, Head};
use libfuzzer_sys::fuzz_target;

#[derive(Debug, Arbitrary)]
enum Op {
    /// A new life starts (the platform mints a later epoch).
    NewLife(u8),
    /// A post: epochs and seqs from a small range around the running
    /// life, so the interesting cases come up.
    Post { epoch_back: u8, ck: Option<(u8, u8, bool)>, clean: bool },
}

fn sum(alt: bool) -> [u8; 32] {
    if alt { [0xbb; 32] } else { [0xaa; 32] }
}

/// One character of a head's wire form inserted, deleted or replaced:
/// whatever still decodes must encode back to the edited string, so no
/// second spelling (a sign, a leading zero, uppercase hex) is accepted.
fn respelled(s: &str, u: &mut Unstructured<'_>) {
    const CHARS: &[u8] = b" +-0123456789abcdefABCDEF_clean";
    let mut b = s.as_bytes().to_vec();
    let (Ok(at), Ok(c), Ok(how)) = (u.int_in_range(0..=b.len()), u.choose(CHARS), u.int_in_range(0..=2u8)) else { return };
    match how {
        0 => b.insert(at, *c),
        1 if at < b.len() => drop(b.remove(at)),
        _ if at < b.len() => b[at] = *c,
        _ => return,
    }
    let edited = String::from_utf8(b).expect("ascii");
    if let Some(h) = Head::decode(&edited) {
        assert_eq!(h.encode(), edited, "a head has one spelling");
    }
}

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        if let Some(h) = Head::decode(s) {
            assert_eq!(h.encode(), s, "a head has one spelling");
        }
    }
    let mut u = Unstructured::new(data);
    let mut running = 1u64;
    let mut head: Option<Head> = None;
    let Ok(ops) = u.arbitrary::<Vec<Op>>() else { return };
    for op in ops.into_iter().take(64) {
        match op {
            Op::NewLife(n) => running += 1 + u64::from(n % 3),
            Op::Post { epoch_back, ck, clean } => {
                let epoch = running.saturating_sub(u64::from(epoch_back % 3)).max(1) + u64::from(epoch_back == 255);
                let checkpoint = ck.map(|(e, s, alt)| CheckpointRef { epoch: epoch.saturating_sub(u64::from(e % 3)).max(1) + u64::from(e == 255), seq: 1 + u64::from(s % 4), sum: sum(alt) });
                let p = Head { epoch, checkpoint, clean };
                assert!(epoch <= HEAD_NUMBER_MAX);
                assert_eq!(Head::decode(&p.encode()), Some(p), "a head round-trips");
                respelled(&p.encode(), &mut u);
                let before = head;
                match decide(head.as_ref(), running, &p) {
                    Decision::Take => {
                        assert_eq!(p.epoch, running, "only the running life moves the head");
                        assert!(p.checkpoint.is_none_or(|c| c.epoch <= p.epoch));
                        if let Some(c) = before.filter(|c| c.epoch == p.epoch) {
                            match (c.checkpoint, p.checkpoint) {
                                (None, None) => assert!(p.clean),
                                (Some(_), None) => panic!("a life's head lost its checkpoint"),
                                (None, Some(q)) => assert_eq!(q.epoch, p.epoch),
                                (Some(a), Some(b)) if a == b => assert!(p.clean && !c.clean, "only the clean mark moves on the same checkpoint"),
                                (Some(a), Some(b)) => {
                                    assert_eq!(b.epoch, p.epoch, "within a life, its own checkpoints only");
                                    assert!((b.epoch, b.seq) > (a.epoch, a.seq), "a checkpoint only goes forward");
                                }
                            }
                        }
                        head = Some(p);
                        assert_eq!(decide(head.as_ref(), running, &p), Decision::Same, "the head posted again is Same");
                    }
                    Decision::Same => assert_eq!(before, Some(p)),
                    Decision::Refuse(_) => assert_eq!(head, before),
                }
            }
        }
    }
});
