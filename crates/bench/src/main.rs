//! `fpac-bench`: the evidence driver (docs/spike.md, Evaluation). Each
//! scenario runs in a place, collects what the guest's half
//! (`fpac-probe`) prints, and with `--write` merges it into
//! `docs/evidence/<scenario>.json` under the place's name, as is.
//!
//! fpac-bench stage                                 the probe built, the image staged and built
//! fpac-bench probe --place krun|docker|cloudflare [--write]
//! fpac-bench path --place krun|cloudflare [--write] [--label <l>]
//! fpac-bench ranges --place cloudflare [--count n] [--diagnose] [--write] [--label <l>]
//! fpac-bench rangecheck --place cloudflare|outside --mode stream|tee|buffer --pattern aligned|unaligned
//!            --count n --threads t --size-max bytes [--write] [--label <l>]
//! fpac-bench exec <argv…> | destroy               the running Cloudflare container, for debugging
//! fpac-bench deploy                                the stand-in Worker, its bucket, its image
//! fpac-bench inventory <label>                     the account's Workers, buckets, container apps
//! fpac-bench reset                                 the one cleanup (docs/spike.md, Musts)
//!
//! Places:
//! - `krun`: this box's sandcastle engine; the remote is the folder
//!   remote on a unix socket (a stand-in for the route and for R2).
//! - `docker`: a plain `docker run` with no privilege, flag or device
//!   added, as local workerd starts a computer's container. Check-only:
//!   it shares this box's kernel, so it builds nothing.
//! - `cloudflare`: the deployed stand-in Worker and its R2 bucket.

mod config;
mod krun;
mod net;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use config::{home, repo, Config};

const PROBE_IN_GUEST: &str = "/usr/local/bin/fpac-probe";

fn now_ms() -> u64 {
    u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).expect("after 1970").as_millis()).expect("fits")
}

fn work_dir() -> PathBuf {
    let d = std::env::var_os("FPAC_WORK_DIR").map(PathBuf::from).unwrap_or_else(|| home().join(".cache/fpac/spike/work"));
    std::fs::create_dir_all(&d).expect("the work directory");
    d
}

fn run(cmd: &mut Command) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("{:?}: {e}", cmd.get_program()))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!("{:?} {:?}: {}\n{}", cmd.get_program(), cmd.get_args().collect::<Vec<_>>(), out.status, String::from_utf8_lossy(&out.stderr)))
    }
}

fn commit() -> Value {
    let head = run(Command::new("git").arg("-C").arg(repo()).args(["rev-parse", "--short=12", "HEAD"])).unwrap_or_default();
    let dirty = !run(Command::new("git").arg("-C").arg(repo()).args(["status", "--porcelain", "--", "crates", "worker", "images"])).unwrap_or_default().trim().is_empty();
    json!({ "fpac": head.trim(), "dirty": dirty })
}

/// Builds the static probe and stages the image's context: answers the
/// context's directory and a tag naming its contents.
fn stage() -> Result<(PathBuf, String), String> {
    run(Command::new("cargo").current_dir(repo()).args(["build", "--release", "--target", "x86_64-unknown-linux-musl", "-p", "fpac-probe"]))?;
    let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| repo().join("target"));
    let bin = target.join("x86_64-unknown-linux-musl/release/fpac-probe");
    let ctx = work_dir().join("image");
    std::fs::create_dir_all(&ctx).map_err(|e| e.to_string())?;
    let (b, d) = (std::fs::read(&bin).map_err(|e| format!("{}: {e}", bin.display()))?, std::fs::read(repo().join("images/tiny/Dockerfile")).map_err(|e| e.to_string())?);
    std::fs::write(ctx.join("fpac-probe"), &b).map_err(|e| e.to_string())?;
    std::fs::write(ctx.join("Dockerfile"), &d).map_err(|e| e.to_string())?;
    std::fs::set_permissions(ctx.join("fpac-probe"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).map_err(|e| e.to_string())?;
    let tag = fpac_remote::hex(&fpac_remote::sha256(&[b, d].concat()))[..12].to_string();
    Ok((ctx, tag))
}

/// The staged image built by the local Docker: its reference.
fn docker_image() -> Result<String, String> {
    let (ctx, tag) = stage()?;
    let reference = format!("fpac-spike-tiny:{tag}");
    run(Command::new("docker").args(["build", "-q", "--platform", "linux/amd64", "-t", &reference]).arg(&ctx))?;
    Ok(reference)
}

