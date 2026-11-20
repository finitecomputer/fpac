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
// The images a start may give the internet (`start`): the npm benchmark's.
const INTERNET_IMAGES = ["npm"];
// A disk's size when the driver names none: 20 GB, as a computer's.
const VOLUME_BYTES_DEFAULT = 20000 * 1024 * 1024 - ((20000 * 1024 * 1024) % 65536);
const NULL_BYTES_MAX = 1024 * 1024;
// The relay (docs/results.md, Hermes on Cloudflare): a container's calls
// to these hosts go, through the Durable Object, over one WebSocket the
// driver opens, to the stand-ins on the driver's machine (fragment's fake
// API and the scripted model): STAND-INS, not fragment.
const RELAY_HOSTS = ["api.fragment.internal", "model.fragment.internal"];
const RELAY_MS_MAX = 120 * 1000;
const RELAY_CHUNK_BYTES = 512 * 1024;
const RELAY_BODY_BYTES_MAX = 64 * 1024 * 1024;
const ENV_NAME = /^[A-Z][A-Z0-9_]{0,63}$/;
const ENV_VARS_MAX = 32;
const REPRO_KEY = /^repro\/[a-z0-9.-]{1,64}$/;
const EMPTY_SHA256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
// Records of mismatches the handler saw, kept and listed at most.
const RECORDS_MAX = 1000;
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
  async fetch(request, env, ctx) {
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/") return text(200, ABOUT);
    // The outside route (docs/spike.md, Results: wrong reads): the same
    // ranged get as the container's, from a client outside Cloudflare,
    // with no container and no intercept between R2 and the client. Its
    // objects live under `repro/`. A put is signed over the SHA-256 it
    // names, which R2 checks against the body.
    if (url.pathname.startsWith("/r2/")) {
      const key = url.pathname.slice(4);
      if (!REPRO_KEY.test(key)) return text(400, "a key under repro/\n");
      const digest = request.method === "PUT" ? request.headers.get("x-fpac-sha256") || "" : EMPTY_SHA256;
      if (!(await signed(request, url, digest, env))) return text(401, "this route answers the spike's driver only\n");
      if (request.method === "GET") return get(env, ctx, key, request, "outside");
      if (request.method === "PUT") return put(env.DISKS, key, request);
      return text(405, "GET or PUT\n");
    }
    const length = Number(request.headers.get("content-length") || "0");
    if (!(length <= DRIVER_BODY_BYTES_MAX)) return text(413, "too large\n");
    const body = new Uint8Array(await request.arrayBuffer());
    if (body.length > DRIVER_BODY_BYTES_MAX) return text(413, "too large\n");
    if (!(await signed(request, url, hex(await crypto.subtle.digest("SHA-256", body)), env))) return text(401, "this route answers the spike's driver only\n");
    const stub = env.SPIKE.get(env.SPIKE.idFromName("spike"));
    const headers = { "x-driver-colo": String((request.cf && request.cf.colo) || "unknown") };
    // the driver's relay: a WebSocket to the Durable Object
    if (url.pathname === "/relay" && request.headers.get("upgrade") === "websocket") {
      return stub.fetch(new Request(url, { method: "GET", headers: { ...headers, upgrade: "websocket" } }));
    }
    return stub.fetch(new Request(url, { method: request.method, headers, body: request.method === "GET" ? undefined : body }));
  },
};

// `authorization: FpacSpike <unix ms>.<r||s hex>` over
// "fpac-spike\n<method>\n<path and query>\n<unix ms>\n<sha256 hex of the body>".
async function signed(request, url, digest, env) {
  const m = /^FpacSpike (\d{13})\.([0-9a-f]{128})$/.exec(request.headers.get("authorization") || "");
  if (!m || !/^04[0-9a-f]{128}$/.test(env.DRIVER_PUBLIC_KEY || "") || !SHA256_HEX.test(digest)) return false;
  if (Math.abs(Date.now() - Number(m[1])) > SIGNED_SKEW_MS_MAX) return false;
  const message = new TextEncoder().encode(`fpac-spike\n${request.method}\n${url.pathname}${url.search}\n${m[1]}\n${digest}`);
  const key = await crypto.subtle.importKey("raw", unhex(env.DRIVER_PUBLIC_KEY), { name: "ECDSA", namedCurve: "P-256" }, false, ["verify"]);
  return crypto.subtle.verify({ name: "ECDSA", hash: "SHA-256" }, key, unhex(m[2]), message);
}

