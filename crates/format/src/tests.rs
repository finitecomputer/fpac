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

/// The header's own seal made again over bytes changed under it, so a
/// layout check (not the seal) is what refuses them.
fn resealed(mut b: Vec<u8>, header_len: usize) -> Vec<u8> {
    let s = sha256(&b[..header_len - 32]);
    b[header_len - 32..header_len].copy_from_slice(&s);
    b
}

// Goal: an object has one spelling: a reserved field that is not zero,
// or an absent pack's fields that are not, is refused even under a seal
// that matches. Method: a segment, a page, a checkpoint and a pack, each
// with one such field set and sealed again.
#[test]
fn reserved_fields_are_zero() {
    let a = extent(1);
    let seg = encode_segment(SegId { epoch: 1, seq: 1 }, GENESIS, 1, true, &[Image { extent: 3, bytes: Some(&a) }]);
    let hl = segment_header_len(1);
    let mut b = seg.clone();
    b[92] = 1;
    assert_eq!(decode_segment_header(&resealed(b, hl)), Err(FormatError::Layout("a segment's reserved field")));
    let l = Loc { seg: SegId { epoch: 1, seq: 1 }, offset: hl as u32, len: EXTENT_BYTES, sum: sum64(&a) };
    let page = encode_page(0, &[(3, l)]);
    let mut b = page.clone();
    b[48] = 1;
    let n = b.len();
    assert_eq!(decode_page(&resealed(b, n)), Err(FormatError::Layout("a page's reserved field")));
    let c = Checkpoint { epoch: 1, seq: 1, end: GENESIS, volume_bytes: 1 << 30, pack: None, crash_pack: None, pages: vec![], boot_pages: vec![], segments: vec![] };
    let mut b = encode_checkpoint(&c);
    b[8 + 4 + 4 + 16 + 48 + 8] = 1;
    let n = b.len();
    assert_eq!(decode_checkpoint(&resealed(b, n)), Err(FormatError::Layout("a checkpoint's pack")));
    let pack = encode_pack(&[(3, l, &a)]);
    let mut b = pack.clone();
    b[20] = 1;
    assert_eq!(decode_pack_header(&resealed(b, pack_header_len(1))), Err(FormatError::Layout("a pack's reserved fields")));
}