fn guest_json(stdout: &str) -> Result<Value, String> {
    let line = stdout.lines().rev().find(|l| l.starts_with('{')).ok_or_else(|| format!("no JSON from the guest: {}", &stdout[..stdout.len().min(2000)]))?;
    serde_json::from_str(line).map_err(|e| format!("the guest's JSON: {e}"))
}

/// Merges `value` into `docs/evidence/<name>.json` under `place`.
fn write(name: &str, place: &str, value: &Value) {
    let path = repo().join("docs/evidence").join(format!("{name}.json"));
    let mut doc: Value = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| json!({ "evidence": name, "places": {} }));
    doc["places"][place] = value.clone();
    std::fs::write(&path, serde_json::to_string_pretty(&doc).expect("JSON") + "\n").expect("the evidence written");
    eprintln!("wrote {} ({place})", path.display());
}

fn krun_probe() -> Result<Value, String> {
    let image = docker_image()?;
    let tar = work_dir().join("tiny.tar");
    run(Command::new("docker").args(["save", "-o"]).arg(&tar).arg(&image))?;
    let loaded = krun::load_image(&image, &tar)?;
    let name = "fpac-probe-krun";
    let _ = krun::destroy(name);
    // The probe needs no remote; an intercept with no handler listening
    // would only fail a call it never makes.
    let started = krun::start(name, &image, &work_dir().join("no-handler.sock"))?;
    let e = krun::exec(name, &[PROBE_IN_GUEST, "probe"]);
    krun::destroy(name)?;
    let e = e?;
    let mut probe = guest_json(&e.stdout).map_err(|x| format!("{x}\nstderr: {}", e.stderr))?;
    probe["exit_code"] = json!(e.code);
    probe["place"] = json!({ "name": "krun", "what": "this box's sandcastle engine: a libkrun microVM per container, 2 vCPU, 6 GiB, 20 GB", "engine_timings_us": started["timings"], "resources": started["resources"], "image": image, "image_digest": loaded["digest"] });
    Ok(probe)
}

/// What local workerd sets on a computer's container, read from the ones
/// it started (when any is running): the privilege-bearing fields. Its
/// app container shares the network of a sidecar workerd starts beside
/// it; each is labeled by that.
fn workerd_host_config() -> Value {
    let ids = run(Command::new("docker").args(["ps", "--filter", "name=workerd-", "--format", "{{.ID}}"])).unwrap_or_default();
    let f = "{\"network\":{{json .HostConfig.NetworkMode}},\"privileged\":{{json .HostConfig.Privileged}},\"cap_add\":{{json .HostConfig.CapAdd}},\"cap_drop\":{{json .HostConfig.CapDrop}},\"devices\":{{json .HostConfig.Devices}},\"security_opt\":{{json .HostConfig.SecurityOpt}},\"runtime\":{{json .HostConfig.Runtime}}}";
    let mut seen = serde_json::Map::new();
    // Bounded: the running containers, and two roles.
    for id in ids.lines() {
        let Some(mut v) = run(Command::new("docker").args(["inspect", id, "--format", f])).ok().and_then(|s| serde_json::from_str::<Value>(s.trim()).ok()) else { continue };
        let role = if v["network"].as_str().is_some_and(|n| n.starts_with("container:")) { "app container (the computer)" } else { "network sidecar" };
        if let Some(o) = v.as_object_mut() {
            o.remove("network");
        }
        seen.entry(role).or_insert(v);
        if seen.len() == 2 {
            break;
        }
    }
    if seen.is_empty() {
        json!("no workerd container was running to read")
    } else {
        Value::Object(seen)
    }
}

fn docker_probe() -> Result<Value, String> {
    let image = docker_image()?;
    // No flag that grants anything: what a computer's container gets from
    // local workerd, which sets none (read below from a live one).
    let out = run(Command::new("docker").args(["run", "--rm", "--network", "none", "--entrypoint", PROBE_IN_GUEST, &image, "probe", "--check-only"]))?;
    let mut probe = guest_json(&out)?;
    let server = run(Command::new("docker").args(["version", "--format", "{{.Server.Version}}"])).unwrap_or_default();
    probe["place"] = json!({ "name": "docker", "what": "docker run with no privilege, capability or device added, as local workerd starts a computer's container; check-only, since it shares this box's kernel", "docker": server.trim(), "workerd_sets": workerd_host_config(), "image": image });
    Ok(probe)
}