// The remote's calls (docs/contract.md), for the one disk and the one
// epoch in props. Objects live at `disks/<disk>/<name>`.
export class FpacRemote extends WorkerEntrypoint {
  async fetch(request) {
    const { disk, epoch, volume, head } = this.ctx.props;
    const url = new URL(request.url);
    // The head the Durable Object read at the start: no storage read here.
    if (request.method === "GET" && url.pathname === "/open") {
      return text(200, "", { "x-fpac-disk": disk, "x-fpac-epoch": String(epoch), "x-fpac-volume-bytes": String(volume), "x-fpac-head": head || "none" });
    }
    // A head is the one call that reaches the Durable Object, which moves
    // it only by the head's rules (crates/store/src/head.rs).
    if (request.method === "POST" && url.pathname === "/head") {
      const line = (await request.text()).slice(0, 200);
      const stub = this.env.SPIKE.get(this.env.SPIKE.idFromName("spike"));
      const r = await stub.fetch("https://spike/head", { method: "POST", body: JSON.stringify({ disk, epoch, line }) });
      const a = await r.json();
      if (a.answer) return text(200, "", { "x-fpac-head-answer": a.answer });
      return text(a.status || 409, `${a.refused || a.error}\n`);
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
    if (request.method === "GET") return get(this.env, this.ctx, key, request, "intercept");
    if (request.method === "PUT") {
      if (named[1] !== String(epoch)) return text(403, "a put names another life's epoch\n");
      return put(this.env.DISKS, key, request);
    }
    return text(405, "GET or PUT\n");
  }
}

// A ranged get, as the contract serves it: R2's body streamed on. The
// stand-in's own diagnostics, outside the contract (docs/spike.md,
// Results: wrong reads), are asked for by `x-fpac-mode`:
// - `tee`: streamed on while a copy is hashed and compared with
//   `x-fpac-expect-sha256`;
// - `buffer`: read whole, hashed and compared, then sent;
// - `x-fpac-diagnose: sha256` (an earlier run's): `buffer`, naming what it
//   sent in `x-fpac-sent-sha256`.
// A mismatch is recorded in the Durable Object under the answer's
// `x-fpac-request`, so a wrong read the client sees can be put on R2's
// side of the handler (recorded) or after it (not recorded). Every ranged
// answer names its request, the Worker's version and any cf-ray.
async function get(env, ctx, key, request, path) {
  const t0 = Date.now();
  const range = request.headers.get("range");
  const bucket = env.DISKS;
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
  const id = crypto.randomUUID();
  const headers = {
    "content-length": String(length),
    "content-range": `bytes ${offset}-${offset + length - 1}/${o.size}`,
    "server-timing": timing(t0),
    "x-fpac-request": id,
    "x-fpac-version": env.VERSION.id,
    "x-fpac-ray": request.headers.get("cf-ray") || "none",
  };
  const legacy = request.headers.get("x-fpac-diagnose") === "sha256";
  const mode = legacy ? "buffer" : request.headers.get("x-fpac-mode") || "stream";
  const expect = request.headers.get("x-fpac-expect-sha256") || "";
  const note = (got, bytes) => ({ id, path, mode, key, range, offset, length, size: o.size, r2_range: o.range || null, got_sha256: got, got_bytes: bytes, expect_sha256: expect, version: env.VERSION.id, ray: headers["x-fpac-ray"], colo: (request.cf && request.cf.colo) || "none", at_ms: Date.now() });
  if (mode === "buffer") {
    const bytes = await o.arrayBuffer();
    const got = hex(await crypto.subtle.digest("SHA-256", bytes));
    if (expect && got !== expect) ctx.waitUntil(record(env, note(got, bytes.byteLength)));
    return new Response(bytes, { status: 206, headers: { ...headers, "content-length": String(bytes.byteLength), "x-fpac-sent-sha256": got } });
  }
  if (mode === "tee") {
    const [out, copy] = o.body.tee();
    ctx.waitUntil(
      (async () => {
        const digest = new crypto.DigestStream("SHA-256");
        let n = 0;
        const counted = copy.pipeThrough(new TransformStream({ transform(chunk, c) { n += chunk.byteLength; c.enqueue(chunk); } }));
        await counted.pipeTo(digest);
        const got = hex(await digest.digest);
        if (expect && got !== expect) await record(env, note(got, n));
      })().catch((e) => record(env, { id, path, mode, key, range, error: String(e), at_ms: Date.now() })),
    );
    return new Response(out, { status: 206, headers });
  }
  return new Response(o.body, { status: 206, headers });
}

// A mismatch seen on R2's side of the handler, kept by the Durable Object.
function record(env, rec) {
  const stub = env.SPIKE.get(env.SPIKE.idFromName("spike"));
  return stub.fetch("https://spike/records", { method: "POST", body: JSON.stringify(rec) }).catch(() => {});
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

// A container's call to a relayed host: to the Durable Object, which
// carries it to the driver (`relayCall`). WebSocket upgrades too.
export class FpacRelay extends WorkerEntrypoint {
  async fetch(request) {
    const url = new URL(request.url);
    const stub = this.env.SPIKE.get(this.env.SPIKE.idFromName("spike"));
    return stub.fetch(new Request(`https://spike/__relay/${this.ctx.props.host}${url.pathname}${url.search}`, request));
  }
}

const b64 = (bytes) => {
  let s = "";
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
};
const unb64 = (s) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));

