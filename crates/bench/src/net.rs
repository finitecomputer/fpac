//! HTTPS for the driver: to the deployed Worker (calls signed with the
//! driver's key) and to Cloudflare's API (the token, in memory only).
//! rustls over a TCP stream, with the HTTP/1.1 exchange of
//! `fpac_remote::http`, so there is one HTTP client in the workspace.

use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fpac_remote::http::{Conn, Request};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::Value;

/// An answer body the driver reads whole (an exec's output included).
const BODY_BYTES_MAX: u64 = 16 << 20;

pub struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Answer {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&self.body).into_owned()))
    }
}

fn tls() -> Arc<rustls::ClientConfig> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().expect("TLS versions").with_root_certificates(roots).with_no_client_auth())
}

pub fn https(host: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8], timeout: Duration) -> Result<Answer, String> {
    let tcp = TcpStream::connect((host, 443)).map_err(|e| format!("connect {host}: {e}"))?;
    tcp.set_read_timeout(Some(timeout)).and_then(|_| tcp.set_write_timeout(Some(timeout))).map_err(|e| e.to_string())?;
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| e.to_string())?;
    let client = rustls::ClientConnection::new(tls(), name).map_err(|e| e.to_string())?;
    let mut conn = Conn::new(rustls::StreamOwned::new(client, tcp));
    let mut all = Vec::new();
    let mut h: Vec<(&str, &str)> = vec![("connection", "close")];
    h.extend_from_slice(headers);
    let (head, _) = conn
        .exchange(&Request { method, path, host, headers: &h, body }, BODY_BYTES_MAX, &mut |_, b| {
            all.extend_from_slice(b);
            Ok(())
        })
        .map_err(|e| format!("{method} https://{host}{path}: {e}"))?;
    Ok(Answer { status: head.status, body: all })
}

/// The driver's key, made on first use (mode 600).
pub struct Key(EcdsaKeyPair);

impl Key {
    pub fn load_or_make(path: &std::path::Path) -> Key {
        let rng = SystemRandom::new();
        if !path.exists() {
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).expect("a key");
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("its directory");
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).expect("the key file");
            std::io::Write::write_all(&mut f, pkcs8.as_ref()).expect("the key written");
        }
        let bytes = std::fs::read(path).expect("the key file");
        Key(EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &bytes, &rng).expect("a P-256 PKCS#8 key"))
    }

    /// The public half, uncompressed (65 bytes), as hex: a plain var.
    pub fn public_hex(&self) -> String {
        fpac_remote::hex(self.0.public_key().as_ref())
    }

    /// The Worker's `authorization` for one call (src/index.mjs, signed).
    pub fn authorization(&self, method: &str, path: &str, body: &[u8]) -> String {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("after 1970").as_millis();
        let message = format!("fpac-spike\n{method}\n{path}\n{now}\n{}", fpac_remote::hex(&fpac_remote::sha256(body)));
        let sig = self.0.sign(&SystemRandom::new(), message.as_bytes()).expect("a signature");
        format!("FpacSpike {now}.{}", fpac_remote::hex(sig.as_ref()))
    }
}

/// A signed call to the Worker.
pub fn worker(host: &str, key: &Key, method: &str, path: &str, body: &Value, timeout: Duration) -> Result<Value, String> {
    let b = if method == "GET" { Vec::new() } else { body.to_string().into_bytes() };
    let auth = key.authorization(method, path, &b);
    let a = https(host, method, path, &[("authorization", &auth), ("content-type", "application/json")], &b, timeout)?;
    let v = a.json();
    if a.status == 200 {
        Ok(v)
    } else {
        Err(format!("{method} {path}: {} {v}", a.status))
    }
}

/// A call to Cloudflare's API: its `result`, or the errors.
pub fn api(token: &str, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value), String> {
    let auth = format!("Bearer {token}");
    let b = body.map(|v| v.to_string().into_bytes()).unwrap_or_default();
    let a = https("api.cloudflare.com", method, &format!("/client/v4{path}"), &[("authorization", &auth), ("content-type", "application/json")], &b, Duration::from_secs(60))?;
    Ok((a.status, a.json()))
}
