//! `fpac-probe aead`: what sealing an extent costs on this CPU, for the
//! choice of cipher (docs/encryption.md). One thread seals and opens 64 KiB
//! with AES-256-GCM-SIV and with XChaCha20-Poly1305, and hashes it with
//! SHA-256 (the sum64 a sealed extent's tag replaces), each for about a
//! second; and reads the CPU's instruction sets that make each fast.

use std::time::{Duration, Instant};

use aes_gcm_siv::aead::{AeadInOut, KeyInit};
use aes_gcm_siv::Aes256GcmSiv;
use chacha20poly1305::XChaCha20Poly1305;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const EXTENT_BYTES: usize = 64 * 1024;
/// Each cipher's turn, at least.
const TURN: Duration = Duration::from_millis(1000);
/// The flags that matter here, as /proc/cpuinfo names them.
const FLAGS: [&str; 7] = ["aes", "vaes", "pclmulqdq", "vpclmulqdq", "avx2", "avx512f", "sha_ni"];

/// MB/s of `f` over 64 KiB, run until a turn has passed.
fn rate(mut f: impl FnMut(&mut [u8])) -> f64 {
    let mut buf = vec![0x5au8; EXTENT_BYTES];
    let t = Instant::now();
    let mut n = 0u64;
    // Bounded by TURN.
    while t.elapsed() < TURN {
        f(&mut buf);
        n += 1;
    }
    let mbs = (n * EXTENT_BYTES as u64) as f64 / t.elapsed().as_secs_f64() / 1e6;
    (mbs * 10.0).round() / 10.0
}

fn cpu() -> Value {
    let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let model = info.lines().find_map(|l| l.strip_prefix("model name").map(|r| r.trim_start_matches([' ', '\t', ':']).to_string()));
    let flags: Vec<&str> = info.lines().find_map(|l| l.strip_prefix("flags")).map(|r| r.trim_start_matches([' ', '\t', ':']).split(' ').collect()).unwrap_or_default();
    json!({ "model": model, "flags": FLAGS.iter().map(|f| (f.to_string(), json!(flags.contains(f)))).collect::<serde_json::Map<_, _>>() })
}

pub fn run() -> Value {
    let aad = [7u8; 48];
    let siv = Aes256GcmSiv::new(&[1u8; 32].into());
    let xc = XChaCha20Poly1305::new(&[2u8; 32].into());
    let siv_nonce = [3u8; 12].into();
    let xc_nonce = [4u8; 24].into();
    let siv_seal = rate(|b| {
        let _ = siv.encrypt_inout_detached(&siv_nonce, &aad, b.into()).expect("a seal");
    });
    // an open reads the sealed bytes into another buffer, so the same
    // message opens every turn
    let siv_open = {
        let mut sealed = vec![0x5au8; EXTENT_BYTES];
        let tag = siv.encrypt_inout_detached(&siv_nonce, &aad, sealed.as_mut_slice().into()).expect("a seal");
        let mut out = vec![0u8; EXTENT_BYTES];
        rate(|_| {
            let io = aes_gcm_siv::aead::inout::InOutBuf::new(&sealed, &mut out).expect("same lengths");
            siv.decrypt_inout_detached(&siv_nonce, &aad, io, &tag).expect("it opens");
        })
    };
    let xc_seal = rate(|b| {
        let _ = xc.encrypt_inout_detached(&xc_nonce, &aad, b.into()).expect("a seal");
    });
    let sha = rate(|b| {
        let d = Sha256::digest(&*b);
        b[0] ^= d[0];
    });
    json!({
        "evidence": "aead",
        "cpu": cpu(),
        "extent_bytes": EXTENT_BYTES,
        "mbs": { "aes256gcmsiv_seal": siv_seal, "aes256gcmsiv_open": siv_open, "xchacha20poly1305_seal": xc_seal, "sha256": sha },
    })
}