fn cf_start(c: &Config, key: &net::Key, disk: &str) -> Result<Value, String> {
    let _ = net::worker(c.host(), key, "POST", "/destroy", &json!({}), Duration::from_secs(60));
    net::worker(c.host(), key, "POST", "/start", &json!({ "disk": disk, "image": "tiny", "epoch": now_ms() }), Duration::from_secs(300))
}

fn cf_exec(c: &Config, key: &net::Key, argv: &[&str]) -> Result<Value, String> {
    let r = net::worker(c.host(), key, "POST", "/exec", &json!({ "argv": argv, "ms": 600_000 }), Duration::from_secs(660))?;
    let stdout = r["stdout"].as_str().unwrap_or("");
    let mut v = guest_json(stdout).map_err(|e| format!("{e}\nstderr: {}", r["stderr"]))?;
    v["exec_ms"] = r["ms"].clone();
    Ok(v)
}

/// An exec whose stdout is wanted as text: (exit code, stdout).
fn cf_exec_raw(c: &Config, key: &net::Key, argv: &[&str], ms: u64) -> Result<(i64, String), String> {
    let r = net::worker(c.host(), key, "POST", "/exec", &json!({ "argv": argv, "ms": ms }), Duration::from_millis(ms + 60_000))?;
    Ok((r["exitCode"].as_i64().unwrap_or(-1), r["stdout"].as_str().unwrap_or("").to_string()))
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// Bad reads joined with what the handler recorded under their request id
/// (a record means R2's bytes were already wrong at the handler).
fn joined(mut check: Value, records: &Value, path: &str, since: u64) -> Value {
    let recs: Vec<Value> = records.as_array().cloned().unwrap_or_default().into_iter().filter(|r| r["path"] == json!(path) && r["at_ms"].as_u64().is_some_and(|t| t >= since)).collect();
    if let Some(bad) = check["bad"].as_array_mut() {
        for b in bad {
            let id = b["ids"]["x-fpac-request"].as_str().unwrap_or("").to_string();
            let seen = recs.iter().find(|r| r["id"] == json!(id)).cloned();
            b["handler_saw_r2_wrong"] = seen.unwrap_or(Value::Null);
        }
    }
    check["handler_records"] = json!(recs.len());
    check["handler_records_sample"] = json!(recs.into_iter().take(20).collect::<Vec<_>>());
    check
}

/// The volume range check (docs/spike.md, Results: wrong reads):
/// `cloudflare` runs the guest's check through the intercept, in the
/// background, polled; `outside` runs it from this machine against the
/// Worker's outside route, no container and no intercept.
fn rangecheck(place: &str, args: &[String]) -> Result<Value, String> {
    let c = Config::load()?;
    let key = net::Key::load_or_make(&c.driver_key_file);
    let (mode, pattern) = (flag(args, "--mode").unwrap_or("stream".into()), flag(args, "--pattern").unwrap_or("aligned".into()));
    let (count, threads, size_max) = (flag(args, "--count").unwrap_or("10000".into()), flag(args, "--threads").unwrap_or("16".into()), flag(args, "--size-max").unwrap_or((1u32 << 20).to_string()));
    let tag = format!("r{}", now_ms());
    // The handler's records are shared by runs on both places at once:
    // each run takes those of its own path and time.
    let since = now_ms();
    match place {
        "cloudflare" => {
            let started = cf_start(&c, &key, "spike-rc")?;
            // The job writes its exit code when it ends, however it ends.
            // `--client remote`: the product's own client, as the first
            // wrong reads were seen through (fpac-probe remotecheck)
            let command = match flag(args, "--client").as_deref() {
                Some("remote") => format!("{PROBE_IN_GUEST} remotecheck --count {count} --threads {threads} --whole-every {} --tag {tag} --out /tmp/rc.json", flag(args, "--whole-every").unwrap_or("200".into())),
                _ => format!("{PROBE_IN_GUEST} rangecheck --mode {mode} --pattern {pattern} --count {count} --threads {threads} --size-max {size_max} --tag {tag} --out /tmp/rc.json"),
            };
            let job = format!("rm -f /tmp/rc.json /tmp/rc.exit; setsid nohup sh -c '{command} 2> /tmp/rc.err; echo $? > /tmp/rc.exit' > /dev/null 2>&1 < /dev/null & echo started");
            let (code, out) = cf_exec_raw(&c, &key, &["sh", "-c", &job], 60_000)?;
            if code != 0 || !out.contains("started") {
                return Err(format!("the job did not start: {code} {out}"));
            }
            // Bounded: three hours of polls every 20 s.
            let mut finished = false;
            for _ in 0..540 {
                std::thread::sleep(Duration::from_secs(20));
                if let Ok((0, code)) = cf_exec_raw(&c, &key, &["cat", "/tmp/rc.exit"], 30_000) {
                    finished = code.trim() == "0";
                    break;
                }
                if let Ok((_, p)) = cf_exec_raw(&c, &key, &["tail", "-n", "1", "/tmp/rc.err"], 30_000) {
                    eprintln!("cloudflare {mode} {pattern}: {}", p.trim());
                }
            }
            let (_, text) = cf_exec_raw(&c, &key, &["cat", "/tmp/rc.json"], 60_000)?;
            let (_, err) = cf_exec_raw(&c, &key, &["tail", "-c", "2000", "/tmp/rc.err"], 30_000).unwrap_or_default();
            let records = net::worker(c.host(), &key, "GET", "/records", &json!({}), Duration::from_secs(60)).unwrap_or(Value::Null);
            let _ = net::worker(c.host(), &key, "POST", "/destroy", &json!({}), Duration::from_secs(60));
            let deleted = net::worker(c.host(), &key, "POST", "/objects/delete", &json!({ "prefix": "disks/spike-rc/" }), Duration::from_secs(300));
            let check = guest_json(&text).map_err(|e| format!("{e} (finished: {finished}); the job's stderr: {err}"))?;
            Ok(json!({ "place": { "name": "cloudflare", "what": "the guest -> its intercept -> the stand-in's handler -> R2", "start": started }, "check": joined(check, &records, "intercept", since), "bulk_data_deleted": deleted.ok() }))
        }
        "outside" => {
            use fpac_testkit::rangecheck::{self, Check, Dial, Mode, Pattern, ReadWrite};
            struct Outside<'a> {
                host: &'a str,
                key: &'a net::Key,
            }
            impl Dial for Outside<'_> {
                fn dial(&self) -> std::io::Result<Box<dyn ReadWrite>> {
                    Ok(Box::new(net::tls_stream(self.host, Duration::from_secs(60))?))
                }
                fn host(&self) -> &str {
                    self.host
                }
                fn headers(&self, method: &str, path: &str) -> Vec<(String, String)> {
                    vec![("authorization".into(), self.key.authorization(method, path, b""))]
                }
            }
            let object = fpac_testkit::conformance::noise(50 << 20, 500);
            let path = format!("/r2/repro/{tag}-rc");
            let sum = fpac_remote::hex(&fpac_remote::sha256(&object));
            let auth = key.authorization("PUT", &path, &object);
            let put = net::https(c.host(), "PUT", &path, &[("authorization", &auth), ("x-fpac-sha256", &sum)], &object, Duration::from_secs(600))?;
            if put.status != 201 {
                return Err(format!("the object's put: {} {}", put.status, put.json()));
            }
            let (Some(m), Some(p)) = (Mode::parse(&mode), Pattern::parse(&pattern)) else { return Err("--mode stream|tee|buffer, --pattern aligned|unaligned".into()) };
            let check = Check { path: &path, object: &object, reads: count.parse().map_err(|_| "--count")?, threads: threads.parse().map_err(|_| "--threads")?, mode: m, pattern: p, size_max: size_max.parse().map_err(|_| "--size-max")?, seed: 0x5eed_0002 };
            let outcome = rangecheck::run(&check, &Outside { host: c.host(), key: &key }, &|n, w, f| eprintln!("outside {mode} {pattern}: {n} reads, {w} wrong, {f} failed"));
            let records = net::worker(c.host(), &key, "GET", "/records", &json!({}), Duration::from_secs(60)).unwrap_or(Value::Null);
            let deleted = net::worker(c.host(), &key, "POST", "/objects/delete", &json!({ "prefix": "repro/" }), Duration::from_secs(300));
            Ok(json!({ "place": { "name": "outside", "what": "this machine -> TLS -> the Worker's fetch handler (signed) -> R2; no container, no intercept" }, "check": joined(rangecheck::evidence(&outcome, &check, &path), &records, "outside", since), "bulk_data_deleted": deleted.ok() }))
        }
        _ => Err("--place cloudflare|outside".into()),
    }
}

