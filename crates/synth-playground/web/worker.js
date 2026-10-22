// Runs the playground's WebAssembly build off the main thread.
// Messages: {id, method, body} → {id, ok, out} or {id, ok: false, error},
// after any number of {id, progress} while the flow runs.

let wasm = null;
// The request being answered, for its progress messages.
let current = 0;

async function load() {
  const url = new URL('synth_playground.wasm', self.location);
  const resp = await fetch(url);
  if (!resp.ok) throw new Error(`synth_playground.wasm: HTTP ${resp.status}`);
  const bytes = await resp.arrayBuffer();
  const env = {
    synth_now_ms: () => performance.now(),
    synth_fs_read: fsRead,
    synth_fs_take: fsTake,
    synth_progress: (ptr, len) => {
      const progress = JSON.parse(dec.decode(new Uint8Array(wasm.memory.buffer, ptr, len).slice()));
      self.postMessage({ id: current, progress });
    },
  };
  const { instance } = await WebAssembly.instantiate(bytes, { env });
  return instance.exports;
}

// The Artix-7 flow reads the Project X-Ray database as files under
// `xc7db/`. Each part's files come in one bundle, `xc7db/<part>.tar.gz`,
// fetched before its first run.
const files = new Map();
const bundles = new Map();
let taken = null;

function fsRead(ptr, len) {
  const path = dec.decode(new Uint8Array(wasm.memory.buffer, ptr, len).slice());
  taken = files.get(path) || null;
  return taken ? taken.length : -1;
}

function fsTake(dst) {
  new Uint8Array(wasm.memory.buffer, dst, taken.length).set(taken);
  taken = null;
}

// The regular files of a tar archive, as name → bytes.
function untar(buf, into) {
  const str = (o, n) => dec.decode(buf.subarray(o, o + n)).replace(/\0.*$/s, '');
  for (let o = 0; o + 512 <= buf.length;) {
    const name = str(o, 100);
    if (!name) break;
    const size = parseInt(str(o + 124, 12).trim() || '0', 8);
    const prefix = str(o + 345, 155);
    const type = String.fromCharCode(buf[o + 156] || 48);
    if (type === '0') into.set(`xc7db/${prefix ? `${prefix}/` : ''}${name.replace(/^\.\//, '')}`, buf.subarray(o + 512, o + 512 + size));
    o += 512 + Math.ceil(size / 512) * 512;
  }
}

function bundle(part) {
  if (!bundles.has(part)) {
    bundles.set(part, (async () => {
      const resp = await fetch(new URL(`xc7db/${part}.tar.gz`, self.location));
      if (!resp.ok) {
        throw new Error(`this build of the playground has no Artix-7 database for ${part} (xc7db/${part}.tar.gz: HTTP ${resp.status}); build it with tools/build_playground.sh and XC7_DB set`);
      }
      const tar = await new Response(resp.body.pipeThrough(new DecompressionStream('gzip'))).arrayBuffer();
      untar(new Uint8Array(tar), files);
    })().catch((e) => { bundles.delete(part); throw e; }));
  }
  return bundles.get(part);
}

// The bundle a request needs, if any.
async function prepare(method, body) {
  if (method !== 'run') return;
  let req;
  try { req = JSON.parse(body); } catch { return; }
  if (typeof req.part !== 'string' || !req.part.startsWith('xc7')) return;
  const part = `${req.part}${req.package}-1`;
  if (!bundles.has(part)) {
    self.postMessage({ id: current, progress: { step: 'elaborate', fraction: null, detail: `downloading the ${req.part} database` } });
  }
  await bundle(part);
}

let ready = load().then((w) => { wasm = w; });

const enc = new TextEncoder();
const dec = new TextDecoder();

function put(s) {
  const b = enc.encode(s);
  const p = wasm.pg_alloc(b.length);
  new Uint8Array(wasm.memory.buffer, p, b.length).set(b);
  return [p, b.length];
}

function call(method, body) {
  const [mp, ml] = put(method);
  const [bp, bl] = put(body);
  try {
    const r = wasm.pg_call(mp, ml, bp, bl);
    const n = wasm.pg_result_len();
    return dec.decode(new Uint8Array(wasm.memory.buffer, r, n).slice());
  } finally {
    try {
      wasm.pg_free(mp, ml);
      wasm.pg_free(bp, bl);
    } catch { /* the instance trapped */ }
  }
}

self.onmessage = async (e) => {
  const { id, method, body } = e.data;
  try {
    await ready;
    current = id;
    await prepare(method, body);
    const out = call(method, body);
    self.postMessage({ id, ok: true, out });
  } catch (err) {
    let msg = String(err && err.message || err);
    // A Rust panic traps; recover its message, then start afresh.
    try {
      const p = wasm.pg_panic();
      const n = wasm.pg_result_len();
      const text = dec.decode(new Uint8Array(wasm.memory.buffer, p, n).slice());
      if (text) msg = `internal error: ${text}`;
    } catch { /* no message */ }
    ready = load().then((w) => { wasm = w; });
    self.postMessage({ id, ok: false, error: msg });
  }
};