export class FpacSpike extends DurableObject {
  async fetch(request) {
    const url = new URL(request.url);
    try {
      if (url.pathname === "/relay" && request.headers.get("upgrade") === "websocket") return this.acceptRelay();
      if (url.pathname.startsWith("/__relay/")) return await this.relayCall(request, url);
      const route = `${request.method} ${url.pathname}`;
      if (route === "POST /start") return json({ ...(await this.start(await request.json())), driver_colo: request.headers.get("x-driver-colo") });
      if (route === "POST /head") return json(await this.head(await request.json()));
      if (route === "POST /exec") return json(await this.exec(await request.json()));
      if (route === "POST /destroy") return json(await this.destroy());
      if (route === "GET /state") return json({ version: this.env.VERSION.id, running: this.ctx.container.running, images: Object.keys(this.ctx.container.images || {}) });
      if (route === "POST /objects/delete") return json(await this.deleteObjects(await request.json()));
      if (route === "POST /objects/usage") return json(await this.usage(await request.json()));
      if (route === "POST /records") return json(await this.keep(await request.json()));
      if (route === "GET /records") return json([...(await this.ctx.storage.list({ prefix: "rec:", limit: RECORDS_MAX })).values()]);
      if (route === "POST /records/clear") return json(await this.clear());
      return text(404, "no such route\n");
    } catch (e) {
      return json({ error: String((e && e.stack) || e) }, 500);
    }
  }

  // Starts the container from the named image, its intercept registered
  // first where the runtime allows it (so the guest's first call never
  // races it), and waits until it takes an exec.
  // The head's rules (crates/store/src/head.rs, `decide`), for the disk
  // whose container is running: only the running life moves it, and
  // within a life only forward.
  async head({ disk, epoch, line }) {
    const running = await this.ctx.storage.get("running");
    if (!running || running.disk !== disk || running.epoch !== epoch) return { refused: "only the running life posts a head", status: 409 };
    const p = parseHead(line);
    if (!p) return { refused: "a malformed head", status: 400 };
    const curLine = await this.ctx.storage.get(`head:${disk}`);
    const cur = curLine ? parseHead(curLine) : null;
    const d = decide(cur, epoch, p);
    if (d === "take") {
      await this.ctx.storage.put(`head:${disk}`, line);
      return { answer: "taken" };
    }
    if (d === "same") return { answer: "same" };
    return { refused: d, status: 409 };
  }

