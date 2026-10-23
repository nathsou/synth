// The Simulation view: the design running on a virtual board. Its clock is
// driven in real time (or as fast as the simulator goes), its outputs shown
// as LEDs lit by their duty cycle, its 1-bit inputs as buttons, and its
// UART (`tx`, `rx`) as a terminal, drawn as text or as pixels (each
// character other than a space or a dot a lit cell) for designs that draw
// frames, like the Game of Life. Registers the design marks as frames
// (`#[attr("frame", "WxH")]`) are read directly and drawn as images.

import { h, icon, store } from './util.js';
import { Terminal } from './terminal.js';

/** How long one simulation request may run, in milliseconds. */
const BUDGET_MS = 40;

export class SimView {
  constructor(el, app) {
    this.el = el;
    this.app = app;
    this.term = new Terminal();
    this.session = null;
    this.running = false;
    this.busy = false;
    this.inputs = {};
    this.rx = [];
    this.mode = store.get('simMode', 'text');
    this.realtime = store.get('simRealtime', true);
    this.build();
  }

  build() {
    this.playBtn = h('button', { class: 'primary', onclick: () => this.toggle(), title: 'Run or pause the simulation' });
    this.resetBtn = h('button', { onclick: () => this.restart(), title: 'Start again from power-on, with the current design' },
      icon('reset'), h('span', {}, 'Restart'));
    this.speed = h('select', {
      title: 'Real time: one simulated second per second. Fastest: as fast as the simulator goes.',
      onchange: (e) => {
        this.realtime = e.target.value === 'real';
        store.set('simRealtime', this.realtime);
        this.anchor();
      },
    }, h('option', { value: 'real' }, 'Real time'), h('option', { value: 'fast' }, 'Fastest'));
    this.speed.value = this.realtime ? 'real' : 'fast';
    this.status = h('span', { class: 'muted sim-status' });
    this.board = h('div', { class: 'sim-board' });
    this.framesBox = h('div', { class: 'sim-frames' });
    this.screen = h('pre', { class: 'sim-screen', tabindex: 0, onkeydown: (e) => this.key(e) });
    this.canvas = h('canvas', { class: 'sim-pixels', tabindex: 0, onkeydown: (e) => this.key(e) });
    this.frameInfo = h('span', { class: 'muted' });
    const modeBtn = (m, label) => h('button', { 'data-mode': m, onclick: () => this.setMode(m) }, label);
    this.modes = h('div', { class: 'seg' }, modeBtn('text', 'Text'), modeBtn('pixels', 'Pixels'));
    this.termBox = h('div', { class: 'sim-term' },
      h('div', { class: 'sim-term-bar' },
        h('span', {}, 'Serial · 115200 baud'), this.frameInfo, h('div', { class: 'grow' }), this.modes,
        h('button', { onclick: () => { this.term.reset(); this.draw(); }, title: 'Clear the screen' }, 'Clear')),
      this.screen, this.canvas);
    this.message = h('div', { class: 'sim-message muted' },
      'Runs the design on a virtual board: its clock, LEDs, buttons and serial port. Press Run to start.');
    this.el.replaceChildren(
      h('div', { class: 'sim-bar' }, this.playBtn, this.resetBtn, this.speed, this.status),
      this.message, this.board, this.framesBox, this.termBox);
    this.updateControls();
    this.setMode(this.mode);
  }

  setMode(m) {
    this.mode = m;
    store.set('simMode', m);
    for (const b of this.modes.querySelectorAll('button')) b.classList.toggle('on', b.dataset.mode === m);
    this.screen.hidden = m !== 'text';
    this.canvas.hidden = m !== 'pixels';
    this.draw();
  }

  updateControls() {
    this.playBtn.replaceChildren(icon(this.running ? 'pause' : 'play'), h('span', {}, this.running ? 'Pause' : 'Run'));
    this.resetBtn.disabled = !this.session;
  }

  /** Starts (or restarts) a simulation of the current design. */
  async restart() {
    if (this.starting) return;
    this.starting = true;
    try {
      await this.start();
    } finally {
      this.starting = false;
    }
  }

  async start() {
    this.running = false;
    this.updateControls();
    this.session = null;
    this.term = new Terminal();
    this.draw();
    this.message.hidden = false;
    this.message.textContent = 'Elaborating…';
    let info;
    const req = this.app.simRequest();
    this.simulated = JSON.stringify(req);
    try {
      info = await this.app.backend.simStart(req);
    } catch (e) {
      info = { ok: false, error: e.message };
    }
    if (!info.ok) {
      this.message.textContent = `Cannot simulate: ${info.error || 'error'}`;
      this.board.replaceChildren();
      this.framesBox.replaceChildren();
      return;
    }
    this.session = info;
    this.message.hidden = !(info.warnings || []).length;
    this.message.textContent = (info.warnings || []).join('\n');
    this.inputs = Object.fromEntries(info.inputs.map((p) => [p.name, 0]));
    this.rx = [];
    this.buildBoard(info);
    this.buildFrames(info);
    this.termBox.hidden = !info.uart_tx && !info.uart_rx;
    this.total = 0;
    this.running = true;
    this.anchor();
    this.updateControls();
    this.loop();
  }

