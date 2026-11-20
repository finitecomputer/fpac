//! Sealed objects (docs/encryption.md): behind a flag, every extent, map
//! page, checkpoint and pack is sealed with AES-256-GCM-SIV under the
//! disk's data key, and its tag is what a read checks in place of sum64.
//!
//! What a seal binds (its associated data): the disk, and the object's
//! name; an extent's seal its segment, its extent number and its offset
//! there too; a packed extent's its pack and its place in it; a cached
//! extent's the life, the extent and how often this life wrote it. So no
//! sealed piece can be spliced into another object, moved within one, or
//! (in the cache) served from an earlier write. The nonce is drawn from
//! the associated data (each is sealed once, or again with the same
//! bytes); GCM-SIV makes a repeated nonce cost no more than showing that
//! the same bytes were sealed twice at the same place.
//!
//! The layouts, beside the plain ones in lib.rs:
//! - a segment: `FPACSEGS`, its count (u32), zero (u32); its plain header
//!   sealed (its own SHA-256 kept inside, which the next segment's base
//!   names); then each image, 64 KiB and its 16-byte tag, at the offset its
//!   entry names. An entry's sum is the first 8 bytes of its image's tag.
//! - a pack: `FPACPAKS`, its count, zero; its plain header sealed; each
//!   extent and its tag.
//! - a page or a checkpoint: `FPACSEAL`, then the plain object sealed.

use aes_gcm_siv::aead::{AeadInOut, KeyInit};
use aes_gcm_siv::{Aes256GcmSiv, Nonce, Tag};

use crate::limits::{EXTENT_BYTES, PACK_ENTRY_BYTES, PACK_EXTENTS_MAX, PACK_FIXED_BYTES, PACK_OBJECT_BYTES_MAX, SEGMENT_BYTES_MAX, SEGMENT_EXTENTS_MAX, SEGMENT_HEADER_BYTES_MAX};
use crate::{sha256, FormatError, SegId};

pub const TAG_BYTES: u32 = 16;
/// A sealed extent in an object: its bytes, then its tag.
pub const SEALED_EXTENT_BYTES: u32 = EXTENT_BYTES + TAG_BYTES;
/// A sealed segment's or pack's clear prefix: magic, count, zero.
pub const SEALED_PREFIX_BYTES: u32 = 16;
pub(crate) const SEGMENT_SEALED_MAGIC: &[u8; 8] = b"FPACSEGS";
pub(crate) const PACK_SEALED_MAGIC: &[u8; 8] = b"FPACPAKS";
const OBJECT_SEALED_MAGIC: &[u8; 8] = b"FPACSEAL";
/// The associated data's first bytes: which scheme sealed it.
const DOMAIN: &[u8; 8] = b"fpacsiv1";

const _: () = assert!(SEGMENT_EXTENTS_MAX <= u32::MAX / SEALED_EXTENT_BYTES && PACK_EXTENTS_MAX <= u32::MAX / SEALED_EXTENT_BYTES);

/// A sealed pack's extents, at most: each carries its tag, so one fewer
/// than a plain pack's (a full one was refused 413 at 100,683,808 bytes).
pub const PACK_EXTENTS_SEALED_MAX: u32 = PACK_EXTENTS_MAX - 1;
/// A sealed pack's largest object; within the plain one's, so every limit
/// asserted of that holds of it.
pub const PACK_SEALED_OBJECT_BYTES_MAX: u64 = SEALED_PREFIX_BYTES as u64 + PACK_FIXED_BYTES + PACK_EXTENTS_SEALED_MAX as u64 * PACK_ENTRY_BYTES + 32 + TAG_BYTES as u64 + PACK_EXTENTS_SEALED_MAX as u64 * SEALED_EXTENT_BYTES as u64;
const _: () = assert!(PACK_SEALED_OBJECT_BYTES_MAX <= PACK_OBJECT_BYTES_MAX);
/// A sealed segment at its most extents stays within SEGMENT_BYTES_MAX.
const _: () = assert!(SEALED_PREFIX_BYTES as u64 + SEGMENT_HEADER_BYTES_MAX as u64 + TAG_BYTES as u64 + SEGMENT_EXTENTS_MAX as u64 * SEALED_EXTENT_BYTES as u64 <= SEGMENT_BYTES_MAX);

