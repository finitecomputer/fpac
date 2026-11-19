// fpac's spike Worker (docs/spike.md). It stands in for a platform's
// route from a container to its disk's objects, and is named so in every
// result: the product's route lives in the platform's own Worker. It is
// JavaScript because workers-rs has no Containers.
//
// Three parts:
// - `FpacRemote`, the egress handler: the container's plain HTTP to
//   fpac.fragment.internal lands here, on the container's own machine,
//   and is answered from R2 (docs/contract.md). It knows the disk and the
//   life's epoch from props the Durable Object set; the guest sends
//   neither and holds no credential.
// - `FpacSpike`, one Durable Object with one container: start, exec,
//   destroy, and the bucket's housekeeping, for the driver.
// - The fetch handler: `GET /` describes the spike to anyone; every other
//   route answers only a request the driver signed (ECDSA P-256; the
//   public half is a plain var, so the deployment holds no secret).

import { DurableObject, WorkerEntrypoint } from "cloudflare:workers";

const REMOTE_HOST = "fpac.fragment.internal";
// docs/contract.md, Limits. One object's bytes: under the intercept's
// 100 MiB request body limit.
const OBJECT_BYTES_MAX = 96 * 1024 * 1024;
const NAME = /^e([1-9][0-9]{0,19})\/[a-z][a-z0-9.-]{0,90}$/;
const RANGE = /^bytes=(\d{1,15})-(\d{1,15})$/;
const SHA256_HEX = /^[0-9a-f]{64}$/;
// The driver's calls.
const SIGNED_SKEW_MS_MAX = 5 * 60 * 1000;
const DRIVER_BODY_BYTES_MAX = 64 * 1024;
const EXEC_MS_MAX = 10 * 60 * 1000;
const EXEC_OUTPUT_BYTES_MAX = 4 * 1024 * 1024;
const READY_MS_MAX = 120 * 1000;
// A delete pass lists at most this many pages of 1,000 keys.
const LIST_PAGES_MAX = 200;
const INSTANCE = { vcpu: 2, memoryMib: 6144, diskMb: 20000 };
const NULL_BYTES_MAX = 1024 * 1024;
// `server-timing: r2;dur=<ms>`: what the R2 call itself took, so the
// driver can put a slow call on the intercept or on the store.
const timing = (t0) => `r2;dur=${Date.now() - t0}`;

const ABOUT = `fpac spike

A lazy disk for long-lived computers: ext4, made by the kernel, on a loop
device over one FUSE file, its blocks in R2 as write-once objects. The
container reaches R2 through an intercepted host answered by this Worker,
on the container's own machine, and holds no credential.

This Worker stands in for a platform's route. Its other routes answer
only signed calls from the spike's driver.

Source and evidence: https://github.com/futurepaul/fpac
`;

const hex = (buf) => [...new Uint8Array(buf)].map((b) => b.toString(16).padStart(2, "0")).join("");
const unhex = (s) => new Uint8Array(s.match(/../g).map((h) => parseInt(h, 16)));
const text = (status, body, headers = {}) => new Response(body, { status, headers: { "content-type": "text/plain; charset=utf-8", ...headers } });
const json = (value, status = 200) => new Response(JSON.stringify(value), { status, headers: { "content-type": "application/json" } });

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/") return text(200, ABOUT);
    const length = Number(request.headers.get("content-length") || "0");
    if (!(length <= DRIVER_BODY_BYTES_MAX)) return text(413, "too large\n");
    const body = new Uint8Array(await request.arrayBuffer());
    if (body.length > DRIVER_BODY_BYTES_MAX) return text(413, "too large\n");
    if (!(await signed(request, url, body, env))) return text(401, "this route answers the spike's driver only\n");
    const stub = env.SPIKE.get(env.SPIKE.idFromName("spike"));
    const headers = { "x-driver-colo": String((request.cf && request.cf.colo) || "unknown") };
    return stub.fetch(new Request(url, { method: request.method, headers, body: request.method === "GET" ? undefined : body }));
  },
};

