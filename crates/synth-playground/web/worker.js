// Runs the playground's WebAssembly build off the main thread.
// Messages: {id, method, body} → {id, ok, out} or {id, ok: false, error}.

let wasm = null;

async function load() {
  const url = new URL('synth_playground.wasm', self.location);
  const resp = await fetch(url);
  if (!resp.ok) throw new Error(`synth_playground.wasm: HTTP ${resp.status}`);
  const bytes = await resp.arrayBuffer();
  const { instance } = await WebAssembly.instantiate(bytes, { env: { synth_now_ms: () => performance.now() } });
  return instance.exports;
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