fn cf_probe() -> Result<Value, String> {
    let c = Config::load()?;
    let key = net::Key::load_or_make(&c.driver_key_file);
    let started = cf_start(&c, &key, "spike-probe")?;
    let probe = cf_exec(&c, &key, &[PROBE_IN_GUEST, "probe"]);
    let destroyed = net::worker(c.host(), &key, "POST", "/destroy", &json!({}), Duration::from_secs(60));
    let mut probe = probe?;
    probe["place"] = json!({ "name": "cloudflare", "what": "a Cloudflare container (a Durable Object's, the durable_object policy), 2 vCPU, 6 GiB, 20 GB, started by the stand-in Worker", "start": started, "destroyed": destroyed.is_ok() });
    Ok(probe)
}

fn cf_path() -> Result<Value, String> {
    let c = Config::load()?;
    let key = net::Key::load_or_make(&c.driver_key_file);
    let tag = format!("r{}", now_ms());
    let started = cf_start(&c, &key, "spike-path")?;
    let path = cf_exec(&c, &key, &[PROBE_IN_GUEST, "path", "--tag", &tag]);
    let conformance = cf_exec(&c, &key, &[PROBE_IN_GUEST, "conformance", "--tag", &format!("{tag}c")]);
    let _ = net::worker(c.host(), &key, "POST", "/destroy", &json!({}), Duration::from_secs(60));
    let usage = net::worker(c.host(), &key, "POST", "/objects/usage", &json!({ "prefix": "disks/spike-path/" }), Duration::from_secs(120));
    let deleted = net::worker(c.host(), &key, "POST", "/objects/delete", &json!({ "prefix": "disks/spike-path/" }), Duration::from_secs(300));
    let after = net::worker(c.host(), &key, "POST", "/objects/usage", &json!({ "prefix": "disks/spike-path/" }), Duration::from_secs(120));
    // The bucket's location (a region hint such as ENAM): with the colos,
    // it says how far the path travels.
    let location = net::api(&c.token(), "GET", &format!("/accounts/{}/r2/buckets/{}", c.account_id, c.bucket_name), None).ok().map(|(_, v)| v["result"]["location"].clone());
    Ok(json!({
        "place": { "name": "cloudflare", "what": "the guest in a Cloudflare container -> its intercept -> the stand-in Worker's handler on the container's machine -> an R2 binding", "start": started, "bucket_location": location },
        "path": path?, "conformance": conformance?,
        "bulk_data": { "written": usage.ok(), "deleted": deleted.ok(), "left": after.ok() },
    }))
}