  // The relay's state: the driver's socket, the calls waiting on it, the
  // container's WebSockets it carries.
  relayState() {
    this.relay ||= { socket: null, pending: new Map(), sockets: new Map(), next: 1 };
    return this.relay;
  }

  acceptRelay() {
    const r = this.relayState();
    if (r.socket) {
      try {
        r.socket.close(1012, "replaced");
      } catch {}
    }
    const [client, server] = Object.values(new WebSocketPair());
    server.accept();
    r.socket = server;
    server.addEventListener("message", (ev) => this.onRelay(ev.data));
    const gone = () => {
      if (r.socket !== server) return;
      r.socket = null;
      for (const [, p] of r.pending) p.fail(new Error("the relay closed"));
      r.pending.clear();
      for (const [, ws] of r.sockets) {
        try {
          ws.close(1011, "the relay closed");
        } catch {}
      }
      r.sockets.clear();
    };
    server.addEventListener("close", gone);
    server.addEventListener("error", gone);
    return new Response(null, { status: 101, webSocket: client });
  }

  relaySend(msg) {
    const r = this.relayState();
    if (r.socket) r.socket.send(JSON.stringify(msg));
  }

  relayWait(id) {
    const r = this.relayState();
    return new Promise((resolve, reject) => {
      const p = { chunks: [], status: 0, headers: [], done: () => resolve(p), fail: reject };
      r.pending.set(id, p);
      setTimeout(() => {
        if (r.pending.delete(id)) reject(new Error("the relay took too long"));
      }, RELAY_MS_MAX);
    });
  }

  onRelay(data) {
    const r = this.relayState();
    let m;
    try {
      m = JSON.parse(data);
    } catch {
      return;
    }
    const p = r.pending.get(m.id);
    if (m.t === "res" && p) {
      p.status = m.status;
      p.headers = m.headers || [];
    } else if (m.t === "body" && p) {
      p.chunks.push(unb64(m.b));
    } else if ((m.t === "end" || m.t === "ws-ok") && p) {
      r.pending.delete(m.id);
      p.done();
    } else if (m.t === "fail" && p) {
      r.pending.delete(m.id);
      p.fail(new Error(m.why || "the relay's call failed"));
    } else if (m.t === "ws-msg") {
      const ws = r.sockets.get(m.id);
      if (ws) ws.send(m.text !== undefined ? m.text : unb64(m.b));
    } else if (m.t === "ws-close") {
      const ws = r.sockets.get(m.id);
      if (ws && r.sockets.delete(m.id)) {
        try {
          ws.close(m.code && m.code >= 1000 && m.code < 5000 && m.code !== 1005 && m.code !== 1006 ? m.code : 1000, m.reason || "");
        } catch {}
      }
    }
  }

