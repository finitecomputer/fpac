//! The store's objects as R2 hands them back: untrusted bytes.
//!
//! Invariants, for segments' and packs' headers, map pages and
//! checkpoints:
//! - no input panics a decoder;
//! - whatever a decoder accepts is canonical: encoding what it decoded
//!   gives back exactly the bytes it read (refuse or round-trip), so an
//!   object has one spelling and its seal names it;
//! - a sealed object changed anywhere its seal covers is refused.
//!
//! Raw bytes rarely pass a seal, so most runs build a valid object from
//! the input, change some of its bytes, and either leave its seal (which
//! must then refuse) or seal it again (which reaches the layout checks).

#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fpac_format::limits::{EXTENTS_MAX, EXTENT_BYTES, PAGES_MAX, PAGE_EXTENTS, SEGMENT_FIXED_BYTES};
use fpac_format::{self as f, Checkpoint, Image, Link, Loc, PackHeader, PackRef, PageRef, SegId};
use libfuzzer_sys::fuzz_target;

#[derive(Debug, Clone, Copy, Arbitrary)]
enum Kind {
    Segment,
    Page,
    Checkpoint,
    Pack,
}

/// Where an object's own seal is (its header's last 32 bytes), from the
/// count its fixed part claims; None when the bytes are too short.
fn seal_end(kind: Kind, b: &[u8]) -> Option<usize> {
    let u32_at = |at: usize| b.get(at..at + 4).map(|x| u32::from_le_bytes(x.try_into().unwrap()) as usize);
    let end = match kind {
        Kind::Segment => f::segment_header_len(u32_at(88)?.min(1 << 16)),
        Kind::Pack => f::pack_header_len(u32_at(16)?.min(1 << 16)),
        Kind::Page | Kind::Checkpoint => b.len(),
    };
    (end >= 32 + 12 && end <= b.len()).then_some(end)
}

fn reseal(kind: Kind, b: &mut [u8]) {
    if let Some(end) = seal_end(kind, b) {
        let s = f::sha256(&b[..end - 32]);
        b[end - 32..end].copy_from_slice(&s);
    }
}

/// Decodes `b` as `kind`; when accepted, asserts it is canonical. Answers
/// whether it was accepted.
fn check(kind: Kind, b: &[u8]) -> bool {
    match kind {
        Kind::Segment => match f::decode_segment_header(b) {
            Ok(h) => {
                assert_eq!(f::encode_segment_header(&h), b[..h.header_len()], "a segment header is canonical");
                true
            }
            Err(_) => false,
        },
        Kind::Page => match f::decode_page(b) {
            Ok((p, entries)) => {
                assert_eq!(f::encode_page(p, &entries), b, "a page is canonical");
                true
            }
            Err(_) => false,
        },
        Kind::Checkpoint => match f::decode_checkpoint(b) {
            Ok(c) => {
                assert_eq!(f::encode_checkpoint(&c), b, "a checkpoint is canonical");
                true
            }
            Err(_) => false,
        },
        Kind::Pack => match f::decode_pack_header(b) {
            Ok(h) => {
                assert_eq!(f::encode_pack_header(&h), b[..h.len()], "a pack header is canonical");
                true
            }
            Err(_) => false,
        },
    }
}

fn seg_id(u: &mut Unstructured<'_>) -> arbitrary::Result<SegId> {
    Ok(SegId { epoch: u.int_in_range(1..=u64::MAX)?, seq: u.int_in_range(1..=u64::MAX)? })
}

fn loc(u: &mut Unstructured<'_>) -> arbitrary::Result<Loc> {
    Ok(Loc { seg: seg_id(u)?, offset: u.int_in_range(SEGMENT_FIXED_BYTES..=u32::MAX)?, len: EXTENT_BYTES, sum: u.arbitrary()? })
}