// `authorization: FpacSpike <unix ms>.<r||s hex>` over
// "fpac-spike\n<method>\n<path and query>\n<unix ms>\n<sha256 hex of the body>".
async function signed(request, url, body, env) {
  const m = /^FpacSpike (\d{13})\.([0-9a-f]{128})$/.exec(request.headers.get("authorization") || "");
  if (!m || !/^04[0-9a-f]{128}$/.test(env.DRIVER_PUBLIC_KEY || "")) return false;
  if (Math.abs(Date.now() - Number(m[1])) > SIGNED_SKEW_MS_MAX) return false;
  const digest = hex(await crypto.subtle.digest("SHA-256", body));
  const message = new TextEncoder().encode(`fpac-spike\n${request.method}\n${url.pathname}${url.search}\n${m[1]}\n${digest}`);
  const key = await crypto.subtle.importKey("raw", unhex(env.DRIVER_PUBLIC_KEY), { name: "ECDSA", namedCurve: "P-256" }, false, ["verify"]);
  return crypto.subtle.verify({ name: "ECDSA", hash: "SHA-256" }, key, unhex(m[2]), message);
}

// The remote's calls (docs/contract.md), for the one disk and the one
// epoch in props. Objects live at `disks/<disk>/<name>`.
export class FpacRemote extends WorkerEntrypoint {
  async fetch(request) {
    const { disk, epoch } = this.ctx.props;
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/open") {
      return text(200, "", { "x-fpac-disk": disk, "x-fpac-epoch": String(epoch) });
    }
    // The stand-in's own diagnostic, outside the contract: the intercept's
    // round trip with no store behind it, and where the handler ran.
    if (request.method === "GET" && url.pathname === "/null") {
      const n = Math.min(Number(url.searchParams.get("bytes")) || 0, NULL_BYTES_MAX);
      return new Response(new Uint8Array(n), { status: 200, headers: { "content-length": String(n), "x-fpac-colo": String((request.cf && request.cf.colo) || "unknown") } });
    }
    const name = url.pathname.startsWith("/o/") ? url.pathname.slice(3) : null;
    if (name === null) return text(404, "no such route\n");
    const named = NAME.exec(name);
    if (!named) return text(400, "a name outside the grammar\n");
    const key = `disks/${disk}/${name}`;
    if (request.method === "GET") return get(this.env.DISKS, key, request.headers.get("range"), request.headers.get("x-fpac-diagnose") === "sha256");
    if (request.method === "PUT") {
      if (named[1] !== String(epoch)) return text(403, "a put names another life's epoch\n");
      return put(this.env.DISKS, key, request);
    }
    return text(405, "GET or PUT\n");
  }
}

// `diagnose`: the stand-in's own diagnostic, outside the contract. The
// range is read whole here and sent with the SHA-256 of what was sent
// (`x-fpac-sent-sha256`), so a wrong read can be put on R2's side of the
// handler or on the path from the handler to the guest.
async function get(bucket, key, range, diagnose) {
  const t0 = Date.now();
  if (range === null) {
    const o = await bucket.get(key);
    if (o === null) return text(404, "absent\n");
    const sum = o.checksums.sha256;
    if (!sum) return text(500, "an object with no checksum\n");
    return new Response(o.body, { status: 200, headers: { "content-length": String(o.size), "x-fpac-sha256": hex(sum), "server-timing": timing(t0) } });
  }
  const r = RANGE.exec(range);
  if (!r || Number(r[2]) < Number(r[1])) return text(400, "a range we do not serve\n");
  const offset = Number(r[1]);
  let o;
  try {
    o = await bucket.get(key, { range: { offset, length: Number(r[2]) - offset + 1 } });
  } catch (e) {
    // R2 refuses a range that starts past the end
    return text(416, `the range: ${String((e && e.message) || e)}\n`);
  }
  if (o === null) return text(404, "absent\n");
  if (offset >= o.size && o.size > 0) return text(416, "the range starts past the end\n");
  const length = Math.max(0, Math.min(Number(r[2]), o.size - 1) - offset + 1);
  if (diagnose) {
    const bytes = await o.arrayBuffer();
    const sent = hex(await crypto.subtle.digest("SHA-256", bytes));
    return new Response(bytes, { status: 206, headers: { "content-length": String(bytes.byteLength), "content-range": `bytes ${offset}-${offset + length - 1}/${o.size}`, "server-timing": timing(t0), "x-fpac-sent-sha256": sent } });
  }
  return new Response(o.body, { status: 206, headers: { "content-length": String(length), "content-range": `bytes ${offset}-${offset + length - 1}/${o.size}`, "server-timing": timing(t0) } });
}