  // A container's call to a relayed host, carried to the driver: a
  // request (head, body chunks, end) answered the same way, or a
  // WebSocket upgrade paired with one the driver opens.
  async relayCall(request, url) {
    const r = this.relayState();
    if (!r.socket) return text(502, "no relay\n");
    const rest = url.pathname.slice("/__relay/".length);
    const slash = rest.indexOf("/");
    const host = slash < 0 ? rest : rest.slice(0, slash);
    const path = (slash < 0 ? "/" : rest.slice(slash)) + url.search;
    if (!RELAY_HOSTS.includes(host)) return text(400, "not a relayed host\n");
    const id = r.next++;
    const skip = ["host", "upgrade", "connection", "sec-websocket-key", "sec-websocket-version", "sec-websocket-extensions", "content-length"];
    const headers = [...request.headers].filter(([k]) => !skip.includes(k.toLowerCase()));
    if (request.headers.get("upgrade") === "websocket") {
      const [client, server] = Object.values(new WebSocketPair());
      server.accept();
      r.sockets.set(id, server);
      const opened = this.relayWait(id);
      this.relaySend({ id, t: "ws-open", host, path, headers });
      try {
        await opened;
      } catch (e) {
        r.sockets.delete(id);
        return text(502, `${e.message}\n`);
      }
      server.addEventListener("message", (ev) => this.relaySend(typeof ev.data === "string" ? { id, t: "ws-msg", text: ev.data } : { id, t: "ws-msg", b: b64(new Uint8Array(ev.data)) }));
      server.addEventListener("close", (ev) => {
        if (r.sockets.delete(id)) this.relaySend({ id, t: "ws-close", code: ev.code, reason: ev.reason });
      });
      return new Response(null, { status: 101, webSocket: client });
    }
    const body = request.method === "GET" || request.method === "HEAD" ? new Uint8Array() : new Uint8Array(await request.arrayBuffer());
    if (body.length > RELAY_BODY_BYTES_MAX) return text(413, "too large\n");
    const done = this.relayWait(id);
    this.relaySend({ id, t: "req", host, method: request.method, path, headers });
    for (let i = 0; i < body.length; i += RELAY_CHUNK_BYTES) this.relaySend({ id, t: "body", b: b64(body.subarray(i, i + RELAY_CHUNK_BYTES)) });
    this.relaySend({ id, t: "end" });
    let p;
    try {
      p = await done;
    } catch (e) {
      return text(502, `${e.message}\n`);
    }
    const out = new Uint8Array(p.chunks.reduce((n, c) => n + c.length, 0));
    let at = 0;
    for (const c of p.chunks) {
      out.set(c, at);
      at += c.length;
    }
    const h = new Headers(p.headers.filter(([k]) => !["content-length", "transfer-encoding", "connection"].includes(String(k).toLowerCase())));
    return new Response([101, 204, 205, 304].includes(p.status) ? null : out, { status: p.status, headers: h });
  }