// Goal: a pack's header round-trips and names each extent's place.
// Method: two extents; then the refusals.
#[test]
fn pack_round_trip() {
    let (a, b) = (extent(7), extent(8));
    let la = Loc { seg: SegId { epoch: 3, seq: 9 }, offset: 4096, len: EXTENT_BYTES, sum: sum64(&a) };
    let lb = Loc { seg: SegId { epoch: 2, seq: 1 }, offset: 4096 + EXTENT_BYTES, len: EXTENT_BYTES, sum: sum64(&b) };
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

// Goal: a sealed segment's header and images open where they were
// sealed, and nowhere else: any byte changed, another key, another
// segment's name, or two images swapped is refused. Method: a segment of
// two images and a zero; each refusal in turn.
#[test]
fn sealed_segments_open_only_where_sealed() {
    let s = Sealer::new(&[9u8; 32], "disk-a");
    let (a, b) = (extent(1), extent(2));
    let id = SegId { epoch: 4, seq: 7 };
    let images = [Image { extent: 3, bytes: Some(&a) }, Image { extent: 9, bytes: None }, Image { extent: 10, bytes: Some(&b) }];
    let obj = encode_segment_sealed(&s, id, GENESIS, 2, true, &images);
    let h = decode_segment_header_sealed(&s, id, &obj).unwrap();
    assert_eq!(h.entries.len(), 3);
    let hl = sealed_segment_header_len(3);
    assert_eq!(h.entries[0].offset as usize, hl);
    assert_eq!(h.entries[2].offset as usize, hl + SEALED_EXTENT_BYTES as usize);
    let open = |e: &Entry, obj: &[u8]| {
        let mut out = vec![0u8; EXTENT_BYTES as usize];
        let at = e.offset as usize;
        s.open_extent(Place::Image { seg: id, extent: e.extent, offset: e.offset }, &obj[at..at + SEALED_EXTENT_BYTES as usize], e.sum, &mut out).map(|()| out)
    };
    assert_eq!(open(&h.entries[0], &obj).unwrap(), a);
    assert_eq!(open(&h.entries[2], &obj).unwrap(), b);
    // the images hold no plain bytes
    assert!(!obj.windows(64).any(|w| w == &a[..64]));
    for i in [0, 12, 20, hl - 1, hl, hl + 100, obj.len() - 1] {
        let mut bad = obj.clone();
        bad[i] ^= 1;
        let refused = if i < hl { decode_segment_header_sealed(&s, id, &bad).is_err() } else { open(&h.entries[if i < hl + SEALED_EXTENT_BYTES as usize { 0 } else { 2 }], &bad).is_err() };
        assert!(refused, "byte {i} changed");
    }
    assert_eq!(decode_segment_header_sealed(&Sealer::new(&[8u8; 32], "disk-a"), id, &obj).err(), Some(FormatError::Sealed));
    assert_eq!(decode_segment_header_sealed(&Sealer::new(&[9u8; 32], "disk-b"), id, &obj).err(), Some(FormatError::Sealed));
    assert_eq!(decode_segment_header_sealed(&s, SegId { epoch: 4, seq: 8 }, &obj).err(), Some(FormatError::Sealed));
    // an image moved to the other's place does not open there
    let (e0, e2) = (h.entries[0], h.entries[2]);
    let moved = Entry { offset: e2.offset, ..e0 };
    let mut swapped = obj.clone();
    let n = SEALED_EXTENT_BYTES as usize;
    swapped.copy_within(e0.offset as usize..e0.offset as usize + n, e2.offset as usize);
    assert!(open(&moved, &swapped).is_err(), "an image is bound to its offset");
    assert!(open(&Entry { extent: 11, ..e0 }, &obj).is_err(), "and to its extent");
}

// Goal: a sealed pack opens extent by extent at its places, and a sealed
// page or checkpoint opens only under its own name. Method: a pack of two
// extents; a page sealed and opened under two names.
#[test]
fn sealed_packs_and_objects_open_only_where_sealed() {
    let s = Sealer::new(&[5u8; 32], "disk-a");
    let (a, b) = (extent(3), extent(4));
    let la = Loc { seg: SegId { epoch: 1, seq: 2 }, offset: 4096, len: EXTENT_BYTES, sum: 77 };
    let lb = Loc { seg: SegId { epoch: 1, seq: 3 }, offset: 4096, len: EXTENT_BYTES, sum: 78 };
    let (obj, sum) = encode_pack_sealed(&s, "e1/p1", &[(5, la, &a), (6, lb, &b)]);
    let (h, sum2) = decode_pack_header_sealed(&s, "e1/p1", &obj).unwrap();
    assert_eq!((h.entries, sum2), (vec![(5, la), (6, lb)], sum));
    assert!(decode_pack_header_sealed(&s, "e1/p2", &obj).is_err());
    let at = sealed_pack_header_len(2) + SEALED_EXTENT_BYTES as usize;
    let mut out = vec![0u8; EXTENT_BYTES as usize];
    let sealed = &obj[at..at + SEALED_EXTENT_BYTES as usize];
    let tag: [u8; 16] = sealed[EXTENT_BYTES as usize..].try_into().unwrap();
    s.open_extent(Place::Packed { pack: "e1/p1", index: 1, extent: 6 }, sealed, Sealer::sum_of(&tag), &mut out).unwrap();
    assert_eq!(out, b);
    assert!(s.open_extent(Place::Packed { pack: "e1/p1", index: 0, extent: 6 }, sealed, Sealer::sum_of(&tag), &mut out).is_err());
    let page = encode_page(0, &[(3, la)]);
    let sealed = s.seal_object("e1/m1-0", &page);
    assert_eq!(s.open_object("e1/m1-0", &sealed).unwrap(), page);
    assert_eq!(s.open_object("e1/m1-1", &sealed), Err(FormatError::Sealed));
    assert!(!sealed.windows(8).any(|w| w == b"FPACMAP1"), "a sealed page shows nothing plain");
}

// Goal: a sealed disk's checkpoint is readable without the key (what it
// names is the platform's cleaner's) and authenticated by it: a byte
// changed, another name or another key is refused. Method: a checkpoint
// authenticated, read back each way.
#[test]
fn an_authenticated_checkpoint_reads_without_the_key_and_verifies_with_it() {
    let s = Sealer::new(&[7u8; 32], "disk-a");
    let c = Checkpoint { epoch: 3, seq: 2, end: GENESIS, volume_bytes: 1 << 30, pack: None, crash_pack: None, pages: vec![PageRef { page: 1, epoch: 3, seq: 2, sum: [9; 32] }], boot_pages: vec![], segments: vec![(SegId { epoch: 3, seq: 1 }, [8; 32])] };
    let plain = encode_checkpoint(&c);
    let auth = s.authenticate_object("e3/c2", &plain);
    assert_eq!(decode_checkpoint_unverified(&auth).unwrap(), c, "readable without the key");
    assert_eq!(decode_checkpoint_unverified(&plain).unwrap(), c, "a plain disk's too");
    assert_eq!(s.verify_object("e3/c2", &auth).unwrap(), plain);
    assert_eq!(s.verify_object("e3/c3", &auth), Err(FormatError::Sealed), "another name");
    assert_eq!(Sealer::new(&[8u8; 32], "disk-a").verify_object("e3/c2", &auth), Err(FormatError::Sealed), "another key");
    for i in [0, 8, 40, auth.len() - 1] {
        let mut bad = auth.clone();
        bad[i] ^= 1;
        assert!(s.verify_object("e3/c2", &bad).is_err(), "byte {i} changed");
    }
}