  toggle() {
    if (!this.session) {
      this.restart();
      return;
    }
    this.running = !this.running;
    this.anchor();
    this.updateControls();
    if (this.running) this.loop();
  }

  /** Real-time pacing restarts from now. */
  anchor() {
    this.wallStart = performance.now();
    this.cyclesStart = this.total || 0;
  }

  /** The view was opened: a design not simulated yet starts. */
  visible() {
    if (!this.session && !this.starting) this.restart();
    else this.draw();
  }

  /** The design changed (`replaced`: another design altogether). A
   *  simulation on screen restarts with it; one out of sight stops. */
  designChanged(replaced) {
    if (!this.session && !replaced) return;
    // A rebuild of the design being simulated changes nothing here.
    if (!replaced && JSON.stringify(this.app.simRequest()) === this.simulated) return;
    if (this.app.view === 'simulation' && (this.running || replaced)) this.restart();
    else if (this.session) {
      this.stop();
      this.message.hidden = false;
      this.message.textContent = 'The design changed: press Run to simulate it.';
    }
  }

  /** Stops the simulation (the design changed, or the view closed). */
  stop() {
    this.running = false;
    this.session = null;
    this.updateControls();
    this.app.backend.simStop().catch(() => {});
  }

  buildBoard(info) {
    this.leds = [];
    const groups = info.outputs.map((p) => {
      const leds = Array.from({ length: p.width }, (_, i) => h('div', { class: 'led', title: `${p.name}[${i}]` }));
      this.leds.push({ name: p.name, leds });
      return h('div', { class: 'sim-group' },
        h('div', { class: 'sim-label' }, p.name),
        // Most significant bit on the left, as boards number them.
        h('div', { class: 'leds' }, [...leds].reverse()));
    });
    const buttons = info.inputs.map((p) => {
      if (p.width === 1) {
        const set = (v) => { this.inputs[p.name] = v; b.classList.toggle('down', !!v); };
        const b = h('button', {
          class: 'sim-button',
          title: `Hold to drive ${p.name} high`,
          onpointerdown: (e) => { b.setPointerCapture(e.pointerId); set(1); },
          onpointerup: () => set(0),
          onpointercancel: () => set(0),
        }, p.name);
        return b;
      }
      return h('label', { class: 'sim-group' }, h('div', { class: 'sim-label' }, p.name),
        h('input', {
          type: 'number', min: 0, value: 0, style: { width: '90px' },
          onchange: (e) => { this.inputs[p.name] = Number(e.target.value) || 0; },
        }));
    });
    this.board.replaceChildren(...groups, ...buttons);
  }

  buildFrames(info) {
    this.frames = (info.frames || []).map((f) => {
      const canvas = h('canvas', { class: 'sim-pixels' });
      const label = h('span', { class: 'muted' });
      const el = h('div', { class: 'sim-frame' },
        h('div', { class: 'sim-term-bar' }, h('span', {}, f.name), label), canvas);
      return { ...f, canvas, label, el, pixels: null };
    });
    this.framesBox.replaceChildren(...this.frames.map((f) => f.el));
    this.framesBox.hidden = !this.frames.length;
  }

  async loop() {
    if (!this.running || this.busy || !this.session) return;
    this.busy = true;
    const session = this.session;
    const hz = session.hz;
    let cycles = 1e9;
    if (this.realtime) {
      const due = this.cyclesStart + ((performance.now() - this.wallStart) / 1000) * hz;
      // At most a quarter second at once (after a pause, a hidden tab).
      cycles = Math.max(0, Math.min(Math.round(due - this.total), hz / 4));
    }
    const rx = this.rx.splice(0);
    const t0 = performance.now();
    let r;
    try {
      r = cycles > 0
        ? await this.app.backend.simStep({ cycles, budget_ms: BUDGET_MS, inputs: this.inputs, rx })
        : null;
    } catch (e) {
      r = { error: e.message };
    }
    this.busy = false;
    // A restart while the request ran: its result is of the old design.
    if (this.session !== session) {
      this.loop();
      return;
    }
    if (r?.error) {
      this.running = false;
      this.message.hidden = false;
      this.message.textContent = `Simulation stopped: ${r.error}`;
      this.updateControls();
      return;
    }
    if (r) this.show(r, performance.now() - t0);
    if (this.running) requestAnimationFrame(() => this.loop());
  }