  async start({ disk, image, epoch, volume, env, relay, internet }) {
    const c = this.ctx.container;
    if (c.running) throw new Error("a container is running: one at a time");
    // The npm benchmark's image alone reaches the internet (docs/results.md,
    // C): a registry install runs its packages' own scripts, so it runs only
    // in a disposable container, never on the driver's machine. Every other
    // start has none.
    if (internet && !INTERNET_IMAGES.includes(image)) throw new Error(`the ${image} image has no internet`);
    const extra = {};
    const names = Object.keys(env || {});
    if (names.length > ENV_VARS_MAX) throw new Error("too many env vars");
    for (const k of names) {
      if (!ENV_NAME.test(k) || typeof env[k] !== "string" || env[k].length > 4096) throw new Error(`an env var: ${k}`);
      extra[k] = env[k];
    }
    if (!/^[a-z0-9-]{1,40}$/.test(disk)) throw new Error("a disk's name: a-z, 0-9 and -");
    if (!Number.isSafeInteger(epoch) || epoch < 1) throw new Error("an epoch is a positive integer");
    const ref = c.images && c.images[image];
    if (!ref) throw new Error(`no image named ${image}`);
    const volumeBytes = volume || VOLUME_BYTES_DEFAULT;
    const head = (await this.ctx.storage.get(`head:${disk}`)) || null;
    if (head && parseHead(head).epoch >= epoch) throw new Error("a life's epoch is later than its disk's head");
    await this.ctx.storage.put("running", { disk, epoch });
    const egress = this.ctx.exports.FpacRemote({ props: { disk, epoch, volume: volumeBytes, head } });
    const t0 = Date.now();
    let intercept = "before the start";
    try {
      await c.interceptOutboundHttp(REMOTE_HOST, egress);
    } catch (e) {
      intercept = `after the start (before it: ${String((e && e.message) || e)})`;
    }
    if (relay) {
      for (const host of RELAY_HOSTS) await c.interceptOutboundHttp(host, this.ctx.exports.FpacRelay({ props: { host } }));
    }
    c.start({ image: ref, enableInternet: Boolean(internet), instance: INSTANCE, env: { ...extra, FPAC_REMOTE: REMOTE_HOST } });
    c.monitor().catch(() => {});
    if (intercept !== "before the start") await c.interceptOutboundHttp(REMOTE_HOST, egress);
    const started = Date.now() - t0;
    const ready = await this.until(READY_MS_MAX);
    // Where this object (and its container, placed beside it) runs: the
    // colo Cloudflare's trace names for a subrequest from here.
    const where = await fetch("https://www.cloudflare.com/cdn-cgi/trace").then((r) => r.text()).then((t) => (/^colo=(\w+)$/m.exec(t) || [])[1] || "unknown", () => "unknown");
    // The image's digest only: its reference names the account.
    const digest = String(ref).split("@")[1] || null;
    return { version: this.env.VERSION.id, disk, epoch, image, image_digest: digest, internet: Boolean(internet), intercept, start_call_ms: started, ready, instance: INSTANCE, object_colo: where };
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

  async keep(rec) {
    const n = (await this.ctx.storage.get("rec-count")) || 0;
    if (n >= RECORDS_MAX) return { kept: false };
    await this.ctx.storage.put({ [`rec:${String(Date.now()).padStart(15, "0")}:${rec.id || crypto.randomUUID()}`]: rec, "rec-count": n + 1 });
    return { kept: true };
  }

  async clear() {
    const keys = [...(await this.ctx.storage.list({ prefix: "rec:", limit: RECORDS_MAX })).keys()];
    if (keys.length) await this.ctx.storage.delete(keys);
    await this.ctx.storage.delete("rec-count");
    return { cleared: keys.length };
  }

  // Deletes every object under `prefix` (which must be under `disks/`):
  // the driver's cleanup of bulk test data, never the guest's.
  async deleteObjects({ prefix }) {
    if (typeof prefix !== "string" || !/^(disks\/([a-z0-9-]+\/)?|repro\/)$/.test(prefix)) throw new Error("a prefix is disks/, disks/<disk>/ or repro/");
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

// A head's line: "<epoch> <checkpoint epoch> <checkpoint seq> <sha256>",
// or "<epoch> - - -", and " clean" after it for a clean sleep
// (crates/remote/src/lib.rs, Head).
// A head's numbers are compared as Numbers, so each must be exact: no
// leading zero, and at most 2^53 - 1 (an epoch is a time in ms).
const exact = (s) => /^[1-9][0-9]{0,15}$/.test(s) && Number.isSafeInteger(Number(s));

function parseHead(line) {
  const p = String(line).split(" ");
  const clean = p.length === 5 && p[4] === "clean";
  if ((p.length !== 4 && !clean) || !exact(p[0])) return null;
  const epoch = Number(p[0]);
  if (p[1] === "-" && p[2] === "-" && p[3] === "-") return { epoch, ck: null, clean };
  if (!exact(p[1]) || !exact(p[2]) || !SHA256_HEX.test(p[3])) return null;
  return { epoch, ck: { epoch: Number(p[1]), seq: Number(p[2]), sum: p[3] }, clean };
}

const sameCk = (a, b) => a.epoch === b.epoch && a.seq === b.seq && a.sum === b.sum;

// crates/store/src/head.rs, `decide`, line for line.
function decide(cur, running, p) {
  if (p.epoch !== running) return "only the running life posts a head";
  if (p.ck && p.ck.epoch > p.epoch) return "a checkpoint from a later life";
  if (!cur) return "take";
  if (cur.epoch === p.epoch && cur.clean === p.clean && ((cur.ck === null && p.ck === null) || (cur.ck && p.ck && sameCk(cur.ck, p.ck)))) return "same";
  if (cur.epoch > p.epoch) return "an earlier life's head";
  if (cur.epoch < p.epoch) return "take";
  if (!cur.ck && !p.ck && p.clean) return "take";
  if (!p.ck) return "a life's head loses its checkpoint";
  if (!cur.ck) return p.ck.epoch === p.epoch ? "take" : "a life's head goes back to another life's checkpoint";
  if (p.ck.epoch !== p.epoch && !sameCk(p.ck, cur.ck)) return "a life's head goes back to another life's checkpoint";
  if (sameCk(p.ck, cur.ck)) return p.clean ? "take" : "a clean head loses its mark";
  if (p.ck.epoch === cur.ck.epoch && p.ck.seq === cur.ck.seq) return "the same checkpoint with another body";
  if (p.ck.epoch === cur.ck.epoch && p.ck.seq < cur.ck.seq) return "a checkpoint older than the head's";
  return "take";
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