/// Ranged reads checked byte for byte on Cloudflare (`ranges.json`).
fn cf_ranges(count: usize, diagnose: bool) -> Result<Value, String> {
    let c = Config::load()?;
    let key = net::Key::load_or_make(&c.driver_key_file);
    let tag = format!("r{}", now_ms());
    let started = cf_start(&c, &key, "spike-ranges")?;
    let n = count.to_string();
    let mut argv = vec![PROBE_IN_GUEST, "ranges", "--tag", &tag, "--count", &n];
    if diagnose {
        argv.push("--diagnose");
    }
    let ranges = cf_exec(&c, &key, &argv);
    let _ = net::worker(c.host(), &key, "POST", "/destroy", &json!({}), Duration::from_secs(60));
    let deleted = net::worker(c.host(), &key, "POST", "/objects/delete", &json!({ "prefix": "disks/spike-ranges/" }), Duration::from_secs(300));
    let location = net::api(&c.token(), "GET", &format!("/accounts/{}/r2/buckets/{}", c.account_id, c.bucket_name), None).ok().map(|(_, v)| v["result"]["location"].clone());
    Ok(json!({ "place": { "name": "cloudflare", "start": started, "bucket_location": location }, "ranges": ranges?, "bulk_data_deleted": deleted.ok() }))
}

fn krun_path() -> Result<Value, String> {
    let image = docker_image()?;
    let tar = work_dir().join("tiny.tar");
    run(Command::new("docker").args(["save", "-o"]).arg(&tar).arg(&image))?;
    krun::load_image(&image, &tar)?;
    let dir = work_dir().join("krun");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("remote")).map_err(|e| e.to_string())?;
    let sock = dir.join("handler.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).map_err(|e| e.to_string())?;
    let _remote = fpac_testkit::folder::Folder::new(&dir.join("remote"), "spike-path", now_ms()).start(fpac_testkit::folder::Listen::Unix(listener));
    let name = "fpac-path-krun";
    let _ = krun::destroy(name);
    let started = krun::start(name, &image, &sock)?;
    let tag = format!("r{}", now_ms());
    let path = krun::exec(name, &[PROBE_IN_GUEST, "path", "--tag", &tag]);
    let conformance = krun::exec(name, &[PROBE_IN_GUEST, "conformance", "--tag", &format!("{tag}c")]);
    krun::destroy(name)?;
    let _ = std::fs::remove_dir_all(&dir);
    let (path, conformance) = (path?, conformance?);
    Ok(json!({
        "place": { "name": "krun", "what": "the guest in a krun microVM -> the engine's intercept -> the folder remote on a unix socket on this box: a STAND-IN for the route and for R2, never the product's number", "engine_timings_us": started["timings"] },
        "path": guest_json(&path.stdout).map_err(|e| format!("{e}\nstderr: {}", path.stderr))?,
        "conformance": guest_json(&conformance.stdout)?,
        "exit_codes": [path.code, conformance.code],
    }))
}