  show(r, ms) {
    this.total = r.total;
    for (const f of r.frames || []) {
      const fr = this.frames.find((x) => x.name === f.name);
      if (!fr) continue;
      fr.label.textContent = `${fr.width}×${fr.height} · frame ${f.changes}`;
      if (f.pixels) {
        fr.pixels = f.pixels;
        this.drawFrame(fr);
      }
    }
    if (r.tx.length) {
      this.term.write(r.tx);
      this.draw();
    }
    for (const o of r.outputs) {
      const g = this.leds.find((l) => l.name === o.name);
      g?.leds.forEach((led, i) => {
        const d = o.duty[i] ?? 0;
        led.style.setProperty('--on', d.toFixed(3));
        led.classList.toggle('lit', d > 0.02);
      });
    }
    // Speed: simulated time per wall-clock time of the last request.
    if (r.cycles > 0 && ms > 0) this.rate = (r.cycles / this.session.hz) / (ms / 1000);
    const secs = r.time < 100 ? r.time.toFixed(2) : r.time.toFixed(0);
    const speed = this.rate ? ` · up to ${this.rate.toFixed(this.rate < 10 ? 1 : 0)}× real time` : '';
    this.status.textContent = `${secs} s simulated · ${(this.session.hz / 1000).toFixed(1)} kHz clock${speed}`;
  }

  key(e) {
    if (!this.session?.uart_rx || e.ctrlKey || e.metaKey || e.altKey) return;
    let bytes = null;
    if (e.key === 'Enter') bytes = [13];
    else if (e.key === 'Backspace') bytes = [8];
    else if (e.key.length === 1) bytes = [...new TextEncoder().encode(e.key)];
    if (bytes) {
      this.rx.push(...bytes);
      e.preventDefault();
    }
  }

  /** Draws a frame: each pixel a cell lit by its value. */
  drawFrame(f) {
    const box = this.framesBox.getBoundingClientRect();
    const cell = Math.max(2, Math.min(40, Math.floor(Math.min(((box.width || 400) - 24) / f.width, 320 / f.height))));
    const colors = this.colors();
    const max = 2 ** f.bpp - 1;
    paint(f.canvas, f.width, f.height, cell, (x, y) => {
      const v = f.pixels[y * f.width + x] / max;
      return v > 0 ? mix(colors.dim, colors.lit, v) : colors.dim;
    });
  }

  colors() {
    const css = getComputedStyle(document.documentElement);
    return {
      lit: css.getPropertyValue('--accent').trim() || '#7aa2ff',
      dim: css.getPropertyValue('--bg-3').trim() || '#242938',
    };
  }

  draw() {
    this.frameInfo.textContent = this.term.frames ? `frame ${this.term.frames}` : '';
    if (this.mode === 'text') {
      this.screen.textContent = this.term.text();
      return;
    }
    // Pixels: one cell per character of the used part of the screen.
    const lines = this.term.text().split('\n');
    const cols = Math.max(1, ...lines.map((l) => l.length));
    const rows = Math.max(1, lines.length);
    const box = this.canvas.parentElement?.getBoundingClientRect();
    const cell = Math.max(4, Math.min(40, Math.floor(Math.min(((box?.width || 400) - 24) / cols, 320 / rows))));
    const { lit, dim } = this.colors();
    paint(this.canvas, cols, rows, cell, (x, y) => {
      const c = lines[y][x] || ' ';
      return c === ' ' ? null : c === '.' ? dim : lit;
    });
  }
}

/** Paints a grid of cells, each `color(x, y)` (none: left blank). */
function paint(canvas, cols, rows, cell, color) {
  const dpr = window.devicePixelRatio || 1;
  canvas.width = cols * cell * dpr;
  canvas.height = rows * cell * dpr;
  canvas.style.width = `${cols * cell}px`;
  canvas.style.height = `${rows * cell}px`;
  const g = canvas.getContext('2d');
  g.scale(dpr, dpr);
  // Small cells touch, larger ones keep a gap between them.
  const gap = cell >= 6 ? 1 : 0;
  for (let y = 0; y < rows; y++) {
    for (let x = 0; x < cols; x++) {
      const c = color(x, y);
      if (!c) continue;
      g.fillStyle = c;
      g.fillRect(x * cell + gap, y * cell + gap, cell - 2 * gap, cell - 2 * gap);
    }
  }
}

/** The color a share `t` of the way from `a` to `b` (both `#rrggbb`). */
function mix(a, b, t) {
  const rgb = (c) => [1, 3, 5].map((i) => parseInt(c.slice(i, i + 2), 16));
  if (!/^#[0-9a-f]{6}$/i.test(a) || !/^#[0-9a-f]{6}$/i.test(b)) return t >= 0.5 ? b : a;
  const [p, q] = [rgb(a), rgb(b)];
  return `rgb(${p.map((v, i) => Math.round(v + (q[i] - v) * t)).join(',')})`;
}
