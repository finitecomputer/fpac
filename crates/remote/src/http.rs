//! One HTTP/1.1 exchange over any stream: a request with a known-length
//! body, then the answer's status, headers and body handed to a sink as it
//! arrives (Content-Length or chunked). Bounded at every step. Written by
//! hand rather than taken from a crate: about two hundred lines, no
//! dependency, and the guest's binary stays static and small
//! (docs/design.md, Dependencies).

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use crate::limits;

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("the connection: {0}")]
    Io(#[from] std::io::Error),
    #[error("the connection closed before the answer was whole")]
    Closed,
    #[error("a malformed answer: {0}")]
    Malformed(&'static str),
    #[error("an answer past a limit: {0}")]
    TooBig(&'static str),
    #[error("the sink refused the body: {0}")]
    Sink(String),
}

/// Where an answer's body goes, chunk by chunk, with the answer's status.
/// An `Err` stops the read (the bytes failed a check).
pub type BodySink<'a> = dyn FnMut(u16, &[u8]) -> Result<(), String> + 'a;

pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub host: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
}

#[derive(Debug)]
pub struct Head {
    pub status: u16,
    /// Names lowercased.
    pub headers: Vec<(String, String)>,
    /// Whether the server will close the connection after this answer.
    pub close: bool,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// When the answer's parts arrived, from the moment the request began.
#[derive(Debug, Clone, Copy, Default)]
pub struct Timing {
    pub head: Duration,
    /// The first byte of the body (the head's time for an empty one).
    pub first_byte: Duration,
    pub total: Duration,
    /// What the remote said its own store took (`server-timing: …;dur=<ms>`),
    /// so a slow call can be put on the network or on the store.
    pub server_ms: Option<f64>,
}

/// The read side: a fixed buffer, made once per connection.
pub struct Conn<S> {
    pub stream: S,
    buf: Vec<u8>,
    start: usize,
    end: usize,
}

impl<S: Read + Write> Conn<S> {
    pub fn new(stream: S) -> Conn<S> {
        Conn { stream, buf: vec![0u8; limits::READ_BUFFER_BYTES], start: 0, end: 0 }
    }

    fn fill(&mut self) -> Result<(), HttpError> {
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        }
        if self.end == self.buf.len() {
            // compact: only the head's parse can get here with a full buffer
            self.buf.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        let n = self.stream.read(&mut self.buf[self.end..])?;
        if n == 0 {
            return Err(HttpError::Closed);
        }
        self.end += n;
        Ok(())
    }

    /// One CRLF-terminated line, without its CRLF, at most `max` bytes.
    fn line(&mut self, max: usize) -> Result<String, HttpError> {
        // Bounded: each pass reads more, and `max` stops it.
        loop {
            if let Some(at) = self.buf[self.start..self.end].windows(2).position(|w| w == b"\r\n") {
                let s = std::str::from_utf8(&self.buf[self.start..self.start + at]).map_err(|_| HttpError::Malformed("a line that is not UTF-8"))?.to_string();
                self.start += at + 2;
                return Ok(s);
            }
            if self.end - self.start > max {
                return Err(HttpError::TooBig("a line"));
            }
            self.fill()?;
        }
    }

    /// Exactly `n` bytes to the sink.
    fn body(&mut self, mut n: u64, status: u16, sink: &mut BodySink<'_>, first: &mut Option<Instant>) -> Result<(), HttpError> {
        // Bounded by n, which each pass lowers.
        while n > 0 {
            if self.start == self.end {
                self.fill()?;
            }
            let take = ((self.end - self.start) as u64).min(n) as usize;
            first.get_or_insert_with(Instant::now);
            sink(status, &self.buf[self.start..self.start + take]).map_err(HttpError::Sink)?;
            self.start += take;
            n -= take as u64;
        }
        Ok(())
    }

    /// Sends `req`, then reads the answer, its body to `sink` with the
    /// answer's status (so a sink can keep an error's text apart from an
    /// object's bytes). `body_max` bounds the body's bytes.
    pub fn exchange(&mut self, req: &Request<'_>, body_max: u64, sink: &mut BodySink<'_>) -> Result<(Head, Timing), HttpError> {
        assert!(self.start == self.end, "nothing left of an earlier answer");
        let t0 = Instant::now();
        let mut head = format!("{} {} HTTP/1.1\r\nhost: {}\r\ncontent-length: {}\r\n", req.method, req.path, req.host, req.body.len());
        for (n, v) in req.headers {
            assert!(!n.contains(['\r', '\n', ':']) && !v.contains(['\r', '\n']), "a header we send is one line");
            head.push_str(n);
            head.push_str(": ");
            head.push_str(v);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        self.stream.write_all(head.as_bytes())?;
        self.stream.write_all(req.body)?;
        self.stream.flush()?;
        let status_line = self.line(limits::HEAD_BYTES_MAX)?;
        let mut parts = status_line.splitn(3, ' ');
        let version = parts.next().unwrap_or("");
        if !version.starts_with("HTTP/1.") {
            return Err(HttpError::Malformed("the status line"));
        }
        let status: u16 = parts.next().and_then(|s| s.parse().ok()).ok_or(HttpError::Malformed("the status"))?;
        let mut headers = Vec::new();
        let mut head_bytes = status_line.len();
        loop {
            let l = self.line(limits::HEAD_BYTES_MAX)?;
            head_bytes += l.len() + 2;
            if head_bytes > limits::HEAD_BYTES_MAX {
                return Err(HttpError::TooBig("the head"));
            }
            if l.is_empty() {
                break;
            }
            if headers.len() == limits::HEADERS_MAX {
                return Err(HttpError::TooBig("the headers"));
            }
            let (n, v) = l.split_once(':').ok_or(HttpError::Malformed("a header"))?;
            headers.push((n.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
        let head_at = t0.elapsed();
        let close = version == "HTTP/1.0" || headers.iter().any(|(n, v)| n == "connection" && v.eq_ignore_ascii_case("close"));
        let h = Head { status, headers, close };
        let mut first = None;
        let chunked = h.header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
        if req.method == "HEAD" || status == 204 || status == 304 {
            // no body
        } else if chunked {
            let mut total = 0u64;
            // Bounded: body_max bounds the sum of the chunks.
            loop {
                let size_line = self.line(64)?;
                let size = u64::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16).map_err(|_| HttpError::Malformed("a chunk size"))?;
                if size == 0 {
                    // trailers, then the blank line
                    while !self.line(limits::HEAD_BYTES_MAX)?.is_empty() {}
                    break;
                }
                total = total.checked_add(size).ok_or(HttpError::TooBig("the body"))?;
                if total > body_max {
                    return Err(HttpError::TooBig("the body"));
                }
                self.body(size, status, sink, &mut first)?;
                if !self.line(2)?.is_empty() {
                    return Err(HttpError::Malformed("a chunk's end"));
                }
            }
        } else if let Some(len) = h.header("content-length") {
            let len: u64 = len.parse().map_err(|_| HttpError::Malformed("content-length"))?;
            if len > body_max {
                return Err(HttpError::TooBig("the body"));
            }
            self.body(len, status, sink, &mut first)?;
        } else {
            return Err(HttpError::Malformed("a body with no length"));
        }
        let total = t0.elapsed();
        let first_byte = first.map(|f| f.duration_since(t0)).unwrap_or(head_at);
        let server_ms = h.header("server-timing").and_then(|v| v.split(';').find_map(|p| p.trim().strip_prefix("dur="))).and_then(|d| d.parse().ok());
        Ok((h, Timing { head: head_at, first_byte, total, server_ms }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream that answers with fixed bytes and keeps what was written.
    struct Canned {
        answer: std::io::Cursor<Vec<u8>>,
        sent: Vec<u8>,
    }

    impl Read for Canned {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            // a few bytes at a time, so every boundary is crossed
            let n = b.len().min(7);
            self.answer.read(&mut b[..n])
        }
    }

    impl Write for Canned {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.sent.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn run(answer: &[u8], body_max: u64) -> Result<(Head, Vec<u8>), HttpError> {
        let mut c = Conn::new(Canned { answer: std::io::Cursor::new(answer.to_vec()), sent: Vec::new() });
        let mut got = Vec::new();
        let req = Request { method: "GET", path: "/o/e1/x", host: "h", headers: &[("range", "bytes=0-9")], body: &[] };
        let (h, _) = c.exchange(&req, body_max, &mut |_, b| {
            got.extend_from_slice(b);
            Ok(())
        })?;
        assert!(String::from_utf8_lossy(&c.stream.sent).starts_with("GET /o/e1/x HTTP/1.1\r\nhost: h\r\ncontent-length: 0\r\nrange: bytes=0-9\r\n\r\n"));
        Ok((h, got))
    }

    // Goal: a body by length and a chunked one both arrive whole.
    // Method: one of each, read seven bytes at a time.
    #[test]
    fn length_and_chunked() {
        let (h, b) = run(b"HTTP/1.1 206 Partial\r\nContent-Length: 5\r\nX-A: b\r\n\r\nhello", 100).unwrap();
        assert_eq!((h.status, b.as_slice(), h.header("x-a")), (206, &b"hello"[..], Some("b")));
        let (h, b) = run(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nhel\r\n2;x=y\r\nlo\r\n0\r\n\r\n", 100).unwrap();
        assert_eq!((h.status, b.as_slice(), h.close), (200, &b"hello"[..], true));
    }

    // Goal: an answer past a limit, cut short, or malformed is refused.
    // Method: one of each.
    #[test]
    fn refusals() {
        assert!(matches!(run(b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\n\r\n", 100), Err(HttpError::TooBig(_))));
        assert!(matches!(run(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhel", 100), Err(HttpError::Closed)));
        assert!(matches!(run(b"HTTP/1.1 200 OK\r\n\r\nhello", 100), Err(HttpError::Malformed(_))));
        assert!(matches!(run(b"SPDY 200 OK\r\n\r\n", 100), Err(HttpError::Malformed(_))));
        assert!(matches!(run(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n", 100), Err(HttpError::Malformed(_))));
        let mut many = b"HTTP/1.1 200 OK\r\n".to_vec();
        for i in 0..100 {
            many.extend_from_slice(format!("x-{i}: y\r\n").as_bytes());
        }
        many.extend_from_slice(b"\r\n");
        assert!(matches!(run(&many, 100), Err(HttpError::TooBig(_))));
    }
}