// Put-if-absent. R2 checks the body against the SHA-256 we pass, and
// refuses the put when the name is taken; then the stored object's
// checksum tells our own earlier put (200) from another writer's (409).
async function put(bucket, key, request) {
  const claimed = request.headers.get("x-fpac-sha256") || "";
  if (!SHA256_HEX.test(claimed)) return text(400, "a put names its SHA-256\n");
  const length = Number(request.headers.get("content-length"));
  if (!Number.isSafeInteger(length) || length < 0) return text(411, "a put names its length\n");
  if (length > OBJECT_BYTES_MAX) return text(413, "an object past its limit\n");
  // A stream of the length the request named, so R2 takes it as one.
  const fixed = new FixedLengthStream(length);
  // Not awaited: a put refused by its condition may leave the body unread.
  (request.body || new Response(new Uint8Array(0)).body).pipeTo(fixed.writable).catch(() => {});
  let stored;
  const t0 = Date.now();
  try {
    stored = await bucket.put(key, fixed.readable, { sha256: claimed, onlyIf: new Headers({ "if-none-match": "*" }) });
  } catch (e) {
    const why = String((e && e.message) || e);
    return text(/sha|checksum|digest/i.test(why) ? 400 : 503, `the put: ${why}\n`);
  }
  if (stored !== null) return text(201, "", { "x-fpac-sha256": claimed, "server-timing": timing(t0) });
  const there = await bucket.head(key);
  if (there === null || !there.checksums.sha256) return text(503, "the name was taken, then gone\n");
  const sum = hex(there.checksums.sha256);
  return text(sum === claimed ? 200 : 409, "", { "x-fpac-sha256": sum, "server-timing": timing(t0) });
}

export class FpacSpike extends DurableObject {
  async fetch(request) {
    const url = new URL(request.url);
    try {
      const route = `${request.method} ${url.pathname}`;
      if (route === "POST /start") return json({ ...(await this.start(await request.json())), driver_colo: request.headers.get("x-driver-colo") });
      if (route === "POST /exec") return json(await this.exec(await request.json()));
      if (route === "POST /destroy") return json(await this.destroy());
      if (route === "GET /state") return json({ version: this.env.VERSION.id, running: this.ctx.container.running, images: Object.keys(this.ctx.container.images || {}) });
      if (route === "POST /objects/delete") return json(await this.deleteObjects(await request.json()));
      if (route === "POST /objects/usage") return json(await this.usage(await request.json()));
      return text(404, "no such route\n");
    } catch (e) {
      return json({ error: String((e && e.stack) || e) }, 500);
    }
  }

  // Starts the container from the named image, its intercept registered
  // first where the runtime allows it (so the guest's first call never
  // races it), and waits until it takes an exec.
  async start({ disk, image, epoch }) {
    const c = this.ctx.container;
    if (c.running) throw new Error("a container is running: one at a time");
    if (!/^[a-z0-9-]{1,40}$/.test(disk)) throw new Error("a disk's name: a-z, 0-9 and -");
    if (!Number.isSafeInteger(epoch) || epoch < 1) throw new Error("an epoch is a positive integer");
    const ref = c.images && c.images[image];
    if (!ref) throw new Error(`no image named ${image}`);
    const egress = this.ctx.exports.FpacRemote({ props: { disk, epoch } });
    const t0 = Date.now();
    let intercept = "before the start";
    try {
      await c.interceptOutboundHttp(REMOTE_HOST, egress);
    } catch (e) {
      intercept = `after the start (before it: ${String((e && e.message) || e)})`;
    }
    c.start({ image: ref, enableInternet: false, instance: INSTANCE, env: { FPAC_REMOTE: REMOTE_HOST } });
    c.monitor().catch(() => {});
    if (intercept !== "before the start") await c.interceptOutboundHttp(REMOTE_HOST, egress);
    const started = Date.now() - t0;
    const ready = await this.until(READY_MS_MAX);
    // Where this object (and its container, placed beside it) runs: the
    // colo Cloudflare's trace names for a subrequest from here.
    const where = await fetch("https://www.cloudflare.com/cdn-cgi/trace").then((r) => r.text()).then((t) => (/^colo=(\w+)$/m.exec(t) || [])[1] || "unknown", () => "unknown");
    // The image's digest only: its reference names the account.
    const digest = String(ref).split("@")[1] || null;
    return { version: this.env.VERSION.id, disk, epoch, image, image_digest: digest, intercept, start_call_ms: started, ready, instance: INSTANCE, object_colo: where };
  }

