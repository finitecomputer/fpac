//! An answer from the remote, read by hand (crates/remote/src/http.rs):
//! untrusted bytes, arriving in pieces of any size.
//!
//! Invariants:
//! - no input panics the reader, and it ends (each read takes bytes);
//! - an accepted answer's body is at most `body_max`, and exactly its
//!   Content-Length when it has one and is not chunked;
//! - its headers are within their limits;
//! - a second exchange on the same connection never panics either (bytes
//!   past an answer are refused).

#![no_main]

use std::io::{Read, Write};

use fpac_remote::http::{Conn, Request};
use fpac_remote::limits;
use libfuzzer_sys::fuzz_target;

/// The answer's bytes, `step` at a time.
struct Canned {
    data: Vec<u8>,
    at: usize,
    step: usize,
}

impl Read for Canned {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let n = (self.data.len() - self.at).min(self.step).min(b.len());
        b[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

impl Write for Canned {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&first, rest)) = data.split_first() else { return };
    let step = 1 + usize::from(first & 0x3f) * 97;
    let body_max = 1u64 << (u32::from(first >> 6) * 6 + 4);
    let mut c = Conn::new(Canned { data: rest.to_vec(), at: 0, step });
    let req = Request { method: "GET", path: "/o/e1/s1", host: "h", headers: &[], body: &[] };
    for _ in 0..2 {
        let mut got = 0u64;
        match c.exchange(&req, body_max, &mut |_, b| {
            got += b.len() as u64;
            Ok(())
        }) {
            Ok((h, _)) => {
                assert!(got <= body_max, "{got} bytes past the body's limit {body_max}");
                assert!(h.headers.len() <= limits::HEADERS_MAX);
                let chunked = h.header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
                if let (false, Some(len)) = (chunked, h.header("content-length")) {
                    if h.status != 204 && h.status != 304 {
                        assert_eq!(Some(got), len.parse().ok(), "the body is its Content-Length");
                    }
                }
            }
            Err(_) => break,
        }
    }
});
