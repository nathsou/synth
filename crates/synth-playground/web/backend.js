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
      this.pending.delete(e.data.id);
      if (e.data.ok) p.resolve(e.data.out);
      else p.reject(new Error(e.data.error));
    };
    this.worker.onerror = (e) => {
      for (const p of this.pending.values()) p.reject(new Error(e.message || 'worker failed'));
      this.pending.clear();
    };
  }
  call(method, body) {
    const id = this.next++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.worker.postMessage({ id, method, body });
    });
  }
}

class HttpChannel {
  async call(method, body) {
    const r = await fetch(`api/${method}`, { method: 'POST', body, headers: { 'Content-Type': 'application/json' } });
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    return r.text();
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
  call(method, body) {
    return new Promise((resolve, reject) => {
      if (this.waiting) this.waiting.reject(Object.assign(new Error('superseded'), { superseded: true }));
      this.waiting = { method, body, resolve, reject };
      this.pump();
    });
  }
  async pump() {
    if (this.busy || !this.waiting) return;
    const job = this.waiting;
    this.waiting = null;
    this.busy = true;
    try {
      job.resolve(JSON.parse(await this.channel.call(job.method, job.body)));
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
      this.runs = new Latest(runner);
      this.checks = new Latest(checker);
    } catch {
      this.mode = 'server';
      const http = new HttpChannel();
      this.direct = http;
      this.runs = new Latest(http);
      this.checks = new Latest(http);
    }
    return this.mode;
  }

  async once(method, body = '{}') {
    return JSON.parse(await this.direct.call(method, body));
  }

  run(req) { return this.runs.call('run', JSON.stringify(req)); }
  analyze(req) { return this.checks.call('analyze', JSON.stringify(req)); }
}