  async until(ms) {
    const t0 = Date.now();
    let tries = 0;
    let last = null;
    while (Date.now() - t0 < ms) {
      tries++;
      try {
        const r = await this.exec({ argv: ["true"], ms: 5000 });
        if (r.exitCode === 0) return { ok: true, ms: Date.now() - t0, tries };
        last = `exit ${r.exitCode}`;
      } catch (e) {
        last = String((e && e.message) || e).slice(0, 300);
      }
      await new Promise((r) => setTimeout(r, 100));
    }
    return { ok: false, ms: Date.now() - t0, tries, last };
  }

  // Runs argv in the container: its exit code, its stdout and stderr (each
  // at most EXEC_OUTPUT_BYTES_MAX), and its time. Killed past `ms`.
  async exec({ argv, ms }) {
    const c = this.ctx.container;
    if (!c.running) throw new Error("no container is running");
    if (!Array.isArray(argv) || argv.length === 0) throw new Error("argv is a list");
    const bound = Math.min(Number(ms) || 60000, EXEC_MS_MAX);
    const kill = new AbortController();
    const timer = setTimeout(() => kill.abort(), bound);
    const t0 = Date.now();
    try {
      const proc = await c.exec(argv, { stdout: "pipe", stderr: "pipe", signal: kill.signal });
      const [stdout, stderr] = await Promise.all([readAll(proc.stdout), readAll(proc.stderr)]);
      return { exitCode: await proc.exitCode, stdout, stderr, ms: Date.now() - t0 };
    } finally {
      clearTimeout(timer);
    }
  }

  async destroy() {
    const c = this.ctx.container;
    const t0 = Date.now();
    if (c.running) await c.destroy("the driver asked").catch(() => {});
    while (c.running && Date.now() - t0 < 30000) await new Promise((r) => setTimeout(r, 100));
    return { running: c.running, ms: Date.now() - t0 };
  }

  // Deletes every object under `prefix` (which must be under `disks/`):
  // the driver's cleanup of bulk test data, never the guest's.
  async deleteObjects({ prefix }) {
    if (typeof prefix !== "string" || !/^disks\/([a-z0-9-]+\/)?$/.test(prefix)) throw new Error("a prefix is disks/ or disks/<disk>/");
    let deleted = 0;
    for (let page = 0; page < LIST_PAGES_MAX; page++) {
      const l = await this.env.DISKS.list({ prefix, limit: 1000 });
      if (l.objects.length === 0) return { deleted, pages: page };
      await this.env.DISKS.delete(l.objects.map((o) => o.key));
      deleted += l.objects.length;
    }
    throw new Error(`more than ${LIST_PAGES_MAX} pages under ${prefix}`);
  }

  async usage({ prefix }) {
    if (typeof prefix !== "string" || !prefix.startsWith("disks/")) throw new Error("a prefix under disks/");
    let objects = 0;
    let bytes = 0;
    let cursor;
    for (let page = 0; page < LIST_PAGES_MAX; page++) {
      const l = await this.env.DISKS.list({ prefix, limit: 1000, cursor });
      objects += l.objects.length;
      for (const o of l.objects) bytes += o.size;
      if (!l.truncated) return { objects, bytes };
      cursor = l.cursor;
    }
    throw new Error(`more than ${LIST_PAGES_MAX} pages under ${prefix}`);
  }
}

async function readAll(stream) {
  if (!stream) return "";
  const parts = [];
  let n = 0;
  for await (const chunk of stream) {
    if (n + chunk.length > EXEC_OUTPUT_BYTES_MAX) throw new Error("output past its limit");
    parts.push(chunk);
    n += chunk.length;
  }
  const all = new Uint8Array(n);
  let at = 0;
  for (const p of parts) {
    all.set(p, at);
    at += p.length;
  }
  return new TextDecoder().decode(all);
}