/// The Worker staged with its config rendered from the local one.
fn stage_worker(c: &Config, key: &net::Key, image_ctx: &Path) -> Result<PathBuf, String> {
    let dir = work_dir().join("worker");
    std::fs::create_dir_all(dir.join("src")).map_err(|e| e.to_string())?;
    let src = repo().join("worker");
    for f in ["package.json", "package-lock.json", "src/index.mjs"] {
        std::fs::copy(src.join(f), dir.join(f)).map_err(|e| format!("{f}: {e}"))?;
    }
    let mut cfg = std::fs::read_to_string(src.join("wrangler.template.jsonc")).map_err(|e| e.to_string())?;
    for (k, v) in [("WORKER_NAME", c.worker_name.clone()), ("BUCKET_NAME", c.bucket_name.clone()), ("DRIVER_PUBLIC_KEY", key.public_hex()), ("IMAGE_DOCKERFILE", image_ctx.join("Dockerfile").display().to_string()), ("IMAGE_CONTEXT", image_ctx.display().to_string())] {
        cfg = cfg.replace(&format!("${{{k}}}"), &v);
    }
    assert!(!cfg.contains("${"), "every placeholder filled");
    std::fs::write(dir.join("wrangler.jsonc"), cfg).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// wrangler, on the pinned Node, with the token in its environment only.
fn wrangler(c: &Config, dir: &Path, args: &[&str], log: &Path) -> Result<String, String> {
    let path = format!("{}:{}", c.node_dir.join("bin").display(), std::env::var("PATH").unwrap_or_default());
    let out = Command::new(c.node_dir.join("bin/node"))
        .current_dir(dir)
        .arg("node_modules/wrangler/bin/wrangler.js")
        .args(args)
        .env("PATH", path)
        .env("CLOUDFLARE_API_TOKEN", c.token())
        .env("CLOUDFLARE_ACCOUNT_ID", &c.account_id)
        .env("WRANGLER_SEND_METRICS", "false")
        .output()
        .map_err(|e| e.to_string())?;
    let text = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    std::fs::write(log, &text).map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!("wrangler {args:?} failed ({}); its log: {}", out.status, log.display()))
    }
}