/// What a seal binds, beside the disk.
#[derive(Debug, Clone, Copy)]
pub enum Place<'a> {
    /// A whole object (a page, a checkpoint) or a segment's or pack's
    /// header: its name.
    Object(&'a str),
    /// An image in a segment.
    Image { seg: SegId, extent: u64, offset: u32 },
    /// The `index`th extent of the pack named `pack`.
    Packed { pack: &'a str, index: u32, extent: u64 },
    /// An extent in the cache file: the life that wrote it, and that
    /// life's count of writes to it.
    Cache { life: u64, extent: u64, generation: u64 },
}

impl Place<'_> {
    fn bytes(&self, disk: &[u8; 32]) -> Vec<u8> {
        let mut a = Vec::with_capacity(96);
        a.extend_from_slice(DOMAIN);
        a.extend_from_slice(disk);
        match self {
            Place::Object(name) => {
                a.push(1);
                a.extend_from_slice(name.as_bytes());
            }
            Place::Image { seg, extent, offset } => {
                a.push(2);
                a.extend_from_slice(&seg.epoch.to_le_bytes());
                a.extend_from_slice(&seg.seq.to_le_bytes());
                a.extend_from_slice(&extent.to_le_bytes());
                a.extend_from_slice(&offset.to_le_bytes());
            }
            Place::Packed { pack, index, extent } => {
                a.push(3);
                a.extend_from_slice(&index.to_le_bytes());
                a.extend_from_slice(&extent.to_le_bytes());
                a.extend_from_slice(pack.as_bytes());
            }
            Place::Cache { life, extent, generation } => {
                a.push(4);
                a.extend_from_slice(&life.to_le_bytes());
                a.extend_from_slice(&extent.to_le_bytes());
                a.extend_from_slice(&generation.to_le_bytes());
            }
        }
        a
    }
}

/// The disk's data key, ready to seal and open. Made once per life, at
/// the mount, from the key the daemon is handed (docs/encryption.md).
#[derive(Clone)]
pub struct Sealer {
    aead: Aes256GcmSiv,
    /// The SHA-256 of the disk's name: every seal binds it.
    disk: [u8; 32],
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sealer(..)")
    }
}

impl Sealer {
    pub fn new(key: &[u8; 32], disk: &str) -> Sealer {
        Sealer { aead: Aes256GcmSiv::new(key.into()), disk: sha256(disk.as_bytes()) }
    }

    fn nonce_ad(&self, at: Place<'_>) -> (Nonce, Vec<u8>) {
        let ad = at.bytes(&self.disk);
        let n = sha256(&ad);
        (Nonce::from(<[u8; 12]>::try_from(&n[..12]).expect("12")), ad)
    }

    /// Seals `buf` in place, answering its tag.
    pub fn seal_detached(&self, at: Place<'_>, buf: &mut [u8]) -> [u8; 16] {
        let (n, ad) = self.nonce_ad(at);
        let tag = self.aead.encrypt_inout_detached(&n, &ad, buf.into()).expect("a seal within GCM-SIV's limits");
        tag.into()
    }

    /// Opens `buf` in place; refused (and `buf` not to be used) when it is
    /// not what was sealed at `at` with this key.
    pub fn open_detached(&self, at: Place<'_>, buf: &mut [u8], tag: &[u8; 16]) -> Result<(), FormatError> {
        let (n, ad) = self.nonce_ad(at);
        self.aead.decrypt_inout_detached(&n, &ad, buf.into(), &Tag::from(*tag)).map_err(|_| FormatError::Sealed)
    }

    /// A whole object, sealed: `FPACSEAL`, its bytes, its tag.
    pub fn seal_object(&self, name: &str, plain: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + plain.len() + TAG_BYTES as usize);
        out.extend_from_slice(OBJECT_SEALED_MAGIC);
        out.extend_from_slice(plain);
        let tag = self.seal_detached(Place::Object(name), &mut out[8..]);
        out.extend_from_slice(&tag);
        out
    }

    /// The plain object from a sealed one named `name`.
    pub fn open_object(&self, name: &str, sealed: &[u8]) -> Result<Vec<u8>, FormatError> {
        if sealed.len() < 8 + TAG_BYTES as usize {
            return Err(FormatError::Short { what: "a sealed object", need: 8 + TAG_BYTES as usize, got: sealed.len() });
        }
        if &sealed[..8] != OBJECT_SEALED_MAGIC {
            return Err(FormatError::Magic("a sealed object"));
        }
        let (body, tag) = sealed[8..].split_at(sealed.len() - 8 - TAG_BYTES as usize);
        let mut plain = body.to_vec();
        self.open_detached(Place::Object(name), &mut plain, tag.try_into().expect("16"))?;
        Ok(plain)
    }

    /// A sealed extent's sum: the first 8 bytes of its tag (what the map
    /// keeps, and a pack entry names).
    pub fn sum_of(tag: &[u8; 16]) -> u64 {
        u64::from_le_bytes(tag[..8].try_into().expect("8"))
    }

    /// Opens a sealed extent (64 KiB and its tag) read from `place`,
    /// into `out`, checking its tag is the one the map named (`sum`).
    pub fn open_extent(&self, at: Place<'_>, sealed: &[u8], sum: u64, out: &mut [u8]) -> Result<(), FormatError> {
        if sealed.len() != SEALED_EXTENT_BYTES as usize || out.len() != EXTENT_BYTES as usize {
            return Err(FormatError::Short { what: "a sealed extent", need: SEALED_EXTENT_BYTES as usize, got: sealed.len() });
        }
        let tag: [u8; 16] = sealed[EXTENT_BYTES as usize..].try_into().expect("16");
        if Self::sum_of(&tag) != sum {
            return Err(FormatError::Sealed);
        }
        out.copy_from_slice(&sealed[..EXTENT_BYTES as usize]);
        self.open_detached(at, out, &tag)
    }
}
