//! `fpac-probe`: run in the guest by the driver (`fpac-bench`), it prints
//! one JSON document on stdout (docs/spike.md, Acceptance 1, 2 and 7).
//!
//! fpac-probe probe [--check-only] [--dir <scratch>]
//! fpac-probe path [--host <h>] [--port <p>] [--tag <t>]
//! fpac-probe conformance [--host <h>] [--port <p>] [--tag <t>]
//! fpac-probe ranges [--count <n>] [--diagnose]
//! fpac-probe remotecheck --count <n> --threads <t> --whole-every <k> [--out <file>]
//! fpac-probe rangecheck --mode stream|tee|buffer --pattern aligned|unaligned --count <n> --threads <t> --size-max <bytes> [--out <file>] [--host <h>] [--port <p>] [--tag <t>]
//! fpac-probe idle
//!
//! `--check-only` reads what the place offers and changes nothing: no
//! node made, nothing mounted, no loop device attached. It is the only
//! mode run in a container that shares this box's kernel.

mod path;
#[cfg(target_os = "linux")]
mod probe;

use std::time::Duration;

use serde_json::{json, Value};

/// The intercepted host the guest reaches the remote by.
pub const REMOTE_HOST: &str = "fpac.fragment.internal";

pub fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

/// Times in ms: n, p50, p99, min, max, mean.
pub fn stats(mut ms: Vec<f64>) -> Value {
    if ms.is_empty() {
        return json!({ "n": 0 });
    }
    ms.sort_by(|a, b| a.total_cmp(b));
    let at = |q: f64| ms[((ms.len() as f64 - 1.0) * q).round() as usize];
    let r = |x: f64| (x * 1000.0).round() / 1000.0;
    json!({ "n": ms.len(), "p50": r(at(0.5)), "p99": r(at(0.99)), "min": r(ms[0]), "max": r(*ms.last().unwrap()), "mean": r(ms.iter().sum::<f64>() / ms.len() as f64) })
}

pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let host = arg(&args, "--host").unwrap_or_else(|| REMOTE_HOST.to_string());
    let port: u16 = arg(&args, "--port").and_then(|p| p.parse().ok()).unwrap_or(80);
    let tag = arg(&args, "--tag").unwrap_or_else(|| "t".to_string());
    let out = match args.get(1).map(String::as_str) {
        #[cfg(target_os = "linux")]
        Some("probe") => probe::run(args.iter().any(|a| a == "--check-only"), &arg(&args, "--dir").unwrap_or_else(|| "/var/tmp/fpac-probe".into())),
        Some("path") => path::run(&host, port, &tag),
        Some("rangecheck") => path::rangecheck(&host, port, &tag, &args),
        Some("remotecheck") => path::remotecheck(&host, port, &tag, &args),
        Some("ranges") => path::ranges(&host, port, &tag, arg(&args, "--count").and_then(|c| c.parse().ok()).unwrap_or(1000).min(100_000), args.iter().any(|a| a == "--diagnose")),
        Some("conformance") => {
            let mut remote = fpac_remote::Remote::new(&host, port);
            let cases = fpac_testkit::conformance::run(&mut remote, &tag);
            let failed = cases.iter().filter(|c| !c.pass).count();
            json!({ "evidence": "conformance", "remote": format!("{host}:{port}"), "passed": cases.len() - failed, "failed": failed,
                "cases": cases.iter().map(|c| json!({ "case": c.name, "pass": c.pass, "detail": c.detail })).collect::<Vec<_>>() })
        }
        #[cfg(target_os = "linux")]
        Some("idle") => idle(),
        _ => {
            eprintln!("usage: fpac-probe probe [--check-only] [--dir d] | path | conformance [--host h] [--port p] [--tag t] | idle");
            std::process::exit(2);
        }
    };
    println!("{out}");
}

#[cfg(target_os = "linux")]
static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(target_os = "linux")]
extern "C" fn on_term(_: libc::c_int) {
    STOP.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// The image's entrypoint: waits, reaping any child it inherits, until
/// SIGTERM or SIGINT.
#[cfg(target_os = "linux")]
fn idle() -> Value {
    // SAFETY: installing a handler that only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as *const () as libc::sighandler_t);
    }
    // Unbounded on purpose: it is the container's life, ended by a signal.
    while !STOP.load(std::sync::atomic::Ordering::SeqCst) {
        // SAFETY: waitpid on any child, not blocking.
        while unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } > 0 {}
        std::thread::sleep(Duration::from_millis(200));
    }
    json!({ "idle": "stopped" })
}
