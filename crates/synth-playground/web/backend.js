// Talks to the flow: the WebAssembly build in workers when it is available
// (a static deployment), otherwise the native server's `/api/<method>`.

class WorkerChannel {
  constructor() {
    this.worker = new Worker(new URL('worker.js', import.meta.url));
    this.next = 1;
    this.pending = new Map();
    this.worker.onmessage = (e) => {
      const p = this.pending.get(e.data.id);
      if (!p) return;
      if (e.data.progress) {
        p.onProgress?.(e.data.progress);
        return;
      }
      this.pending.delete(e.data.id);
      if (e.data.ok) p.resolve(e.data.out);
      else p.reject(new Error(e.data.error));
    };
    this.worker.onerror = (e) => {
      for (const p of this.pending.values()) p.reject(new Error(e.message || 'worker failed'));
      this.pending.clear();
    };
  }
  call(method, body, onProgress) {
    const id = this.next++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject, onProgress });
      this.worker.postMessage({ id, method, body });
    });
  }
}

class HttpChannel {
  async call(method, body, onProgress) {
    const url = onProgress ? `api/${method}?progress` : `api/${method}`;
    const r = await fetch(url, { method: 'POST', body, headers: { 'Content-Type': 'application/json' } });
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    if (!onProgress || !r.body) return r.text();
    // One JSON object of progress per line, then the result.
    const reader = r.body.pipeThrough(new TextDecoderStream()).getReader();
    const parts = [];
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      let start = 0;
      for (let nl = value.indexOf('\n'); nl >= 0; nl = value.indexOf('\n', start)) {
        parts.push(value.slice(start, nl));
        const line = parts.join('');
        parts.length = 0;
        start = nl + 1;
        try { onProgress(JSON.parse(line)); } catch { /* not progress */ }
      }
      parts.push(value.slice(start));
    }
    return parts.join('');
  }
}

/** Runs one request at a time; a request made while busy replaces any
 *  waiting one (only the latest edit matters). */
class Latest {
  constructor(channel) {
    this.channel = channel;
    this.busy = false;
    this.waiting = null;
  }
  call(method, body, onProgress) {
    return new Promise((resolve, reject) => {
      if (this.waiting) this.waiting.reject(Object.assign(new Error('superseded'), { superseded: true }));
      this.waiting = { method, body, onProgress, resolve, reject };
      this.pump();
    });
  }
  async pump() {
    if (this.busy || !this.waiting) return;
    const job = this.waiting;
    this.waiting = null;
    this.busy = true;
    try {
      job.onProgress?.(null);
      job.resolve(JSON.parse(await this.channel.call(job.method, job.body, job.onProgress)));
    } catch (e) {
      job.reject(e);
    } finally {
      this.busy = false;
      this.pump();
    }
  }
}

export class Backend {
  async init() {
    try {
      const probe = await fetch(new URL('synth_playground.wasm', import.meta.url), { method: 'HEAD' });
      if (!probe.ok) throw new Error('no wasm');
      this.mode = 'wasm';
      const runner = new WorkerChannel();
      const checker = new WorkerChannel();
      this.direct = runner;
      this.checker = checker;
      this.runs = new Latest(runner);
      this.checks = new Latest(checker);
    } catch {
      this.mode = 'server';
      const http = new HttpChannel();
      this.direct = http;
      this.checker = http;
      this.runs = new Latest(http);
      this.checks = new Latest(http);
    }
    return this.mode;
  }

  async once(method, body = '{}') {
    return JSON.parse(await this.direct.call(method, body));
  }

  /** `onProgress` receives `{step, fraction, detail}` as the flow runs, and
   *  null when the run starts (after waiting for the previous one). */
  run(req, onProgress) { return this.runs.call('run', JSON.stringify(req), onProgress); }
  analyze(req) { return this.checks.call('analyze', JSON.stringify(req)); }
  async format(text) { return JSON.parse(await this.checker.call('format', JSON.stringify({ text }))); }
}
