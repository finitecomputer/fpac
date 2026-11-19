//! The conformance suite (docs/contract.md, Conformance): what every
//! remote must answer, run through the product's own client. It runs
//! against the folder remote in this workspace's tests, and against the
//! deployed stand-in Worker from inside a guest (`fpac-probe
//! conformance`), so both are held to one list.

use fpac_remote::{sha256, CheckpointRef, Head, HeadAnswer, Put, Remote, RemoteError};

pub struct Case {
    pub name: &'static str,
    pub pass: bool,
    pub detail: String,
}

fn case(name: &'static str, pass: bool, detail: impl Into<String>) -> Case {
    Case { name, pass, detail: detail.into() }
}

/// Bytes that don't compress and differ by `seed`.
pub fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut v = Vec::with_capacity(len);
    // Bounded by len.
    while v.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes()[..(len - v.len()).min(8)]);
    }
    v
}

/// Runs every case under names that start with `tag` (unique per run).
pub fn run(remote: &mut Remote, tag: &str) -> Vec<Case> {
    let mut out = Vec::new();
    let open = remote.open();
    let epoch = match &open {
        Ok(o) => o.epoch,
        Err(_) => 0,
    };
    out.push(case("open names the disk, this life's epoch, the volume and the head", open.as_ref().is_ok_and(|o| !o.disk.is_empty() && o.epoch > 0 && o.volume_bytes > 0), format!("{open:?}")));
    if epoch == 0 {
        return out;
    }
    let name = format!("e{epoch}/{tag}-a");
    let first = noise(100_000, 1);
    let other = noise(100_000, 2);
    let r = remote.put(&name, &first);
    out.push(case("a put of a new name is created", matches!(r, Ok(Put::Created)), format!("{r:?}")));
    let r = remote.put(&name, &first);
    out.push(case("the same put again is the same (a lost answer, replayed)", matches!(r, Ok(Put::Same)), format!("{r:?}")));
    let r = remote.put(&name, &other);
    out.push(case("other bytes at that name conflict, naming the stored checksum", r.as_ref().is_ok_and(|p| *p == Put::Conflict { sha256: sha256(&first) }), format!("{r:?}")));
    let mut whole = Vec::new();
    let r = remote.get(&name, &mut |b| whole.extend_from_slice(b));
    out.push(case("a get streams the whole object, its checksum checked", matches!(r, Ok(Some(100_000))) && whole == first, format!("{r:?}")));
    let mut buf = vec![0u8; 4096];
    let r = remote.get_range(&name, 50_000, &mut buf);
    out.push(case("a ranged get answers exactly the range", matches!(r, Ok(Some(4096))) && buf[..] == first[50_000..54_096], format!("{r:?}")));
    let r = remote.get_range(&name, 98_000, &mut buf);
    out.push(case("a range past the end answers the tail", matches!(r, Ok(Some(2000))) && buf[..2000] == first[98_000..], format!("{r:?}")));
    let r = remote.get_range(&name, 200_000, &mut buf);
    out.push(case("a range starting past the end is refused", matches!(r, Err(RemoteError::Refused { status: 416, .. })), format!("{r:?}")));
    let r = remote.get_range(&format!("e{epoch}/{tag}-absent"), 0, &mut buf);
    out.push(case("an absent name reads as absent", matches!(r, Ok(None)), format!("{r:?}")));
    let mut none = 0u64;
    let r = remote.get(&format!("e{epoch}/{tag}-absent"), &mut |b| none += b.len() as u64);
    out.push(case("an absent name streams as absent", matches!(r, Ok(None)) && none == 0, format!("{r:?}")));
    let r = remote.put(&format!("e{}/{tag}-b", epoch + 1), &first);
    out.push(case("a put into another life's epoch is refused", matches!(r, Err(RemoteError::Refused { status: 403, .. })), format!("{r:?}")));
    let r = remote.put(&format!("e{epoch}/{tag}-empty"), &[]);
    let mut empty = Vec::new();
    let g = remote.get(&format!("e{epoch}/{tag}-empty"), &mut |b| empty.extend_from_slice(b));
    out.push(case("an empty object round-trips", matches!(r, Ok(Put::Created)) && matches!(g, Ok(Some(0))), format!("{r:?} {g:?}")));
    // The head's rules, on a disk this suite owns (a new one, or one whose
    // head is older than this life).
    let base = open.as_ref().ok().and_then(|o| o.head);
    let mine = Head { epoch, checkpoint: base.and_then(|h| h.checkpoint) };
    let r = remote.post_head(&mine);
    out.push(case("this life's head is taken", matches!(r, Ok(HeadAnswer::Taken)), format!("{r:?}")));
    let r = remote.post_head(&mine);
    out.push(case("the same head again is the same (a lost answer, replayed)", matches!(r, Ok(HeadAnswer::Same)), format!("{r:?}")));
    let later = Head { epoch, checkpoint: Some(CheckpointRef { epoch, seq: 1, sum: sha256(tag.as_bytes()) }) };
    let r = remote.post_head(&later);
    out.push(case("a later checkpoint of this life is taken", matches!(r, Ok(HeadAnswer::Taken)), format!("{r:?}")));
    let conflicting = Head { epoch, checkpoint: Some(CheckpointRef { epoch, seq: 1, sum: sha256(b"another body") }) };
    let r = remote.post_head(&conflicting);
    out.push(case("the same checkpoint with another body is refused", matches!(r, Err(RemoteError::Refused { status: 409, .. })), format!("{r:?}")));
    let other = Head { epoch: epoch + 1, checkpoint: None };
    let r = remote.post_head(&other);
    out.push(case("a head from a life that isn't running is refused", matches!(r, Err(RemoteError::Refused { status: 409, .. })), format!("{r:?}")));
    out
}
