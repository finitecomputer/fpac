//! Each object round-trips, and every malformed one is refused as a value:
//! cut short, wrong magic or version, past a limit, out of order,
//! inconsistent, or failing its checksum.

use super::*;

fn extent(seed: u8) -> Vec<u8> {
    (0..EXTENT_BYTES as usize).map(|i| (i as u8).wrapping_mul(31) ^ seed).collect()
}

fn seg() -> (Vec<u8>, Vec<Vec<u8>>) {
    let a = extent(1);
    let b = extent(2);
    let images = [Image { extent: 3, bytes: Some(&a) }, Image { extent: 9, bytes: None }, Image { extent: 10, bytes: Some(&b) }];
    let base = Link { epoch: 4, seq: 7, sum: [5; 32] };
    (encode_segment(SegId { epoch: 5, seq: 1 }, base, 2, true, &images), vec![a, b])
}

// Goal: a segment's header decodes to what was encoded, its extents'
// images sit where the header says, and each image's sum64 matches.
// Method: encode three extents (one of them zeroed) and read them back.
#[test]
fn segment_round_trip() {
    let (obj, bytes) = seg();
    let h = decode_segment_header(&obj).unwrap();
    assert_eq!(h.id, SegId { epoch: 5, seq: 1 });
    assert_eq!(h.base.seq, 7);
    assert!(h.last_of_cut);
    assert_eq!(h.cut, 2);
    assert_eq!(h.entries.len(), 3);
    assert!(h.entries[1].len == 0 && h.entries[1].extent == 9);
    for (e, b) in [(&h.entries[0], &bytes[0]), (&h.entries[2], &bytes[1])] {
        let image = &obj[e.offset as usize..(e.offset + e.len) as usize];
        assert_eq!(image, &b[..]);
        assert_eq!(sum64(image), e.sum);
    }
    assert_eq!(obj.len(), h.header_len() + 2 * EXTENT_BYTES as usize);
    // the header alone decodes: a replay fetches only that much
    assert_eq!(decode_segment_header(&obj[..h.header_len()]).unwrap(), h);
}

// Goal: every way a segment's header can be wrong is refused, never
// panics. Method: cut it short at every length; flip each header byte.
#[test]
fn segment_refusals() {
    let (obj, _) = seg();
    let h = decode_segment_header(&obj).unwrap();
    for n in 0..h.header_len() {
        assert!(decode_segment_header(&obj[..n]).is_err(), "cut at {n}");
    }
    for i in 0..h.header_len() {
        let mut bad = obj.clone();
        bad[i] ^= 0x40;
        assert!(decode_segment_header(&bad).is_err(), "flipped byte {i}");
    }
}

// Goal: a page round-trips, and its refusals hold. Method: two entries
// (one zero), then cut short, out of order, and a wrong sum.
#[test]
fn page_round_trip_and_refusals() {
    let l = Loc { seg: SegId { epoch: 2, seq: 3 }, offset: 4096, len: EXTENT_BYTES, sum: 77 };
    let z = Loc { seg: SegId { epoch: 0, seq: 0 }, offset: 0, len: 0, sum: 0 };
    let b = encode_page(5, &[(1, l), (9, z)]);
    assert_eq!(decode_page(&b).unwrap(), (5, vec![(1, l), (9, z)]));
    for n in 0..b.len() {
        assert!(decode_page(&b[..n]).is_err());
    }
    let mut extra = b.clone();
    extra.push(0);
    assert!(decode_page(&extra).is_err(), "bytes past the page");
    for i in 0..b.len() {
        let mut bad = b.clone();
        bad[i] ^= 1;
        assert!(decode_page(&bad).is_err(), "flipped byte {i}");
    }
}

fn checkpoint() -> Checkpoint {
    Checkpoint {
        epoch: 6,
        seq: 12,
        end: Link { epoch: 6, seq: 12, sum: [1; 32] },
        volume_bytes: 20 << 30,
        pack: Some(PackRef { epoch: 6, seq: 12, header_sum: [2; 32] }),
        crash_pack: Some(PackRef { epoch: 6, seq: 13, header_sum: [7; 32] }),
        pages: vec![PageRef { page: 0, epoch: 5, seq: 3, sum: [3; 32] }, PageRef { page: 7, epoch: 6, seq: 12, sum: [4; 32] }],
        boot_pages: vec![7],
        segments: vec![(SegId { epoch: 5, seq: 1 }, [5; 32]), (SegId { epoch: 6, seq: 2 }, [6; 32])],
    }
}

// Goal: a checkpoint round-trips, with and without a pack, and every
// malformed one is refused. Method: encode, decode; cut and flip.
#[test]
fn checkpoint_round_trip_and_refusals() {
    let c = checkpoint();
    let b = encode_checkpoint(&c);
    assert_eq!(decode_checkpoint(&b).unwrap(), c);
    let mut no_pack = c.clone();
    no_pack.pack = None;
    assert_eq!(decode_checkpoint(&encode_checkpoint(&no_pack)).unwrap(), no_pack);
    for n in 0..b.len() {
        assert!(decode_checkpoint(&b[..n]).is_err());
    }
    for i in 0..b.len() {
        let mut bad = b.clone();
        bad[i] ^= 0x10;
        assert!(decode_checkpoint(&bad).is_err(), "flipped byte {i}");
    }
}

// Goal: a pack's header round-trips and names each extent's place.
// Method: two extents; then the refusals.
#[test]
fn pack_round_trip() {
    let (a, b) = (extent(7), extent(8));
    let la = Loc { seg: SegId { epoch: 3, seq: 9 }, offset: 4096, len: EXTENT_BYTES, sum: sum64(&a) };
    let lb = Loc { seg: SegId { epoch: 2, seq: 1 }, offset: 0, len: EXTENT_BYTES, sum: sum64(&b) };
    let obj = encode_pack(&[(40, la, &a), (2, lb, &b)]);
    let h = decode_pack_header(&obj).unwrap();
    assert_eq!(h.entries, vec![(40, la), (2, lb)]);
    assert_eq!(&obj[h.len()..h.len() + EXTENT_BYTES as usize], &a[..]);
    for n in 0..h.len() {
        assert!(decode_pack_header(&obj[..n]).is_err());
    }
}

// Goal: a cleaner deletes only what no kept checkpoint names, from lives
// before the oldest kept one, and only once old enough; it never touches
// names that are not FPAC's. Method: a kept checkpoint of life 6 naming
// a segment and a page of life 5; objects of lives 4, 5 and 6, old and
// young.
#[test]
fn deletes_are_planned_from_kept_checkpoints_only() {
    let mut c = checkpoint();
    c.end = Link { epoch: 5, seq: 9, sum: [0; 32] };
    let day = 86_400_000;
    let objects: Vec<(String, u64)> = [
        ("e5/s1", day),     // named: kept
        ("e5/s2", day),     // not named, before the oldest life: deleted
        ("e5/m3-0", day),   // named page: kept
        ("e5/m3-1", day),   // not named: deleted
        ("e4/c1", day),     // an older checkpoint: deleted
        ("e4/s9", 1000),    // too young: kept
        ("e6/s99", day),    // the kept checkpoint's own life: kept
        ("e6/c12", day),    // the checkpoint itself: kept
        ("other/x", day),   // not ours: kept
    ]
    .iter()
    .map(|(n, a)| (n.to_string(), *a))
    .collect();
    let mut plan = plan_deletes(&[c], &objects, 3_600_000);
    plan.sort();
    assert_eq!(plan, vec!["e4/c1", "e5/m3-1", "e5/s2"]);
}
