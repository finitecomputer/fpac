//! The spike's local config: everything specific to one deployment (the
//! account, the names, the key and token files). It lives outside this
//! repository, at `$FPAC_SPIKE_CONFIG` (default
//! `~/.cache/fpac/spike/local.json`), so nothing account-specific is ever
//! committed (docs/spike.md, Musts).

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub account_id: String,
    pub worker_name: String,
    pub bucket_name: String,
    /// R2's location hint for the bucket when the driver makes it (`wnam`,
    /// `enam`, `weur`, `eeur`, `apac`, `oc`); R2 picks when absent.
    #[serde(default)]
    pub bucket_location_hint: Option<String>,
    /// `https://<worker>.<subdomain>.workers.dev`
    pub worker_url: String,
    /// A file holding the API token (read into memory, passed to wrangler
    /// through its environment, never printed or put on a command line).
    pub token_file: PathBuf,
    /// The driver's ECDSA P-256 key (PKCS#8), made on first use, mode 600.
    pub driver_key_file: PathBuf,
    /// The pinned Node (its unpacked directory) that runs wrangler.
    pub node_dir: PathBuf,
    /// A sealed disk's test data key (docs/encryption.md): a file of 64
    /// hex digits, mode 600, used when `FPAC_ENCRYPT` is set. A test key
    /// only: a real disk's comes from its agent's fragment.
    #[serde(default)]
    pub test_data_key_file: Option<PathBuf>,
}

impl Config {
    pub fn load() -> Result<Config, String> {
        let path = std::env::var_os("FPAC_SPIKE_CONFIG").map(PathBuf::from).unwrap_or_else(|| home().join(".cache/fpac/spike/local.json"));
        let text = std::fs::read_to_string(&path).map_err(|e| format!("the local config {}: {e}", path.display()))?;
        let c: Config = serde_json::from_str(&text).map_err(|e| format!("the local config {}: {e}", path.display()))?;
        assert!(c.worker_name.starts_with("fpac-") && c.bucket_name.starts_with("fpac-"), "every resource the spike makes carries the project's prefix");
        assert!(c.worker_url.starts_with("https://"));
        Ok(c)
    }

    pub fn host(&self) -> &str {
        self.worker_url.trim_start_matches("https://").trim_end_matches('/')
    }

    pub fn token(&self) -> String {
        std::fs::read_to_string(&self.token_file).expect("the token file").trim().to_string()
    }
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME"))
}

/// This repository's root (the driver is run from its workspace).
pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("the repository")
}