/// A valid object of `kind` from the input.
fn build(kind: Kind, u: &mut Unstructured<'_>) -> arbitrary::Result<Vec<u8>> {
    Ok(match kind {
        Kind::Segment => {
            let n = u.int_in_range(1..=6usize)?;
            let mut extent = u.int_in_range(0..=1000u64)?;
            let fill: Vec<Option<u8>> = (0..n).map(|_| u.arbitrary()).collect::<Result<_, _>>()?;
            let bytes: Vec<Option<Vec<u8>>> = fill.iter().map(|x| x.map(|b| vec![b; EXTENT_BYTES as usize])).collect();
            let mut images = Vec::new();
            for b in &bytes {
                images.push(Image { extent, bytes: b.as_deref() });
                extent += u.int_in_range(1..=5000u64)?;
            }
            let base = Link { epoch: u.arbitrary()?, seq: u.arbitrary()?, sum: u.arbitrary()? };
            f::encode_segment(seg_id(u)?, base, u.arbitrary()?, u.arbitrary()?, &images)
        }
        Kind::Page => {
            let n = u.int_in_range(0..=8usize)?;
            let mut i = u.int_in_range(0..=10u32)?;
            let mut entries = Vec::new();
            for _ in 0..n {
                if i >= PAGE_EXTENTS {
                    break;
                }
                let l = if u.arbitrary()? { Loc { seg: SegId { epoch: 0, seq: 0 }, offset: 0, len: 0, sum: 0 } } else { loc(u)? };
                entries.push((i, l));
                i += u.int_in_range(1..=600u32)?;
            }
            f::encode_page(u.int_in_range(0..=PAGES_MAX - 1)?, &entries)
        }
        Kind::Checkpoint => {
            let mut pages: Vec<PageRef> = Vec::new();
            let mut p = u.int_in_range(0..=4u32)?;
            for _ in 0..u.int_in_range(0..=6usize)? {
                pages.push(PageRef { page: p, epoch: u.int_in_range(1..=u64::MAX)?, seq: u.arbitrary()?, sum: u.arbitrary()? });
                p += u.int_in_range(1..=50u32)?;
                if p >= PAGES_MAX {
                    break;
                }
            }
            let mut segments = Vec::new();
            let mut seq = 1u64;
            for _ in 0..u.int_in_range(0..=6usize)? {
                segments.push((SegId { epoch: 1, seq }, u.arbitrary()?));
                seq += u.int_in_range(1..=9u64)?;
            }
            let boot_pages: Vec<u32> = pages.iter().map(|p| p.page).filter(|_| u.arbitrary().unwrap_or(false)).collect();
            let pack = |u: &mut Unstructured<'_>| -> arbitrary::Result<Option<PackRef>> {
                Ok(if u.arbitrary()? { Some(PackRef { epoch: u.int_in_range(1..=u64::MAX)?, seq: u.int_in_range(1..=u64::MAX)?, header_sum: u.arbitrary()? }) } else { None })
            };
            let c = Checkpoint {
                epoch: u.int_in_range(1..=u64::MAX)?,
                seq: u.int_in_range(1..=u64::MAX)?,
                end: Link { epoch: u.arbitrary()?, seq: u.arbitrary()?, sum: u.arbitrary()? },
                volume_bytes: u64::from(EXTENT_BYTES) * u.int_in_range(1..=1000u64)?,
                pack: pack(u)?,
                crash_pack: pack(u)?,
                pages,
                boot_pages,
                segments,
            };
            f::encode_checkpoint(&c)
        }
        Kind::Pack => {
            let entries: Vec<(u64, Loc)> = (0..u.int_in_range(1..=6usize)?).map(|_| Ok((u.int_in_range(0..=EXTENTS_MAX - 1)?, loc(u)?))).collect::<arbitrary::Result<_>>()?;
            f::encode_pack_header(&PackHeader { entries })
        }
    })
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let Ok(kind) = u.arbitrary::<Kind>() else { return };
    let Ok(raw) = u.arbitrary::<bool>() else { return };
    if raw {
        let mut b = u.take_rest().to_vec();
        check(kind, &b);
        reseal(kind, &mut b);
        check(kind, &b);
        return;
    }
    let Ok(good) = build(kind, &mut u) else { return };
    assert!(check(kind, &good), "what the encoder wrote decodes");
    let end = seal_end(kind, &good).expect("a whole object");
    // some bytes changed, then sealed again or not
    let mut b = good.clone();
    let Ok(n) = u.int_in_range(1..=4usize) else { return };
    for _ in 0..n {
        let (Ok(at), Ok(x)) = (u.int_in_range(0..=b.len() - 1), u.int_in_range(1..=255u8)) else { break };
        b[at] ^= x;
    }
    let Ok(again) = u.arbitrary::<bool>() else { return };
    if again {
        reseal(kind, &mut b);
        check(kind, &b);
    } else if b[..end] != good[..end] {
        assert!(!check(kind, &b), "a sealed object changed under its seal is refused");
    }
});