fn deploy() -> Result<Value, String> {
    let c = Config::load()?;
    let key = net::Key::load_or_make(&c.driver_key_file);
    let (ctx, tag) = stage()?;
    let dir = stage_worker(&c, &key, &ctx)?;
    if !dir.join("node_modules/wrangler").exists() {
        let path = format!("{}:{}", c.node_dir.join("bin").display(), std::env::var("PATH").unwrap_or_default());
        run(Command::new(c.node_dir.join("bin/npm")).current_dir(&dir).args(["ci", "--no-audit", "--no-fund"]).env("PATH", path))?;
    }
    let token = c.token();
    let a = &c.account_id;
    let (s, _) = net::api(&token, "GET", &format!("/accounts/{a}/r2/buckets/{}", c.bucket_name), None)?;
    let bucket = if s == 404 {
        let mut body = json!({ "name": c.bucket_name });
        if let Some(hint) = &c.bucket_location_hint {
            body["locationHint"] = json!(hint);
        }
        let (s, v) = net::api(&token, "POST", &format!("/accounts/{a}/r2/buckets"), Some(&body))?;
        if s != 200 {
            return Err(format!("the bucket: {s} {v}"));
        }
        "made"
    } else {
        "there"
    };
    let log = work_dir().join(format!("deploy-{}.log", now_ms()));
    let out = wrangler(&c, &dir, &["deploy"], &log)?;
    let image = out.lines().find(|l| l.contains("@sha256:")).and_then(|l| l.split('@').nth(1)).map(|d| d.trim().to_string());
    let version = out.lines().find_map(|l| l.trim().strip_prefix("Current Version ID:")).map(|v| v.trim().to_string()).ok_or("no version in wrangler's output")?;
    // The new version must answer, the Durable Object included, before a
    // measurement may run: a deploy reaches the network over a minute.
    for _ in 0..120 {
        if let Ok(st) = net::worker(c.host(), &key, "GET", "/state", &json!({}), Duration::from_secs(10)) {
            if st["version"] == json!(version) {
                return Ok(json!({ "deployed": true, "version": version, "bucket": bucket, "image_tag": tag, "image_digest": image, "log": log }));
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Err(format!("version {version} does not answer two minutes after its deploy; log {}", log.display()))
}

/// The account's Workers, buckets and container applications, by name
/// (kept in the work directory, never in the repository).
fn inventory(label: &str) -> Result<Value, String> {
    let c = Config::load()?;
    let token = c.token();
    let a = &c.account_id;
    let names = |path: &str, field: &str, list: &dyn Fn(&Value) -> Option<&Vec<Value>>| -> Result<Vec<String>, String> {
        let (s, v) = net::api(&token, "GET", path, None)?;
        if s != 200 {
            return Err(format!("{path}: {s}"));
        }
        let mut n: Vec<String> = list(&v).map(|l| l.iter().filter_map(|x| x[field].as_str().map(String::from)).collect()).unwrap_or_default();
        n.sort();
        Ok(n)
    };
    let inv = json!({
        "label": label, "at_ms": now_ms(),
        "workers": names(&format!("/accounts/{a}/workers/scripts"), "id", &|v| v["result"].as_array())?,
        "buckets": names(&format!("/accounts/{a}/r2/buckets"), "name", &|v| v["result"]["buckets"].as_array())?,
        "container_apps": names(&format!("/accounts/{a}/containers/applications"), "name", &|v| v["result"].as_array())?,
    });
    let path = work_dir().join(format!("inventory-{label}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&inv).expect("JSON")).map_err(|e| e.to_string())?;
    let ours = |k: &str| inv[k].as_array().map(|l| l.iter().filter(|n| n.as_str().is_some_and(|n| n.starts_with(c.worker_name.as_str()) || n.starts_with(c.bucket_name.as_str()))).count()).unwrap_or(0);
    let all = |k: &str| inv[k].as_array().map(|l| l.len()).unwrap_or(0);
    Ok(json!({ "label": label, "kept_locally": path, "workers": { "all": all("workers"), "spike": ours("workers") }, "buckets": { "all": all("buckets"), "spike": ours("buckets") }, "container_apps": { "all": all("container_apps"), "spike": ours("container_apps") } }))
}

/// The one cleanup: the spike's objects, Worker, container application,
/// registry images and bucket, each by its name from the local config,
/// then the local work directory. Nothing else is touched.
fn reset() -> Result<Value, String> {
    let c = Config::load()?;
    let key = net::Key::load_or_make(&c.driver_key_file);
    let token = c.token();
    let a = &c.account_id;
    let mut done = Vec::new();
    // the bucket is emptied while the Worker that can list it still runs
    let _ = net::worker(c.host(), &key, "POST", "/destroy", &json!({}), Duration::from_secs(60));
    done.push(json!({ "objects": net::worker(c.host(), &key, "POST", "/objects/delete", &json!({ "prefix": "disks/" }), Duration::from_secs(600)).map_err(|e| e.to_string()) }));
    let (s, _) = net::api(&token, "DELETE", &format!("/accounts/{a}/workers/scripts/{}?force=true", c.worker_name), None)?;
    done.push(json!({ "worker": s }));
    let (_, apps) = net::api(&token, "GET", &format!("/accounts/{a}/containers/applications"), None)?;
    for app in apps["result"].as_array().cloned().unwrap_or_default() {
        let (name, id) = (app["name"].as_str().unwrap_or(""), app["id"].as_str().unwrap_or(""));
        if name.starts_with(&c.worker_name) && !id.is_empty() {
            let (s, _) = net::api(&token, "DELETE", &format!("/accounts/{a}/containers/applications/{id}"), None)?;
            done.push(json!({ "container_app": s }));
        }
    }
    let dir = work_dir().join("worker");
    if dir.join("node_modules/wrangler").exists() {
        let log = work_dir().join("reset-images.log");
        if let Ok(out) = wrangler(&c, &dir, &["containers", "images", "list", "--json"], &log) {
            let (start, end) = (out.find('[').unwrap_or(0), out.rfind(']').map(|e| e + 1).unwrap_or(out.len()));
            let list: Value = serde_json::from_str(out.get(start..end).unwrap_or("")).unwrap_or(Value::Null);
            for img in list.as_array().cloned().unwrap_or_default() {
                let repo_name = img["name"].as_str().unwrap_or("");
                if repo_name.starts_with(&c.worker_name) {
                    for t in img["tags"].as_array().cloned().unwrap_or_default() {
                        let r = wrangler(&c, &dir, &["containers", "images", "delete", &format!("{repo_name}:{}", t.as_str().unwrap_or(""))], &log);
                        done.push(json!({ "image": r.is_ok() }));
                    }
                }
            }
        }
    }
    // the configured bucket, and any other the spike made beside it under
    // the same name (a second location, say), each emptied by now
    let (_, buckets) = net::api(&token, "GET", &format!("/accounts/{a}/r2/buckets"), None)?;
    for b in buckets["result"]["buckets"].as_array().cloned().unwrap_or_default() {
        let name = b["name"].as_str().unwrap_or("");
        if name.starts_with(&c.bucket_name) {
            let (s, _) = net::api(&token, "DELETE", &format!("/accounts/{a}/r2/buckets/{name}"), None)?;
            done.push(json!({ "bucket": s }));
        }
    }
    let _ = std::fs::remove_dir_all(work_dir());
    Ok(json!({ "reset": done }))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let place = args.iter().position(|a| a == "--place").and_then(|i| args.get(i + 1)).map(String::as_str).unwrap_or("");
    let write_it = args.iter().any(|a| a == "--write");
    // `--label <l>`: the evidence goes under `<place>-<l>`, so two runs of
    // one place (two buckets, say) sit side by side.
    let label = args.iter().position(|a| a == "--label").and_then(|i| args.get(i + 1));
    let key = label.map(|l| format!("{place}-{l}")).unwrap_or_else(|| place.to_string());
    let result = match (args.get(1).map(String::as_str), place) {
        (Some("stage"), _) => stage().map(|(d, t)| json!({ "context": d, "tag": t })),
        (Some("probe"), "krun") => krun_probe(),
        (Some("probe"), "docker") => docker_probe(),
        (Some("probe"), "cloudflare") => cf_probe(),
        (Some("path"), "krun") => krun_path(),
        (Some("path"), "cloudflare") => cf_path(),
        (Some("rangecheck"), p) => rangecheck(p, &args),
        // A command in the running Cloudflare container, for debugging a run.
        (Some("exec"), _) => Config::load().and_then(|c| {
            let key = net::Key::load_or_make(&c.driver_key_file);
            let argv: Vec<&str> = args[2..].iter().map(String::as_str).collect();
            net::worker(c.host(), &key, "POST", "/exec", &json!({ "argv": argv, "ms": 60_000 }), Duration::from_secs(120))
        }),
        (Some("destroy"), _) => Config::load().and_then(|c| net::worker(c.host(), &net::Key::load_or_make(&c.driver_key_file), "POST", "/destroy", &json!({}), Duration::from_secs(60))),
        (Some("ranges"), "cloudflare") => cf_ranges(args.iter().position(|a| a == "--count").and_then(|i| args.get(i + 1)).and_then(|c| c.parse().ok()).unwrap_or(1000), args.iter().any(|a| a == "--diagnose")),
        (Some("deploy"), _) => deploy(),
        (Some("inventory"), _) => inventory(args.get(2).map(String::as_str).unwrap_or("now")),
        (Some("reset"), _) => reset(),
        (Some("engine"), _) => krun::list(),
        _ => Err("usage: fpac-bench stage | probe --place krun|docker|cloudflare | path --place krun|cloudflare [--write] | deploy | inventory <label> | reset".into()),
    };
    match result {
        Ok(mut v) => {
            if write_it && ["probe", "path", "ranges", "rangecheck"].contains(&args[1].as_str()) {
                v["measured_at_ms"] = json!(now_ms());
                v["commit"] = commit();
                // the outside place writes a file of its own, so the two
                // places can run at once
                let name = if args[1] == "rangecheck" && place == "outside" { "rangecheck-outside".to_string() } else { args[1].clone() };
                write(&name, &key, &v);
            }
            println!("{}", serde_json::to_string_pretty(&v).expect("JSON"));
        }
        Err(e) => {
            eprintln!("fpac-bench: {e}");
            std::process::exit(1);
        }
    }
}
