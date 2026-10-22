// The pipeline overview, timing, bitstream, problems and log panels.

import { Renderer, zoomBar } from './gl.js';
import { settings } from './settings.js';
import { h, fill, icon, rgb, cssVar, mix, esc, fmtNum, hsl, showTooltip, hideTooltip, download, fromBase64, isPins } from './util.js';

// ----- pipeline -----

const STAGE_INFO = {
  elaborated: ['Elaborate', 'The source becomes a flat word-level netlist: operators, multiplexers, registers, memories.'],
  optimised: ['Optimise RTL', 'Constant folding, merging duplicate cells, sharing, multiplexer trees, FSM extraction, width reduction.'],
  memories_mapped: ['Map memories', 'Memories become block RAMs, or flip-flops and logic when small.'],
  gates_lowered: ['Lower to gates', 'Word-level cells become an and-inverter graph, flip-flops and carry chains.'],
  gates_optimised: ['Optimise gates', 'Rewriting, refactoring and SAT sweeping shrink the graph.'],
  mapped: ['Map to LUTs', 'The gates are covered by 4-input LUTs; flip-flops and carries become iCE40 primitives.'],
  mapped_xc7: ['Synthesise', 'Optimisation, then 6-input LUTs, MUXF7/MUXF8, carry chains, block RAMs, DSPs and shift registers: Artix-7 primitives.'],
  packed: ['Pack', 'LUTs, flip-flops and carries are paired into logic cells; IOs and global buffers are added.'],
};

function statChips(stats, prev) {
  return Object.entries(stats || {}).filter(([, n]) => n).map(([k, n]) => {
    const p = prev?.[k];
    const delta = p !== undefined && p !== n ? h('span', { class: `delta ${n < p ? 'down' : ''}` }, ` ${n < p ? '−' : '+'}${Math.abs(n - p)}`) : null;
    return h('span', {}, h('b', {}, fmtNum(n)), ` ${k}`, delta);
  });
}

export function renderPipeline(el, result, app) {
  const stages = result.stages || [];
  const times = Object.fromEntries(result.times || []);
  const chip = result.chip;
  const rows = [];
  let n = 0;
  const step = (title, desc, stats, opts = {}) => {
    n++;
    rows.push(h('div', { class: `stage ${opts.failed ? 'failed' : opts.pending ? '' : 'done'}`, onclick: opts.open },
      h('div', { class: 'num' }, n),
      h('div', {}, h('div', { class: 'title' }, title), h('div', { class: 'desc' }, desc), h('div', { class: 'stats' }, stats)),
      h('div', { class: 'open' }, opts.ms ? h('span', { class: 'ms', title: opts.msTitle }, `${opts.ms} ms`) : null, opts.open ? icon(opts.icon || 'graph') : null)));
  };
  const files = app.project.files.filter((f) => !isPins(f.name));
  const lines = files.reduce((a, f) => a + f.text.split('\n').length, 0);
  step('Source', `${files.map((f) => f.name).join(', ')}`, [h('span', {}, h('b', {}, files.length), ' files'), h('span', {}, h('b', {}, lines), ' lines')], { failed: !stages.length && !result.ok });
  let prev = null;
  for (const s of stages) {
    const [title, desc] = STAGE_INFO[s.id] || [s.title, ''];
    const mapped = s.id === 'mapped' || s.id === 'mapped_xc7';
    const ms = s.id === 'elaborated' ? times.elaborate : mapped ? times.synthesise : undefined;
    step(title, desc, statChips(s.stats, prev), { open: s.graph ? () => app.openSchematic(s.id) : undefined, ms, msTitle: mapped ? 'all of synthesis' : undefined });
    prev = s.stats;
  }
  if (chip) {
    const pips = chip.nets.reduce((a, x) => a + x.pips.length, 0);
    step('Place', 'Simulated annealing assigns every cell to a site on the die, minimising wire length and delay.',
      [h('span', {}, h('b', {}, chip.cells.length), ' sites'), h('span', {}, h('b', {}, chip.usage.lc), ` of ${chip.usage.lc_total} logic cells`)], { open: () => app.openView('chip'), icon: 'chip' });
    step('Route', 'A negotiated-congestion router connects every net through the programmable switches.',
      [h('span', {}, h('b', {}, chip.nets.length), ' nets'), h('span', {}, h('b', {}, fmtNum(pips)), ' switches'), h('span', {}, h('b', {}, chip.nodes.length), ' wires')], { open: () => app.openView('chip'), icon: 'chip', ms: times['place and route'], msTitle: 'packing, placement, routing and timing' });
  }
  if (result.placed) {
    const p = result.placed;
    step('Place and route', 'Packing into slices, placement and routing on the Artix-7 fabric, then the bitstream (no die view yet).',
      [h('span', {}, h('b', {}, fmtNum(p.slices)), ' slices'), h('span', {}, h('b', {}, p.block_rams), ' block RAMs'), h('span', {}, h('b', {}, p.dsps), ' DSPs'), h('span', {}, h('b', {}, p.ios), ' IOs')],
      { ms: times['place and route'], msTitle: 'packing, placement, routing, timing and the bitstream' });
  }
  if (result.timing) {
    const clocks = result.timing.clocks;
    step('Timing', 'Static timing analysis of the routed design.',
      clocks.length ? clocks.map((c) => h('span', {}, h('b', {}, `${c.fmax_mhz.toFixed(1)} MHz`), ` ${c.clock}`)) : [h('span', {}, 'no clocks')], { open: () => app.openView('timing'), icon: 'clock' });
  }
  if (result.bitstream) {
    const b = result.bitstream;
    step('Bitstream', 'The configuration image, read back tile by tile.',
      [h('span', {}, h('b', {}, fmtNum(b.set_bits)), ` of ${fmtNum(b.tile_bits)} bits set`), h('span', {}, h('b', {}, fmtNum(b.bytes)), ' bytes')], { open: () => app.openView('bitstream'), icon: 'bits', ms: times.export });
  }
  if (!result.ok) step('Stopped', result.error || 'failed', [], { failed: true });

  const usage = chip ? [
    ['Logic cells', chip.usage.lc, chip.usage.lc_total],
    ['IO pins', chip.usage.io, chip.usage.io_total],
    ['Block RAMs', chip.usage.ram, chip.usage.ram_total],
  ] : [];
  const fmax = result.timing?.clocks?.[0];
  fill(el, h('div', { class: 'pipeline' },
    h('h2', {}, result.ok ? `${result.top || 'design'} on ${result.family || 'iCE40'} ${(chip?.part || app.project.part || '').toUpperCase()}` : 'The flow stopped'),
    h('div', { class: 'summary' }, result.ok
      ? `${chip ? `${chip.package}, ` : ''}${stages.length} synthesis stages${fmax ? `, ${fmax.fmax_mhz.toFixed(1)} MHz on ${fmax.clock}` : ''}. Click a step to inspect it.`
      : (result.error || '')),
    usage.length ? h('div', { class: 'usage' }, usage.map(([l, a, b]) => h('div', { class: 'u' },
      h('div', { class: 'n' }, `${a}`, h('span', { class: 'muted', style: { fontSize: '13px', fontWeight: 400 } }, ` / ${b}`)),
      h('div', { class: 'l' }, `${l} · ${b ? ((100 * a) / b).toFixed(1) : 0}%`),
      h('div', { class: 'bar' }, h('i', { style: { width: `${b ? Math.max(1, (100 * a) / b) : 0}%` } }))))) : null,
    h('div', { class: 'stages' }, rows)));
}

// ----- timing -----

export function renderTiming(el, result, app) {
  const t = result.timing;
  if (!t) {
    el.replaceChildren(h('div', { class: 'timing' }, h('h2', {}, 'Timing'), h('p', { class: 'muted' }, 'Run the flow through place and route to see timing.')));
    return;
  }
  const cards = t.clocks.map((c, ci) => {
    const steps = c.path.map((p, i) => ({ ...p, d: i ? Math.max(0, p.t - c.path[i - 1].t) : p.t }));
    const totalT = Math.max(1e-9, steps.length ? steps[steps.length - 1].t : 1);
    const color = (i) => hsl((i * 47) % 360, 0.6, settings.get('theme') === 'dark' ? 0.55 : 0.5);
    const css = (c3) => `rgb(${c3.map((x) => Math.round(x * 255)).join(',')})`;
    return h('div', { class: 'clock-card' },
      h('div', { class: 'head' },
        h('span', { class: 'name' }, c.clock),
        h('span', { class: 'big' }, `${c.fmax_mhz.toFixed(2)} MHz`),
        h('span', { class: 'muted' }, `period ${c.period_ns.toFixed(3)} ns`),
        c.target_mhz ? h('span', { class: `pill ${c.met ? 'ok' : 'bad'}` }, `${c.met ? 'meets' : 'misses'} ${c.target_mhz} MHz`) : h('span', { class: 'pill', style: { background: 'var(--bg-3)' } }, 'no target'),
        h('span', { class: 'spacer', style: { flex: 1 } }),
        result.chip ? h('button', { onclick: () => app.showCriticalPath(ci) }, icon('chip'), 'Show on chip') : null),
      h('div', { class: 'pathbar', title: 'Delay of each step of the critical path' },
        steps.map((s, i) => h('i', { title: `${s.cell}.${s.pin}: +${s.d.toFixed(2)} ns`, style: { width: `${(100 * s.d) / totalT}%`, background: css(color(i)) } }))),
      h('table', { class: 'path' },
        h('tr', {}, h('th', {}, 'Cell'), h('th', {}, 'Pin'), h('th', { style: { textAlign: 'right' } }, 'Step'), h('th', { style: { textAlign: 'right' } }, 'Arrival')),
        steps.map((s, i) => h('tr', { onclick: result.chip ? () => app.focusChipCell(s.ci, ci) : null },
          h('td', {}, h('i', { class: 'sw', style: { background: css(color(i)) } }), s.cell),
          h('td', {}, s.pin),
          h('td', { class: 'num' }, `+${s.d.toFixed(2)}`),
          h('td', { class: 'num' }, `${s.t.toFixed(2)} ns`)))));
  });
  fill(el, h('div', { class: 'timing' }, h('h2', {}, 'Timing'),
    cards.length ? cards : h('p', { class: 'muted' }, 'The design has no clocked paths.')));
}

// ----- bitstream -----

const FEATURE_KEYS = { cfg: 'colLc', route: 'colSpan4', buf: 'colLocal' };

export class BitstreamView {
  constructor(el, app) {
    this.el = el;
    this.app = app;
    this.layout = h('div', { class: 'bits-layout' });
    this.map = h('div', { class: 'bits-map canvas-wrap' });
    this.side = h('div', { class: 'bits-side' });
    this.layout.append(this.map, this.side);
    el.append(this.layout);
    try {
      this.r = new Renderer(this.map);
    } catch (e) {
      this.map.append(h('div', { class: 'empty-state' }, e.message));
      return;
    }
    this.map.append(zoomBar(this.r, () => this.r.fit(0, 0, this.width, this.height, 20, 30), h, icon));
    this.bg = this.r.batch('rect');
    this.bitsBatch = this.r.batch('rect');
    this.selBatch = this.r.batch('rect');
    this.r.onOverlay = (ctx) => this.overlay(ctx);
    this.r.interact({
      onClick: (x, y) => this.pickTile(x, y),
      onHover: (x, y, e) => this.hover(x, y, e),
      onLeave: hideTooltip,
    });
    settings.on('change', (k) => { if (k === 'theme' || k.startsWith('col') || k === 'accent') this.paint(); });
  }

  show(result) {
    this.result = result;
    this.data = result.bitstream || null;
    this.chip = result.chip || null;
    this.sel = null;
    this.selFeature = -1;
    if (!this.data || !this.r) {
      fill(this.side, h('p', { class: 'muted' }, 'Run the flow through bitstream generation to explore the configuration bits.'));
      return;
    }
    // Columns of the configuration memory: each tile column is as wide as
    // its tiles' configuration blocks.
    const gw = this.chip.gw, gh = this.chip.gh;
    const widthOf = [0, 18, 54, 42, 42];
    const colW = new Array(gw).fill(0);
    for (let y = 0; y < gh; y++) for (let x = 0; x < gw; x++) colW[x] = Math.max(colW[x], widthOf[this.chip.tiles[y * gw + x]]);
    this.colX = [];
    let cx = 0;
    for (let x = 0; x < gw; x++) { this.colX.push(cx); cx += colW[x] + 4; }
    this.colW = colW;
    this.width = cx;
    this.height = gh * 20;
    this.byXY = new Map(this.data.tiles.map((t, i) => [`${t.x},${t.y}`, i]));
    this.paint();
    this.r.fit(0, 0, this.width, this.height, 20, 30);
    this.renderSide();
  }

  tileOrigin(x, y) {
    return [this.colX[x], (this.chip.gh - 1 - y) * 20];
  }

  paint() {
    if (!this.data || !this.r) return;
    const c = { bg: rgb(cssVar('--bg')), fg: rgb(cssVar('--fg')), muted: rgb(cssVar('--muted')), accent: rgb(settings.get('accent')) };
    this.r.background = [...c.bg, 1];
    const bg = this.bg.inst, bits = this.bitsBatch.inst;
    bg.clear();
    bits.clear();
    const gw = this.chip.gw, gh = this.chip.gh;
    const widthOf = [0, 18, 54, 42, 42];
    for (let y = 0; y < gh; y++) for (let x = 0; x < gw; x++) {
      const k = this.chip.tiles[y * gw + x];
      if (!k) continue;
      const [ox, oy] = this.tileOrigin(x, y);
      bg.rect(ox, oy, widthOf[k], 16, [...mix(c.bg, c.fg, 0.07), 1]);
    }
    for (const t of this.data.tiles) {
      const [ox, oy] = this.tileOrigin(t.x, t.y);
      const kindOf = new Map();
      for (const f of t.f) for (const b of f.b) kindOf.set(b, f.k);
      for (const b of t.set) {
        const col = rgb(settings.get(FEATURE_KEYS[kindOf.get(b)] || 'colFf'));
        bits.rect(ox + (b % t.w), oy + Math.floor(b / t.w), 1, 1, [...col, 1]);
      }
    }
    this.paintSel(c);
    this.r.update(this.bg, this.bitsBatch, this.selBatch);
  }

  paintSel(c = { accent: rgb(settings.get('accent')) }) {
    const sb = this.selBatch.inst;
    sb.clear();
    if (this.sel) {
      const t = this.data.tiles[this.sel] || null;
      const [x, y] = t ? [t.x, t.y] : this.selXY;
      const [ox, oy] = this.tileOrigin(x, y);
      sb.rect(ox - 1, oy - 1, (t ? t.w : 54) + 2, 18, [0, 0, 0, 0], [...c.accent, 1], 1, 2);
    }
  }

  overlay(ctx) {
    if (!this.data) return;
    const s = this.r.cam.scale;
    if (s < 1.6) return;
    ctx.font = '10px ui-monospace, Menlo, monospace';
    ctx.fillStyle = cssVar('--muted');
    ctx.textBaseline = 'bottom';
    const v = this.r.viewRect();
    for (let y = 0; y < this.chip.gh; y++) for (let x = 0; x < this.chip.gw; x++) {
      if (!this.chip.tiles[y * this.chip.gw + x]) continue;
      const [ox, oy] = this.tileOrigin(x, y);
      if (ox > v.x1 || oy > v.y1 || ox + 54 < v.x0 || oy + 20 < v.y0) continue;
      const [sx, sy] = this.r.toScreen(ox, oy);
      ctx.fillText(`${x},${y}`, sx, sy - 1);
    }
  }

  tileAtWorld(wx, wy) {
    const y = this.chip.gh - 1 - Math.floor(wy / 20);
    const x = this.colX.findIndex((cx, i) => wx >= cx && wx < cx + this.colW[i]);
    if (x < 0 || y < 0 || y >= this.chip.gh || wy - (this.chip.gh - 1 - y) * 20 >= 16) return null;
    if (!this.chip.tiles[y * this.chip.gw + x]) return null;
    return [x, y];
  }

  hover(wx, wy, e) {
    const xy = this.tileAtWorld(wx, wy);
    if (!xy) return hideTooltip();
    const i = this.byXY.get(`${xy[0]},${xy[1]}`);
    const t = i !== undefined ? this.data.tiles[i] : null;
    const [ox, oy] = this.tileOrigin(...xy);
    const col = Math.floor(wx - ox), row = Math.floor(wy - oy);
    const f = t && t.f.find((ft) => ft.b.includes(row * t.w + col));
    showTooltip(`<div><b>tile ${xy[0]},${xy[1]}</b> <span class="t">B${row}[${col}]</span></div>${f ? `<div>${esc(f.n)}</div>` : `<div class="t">${t ? `${t.set.length} bits set` : 'unconfigured'}</div>`}`, e.clientX, e.clientY);
  }

  pickTile(wx, wy) {
    const xy = this.tileAtWorld(wx, wy);
    if (!xy) return;
    this.select(...xy);
  }

  select(x, y) {
    if (!this.data) return;
    const i = this.byXY.get(`${x},${y}`);
    this.sel = i ?? null;
    this.selXY = [x, y];
    this.selFeature = -1;
    if (i === undefined) this.sel = null;
    this.paintSel();
    this.r.update(this.selBatch);
    this.renderSide(x, y);
  }

  renderSide(x, y) {
    const d = this.data;
    const binBtn = h('button', { onclick: () => download(`${this.result.top || 'design'}.bin`, fromBase64(this.result.bin)) }, icon('download'), 'Download .bin');
    if (x === undefined) {
      const kinds = new Map();
      for (const t of d.tiles) for (const f of t.f) kinds.set(f.k, (kinds.get(f.k) || 0) + f.b.length);
      const sw = (k) => h('i', { style: { background: settings.get(FEATURE_KEYS[k] || 'colFf') } });
      fill(this.side, 
        h('h3', {}, 'Configuration memory'),
        h('div', { class: 'sub' }, `${d.cram[0]} × ${d.cram[1]} bits per bank, 4 banks, ${fmtNum(d.bytes)} bytes as a .bin`),
        h('div', { class: 'kv' },
          h('span', {}, 'tile bits set'), h('span', {}, `${fmtNum(d.set_bits)} of ${fmtNum(d.tile_bits)}`),
          h('span', {}, 'tiles configured'), h('span', {}, d.tiles.length),
          h('span', {}, 'block RAM bits set'), h('span', {}, d.bram.reduce((a, b) => a + b.ones, 0))),
        h('h4', {}, 'Bits by purpose'),
        h('div', { class: 'features' },
          [['route', 'routing switches'], ['buf', 'buffers (local tracks, LUT inputs)'], ['cfg', 'cell configuration (LUTs, flip-flops, IO)']].map(([k, l]) => h('div', {}, sw(k), h('span', { class: 'n' }, l), h('span', { class: 'c' }, kinds.get(k) || 0)))),
        h('p', { class: 'muted' }, 'Each tile’s block is drawn bit for bit, in IceStorm’s B<row>[<col>] layout. Click a tile to decode it.'),
        binBtn);
      return;
    }
    const t = this.sel !== null ? d.tiles[this.sel] : null;
    const kinds = ['', 'IO', 'logic', 'RAM (bottom)', 'RAM (top)'];
    const k = this.chip.tiles[y * this.chip.gw + x];
    const w = t ? t.w : [0, 18, 54, 42, 42][k];
    const featOf = new Map();
    if (t) t.f.forEach((f, fi) => f.b.forEach((b) => featOf.set(b, fi)));
    const grid = h('div', { class: 'bitgrid', style: { gridTemplateColumns: `repeat(${w}, 1fr)` } });
    const set = new Set(t ? t.set : []);
    const cells = [];
    for (let b = 0; b < 16 * w; b++) {
      const fi = featOf.get(b);
      const on = set.has(b);
      const i = h('i', {
        class: on ? 'on' : '',
        style: on ? { background: settings.get(FEATURE_KEYS[t.f[fi]?.k] || 'colFf') } : undefined,
        onmouseenter: (e) => showTooltip(`<b>B${Math.floor(b / w)}[${b % w}]</b>${fi !== undefined ? `<div>${esc(t.f[fi].n)}</div>` : on ? '<div class="t">unexplained</div>' : '<div class="t">0</div>'}`, e.clientX, e.clientY),
        onmouseleave: hideTooltip,
        onclick: () => fi !== undefined && highlight(fi),
      });
      cells.push(i);
    }
    grid.append(...cells);
    const list = h('div', { class: 'features' });
    const highlight = (fi) => {
      this.selFeature = fi;
      cells.forEach((el, b) => el.classList.toggle('hl', featOf.get(b) === fi));
      [...list.children].forEach((el, j) => el.classList.toggle('sel', j === fi));
    };
    if (t) {
      list.append(...t.f.map((f, fi) => h('div', { onclick: () => highlight(fi) },
        h('i', { style: { background: settings.get(FEATURE_KEYS[f.k] || 'colFf') } }),
        h('span', { class: 'n' }, f.n),
        h('span', { class: 'c' }, f.b.length))));
    }
    const cellsHere = (this.chip.cells || []).map((c, ci) => [c, ci]).filter(([c]) => c.x === x && (c.y === y || (c.bel === 'ram' && c.y + 1 === y)));
    fill(this.side, 
      h('h3', {}, `${kinds[k]} tile ${x},${y}`),
      h('div', { class: 'sub' }, t ? `template ${t.t} · ${t.set.length} of ${16 * w} bits set · ${t.f.length} features` : 'no bits set'),
      grid,
      cellsHere.length ? h('div', { class: 'chips', style: { marginBottom: '8px' } }, cellsHere.map(([c, ci]) => h('span', { class: 'chip', onclick: () => this.app.focusChipCell(ci) }, c.name))) : null,
      h('h4', {}, 'Decoded'),
      t ? list : h('p', { class: 'muted' }, 'This tile is not configured.'),
      h('p', {}, h('button', { onclick: () => { this.sel = null; this.paintSel(); this.r.update(this.selBatch); this.renderSide(); } }, 'Back to overview')));
  }

  visible() {
    if (this.r) this.r.resize();
  }
}

// ----- problems and log -----

export function renderProblems(el, diags, app) {
  if (!diags.length) {
    el.replaceChildren(h('div', { class: 'empty' }, 'No problems.'));
    return;
  }
  el.replaceChildren(...diags.map((d) => {
    const sev = d.severity === 'error' ? 'error' : d.severity === 'warning' ? 'warning' : 'info';
    const where = d.at ? `${d.at.file}:${d.at.line}:${d.at.col}` : '';
    return h('div', {
      class: 'item', title: d.rendered,
      onclick: () => d.at && app.revealSource(d.at.file, d.at.start, d.at.end),
    },
    h('span', { class: d.severity }, icon(sev)),
    h('div', { class: 'msg' }, d.message, d.label ? h('span', { class: 'muted' }, ` — ${d.label}`) : null,
      (d.helps || []).map((x) => h('div', { class: 'help' }, `help: ${x}`))),
    h('span', { class: 'where' }, where));
  }));
}

export function renderLog(el, result) {
  const lines = [];
  for (const l of result.log || []) lines.push(h('div', {}, l));
  if (result.times?.length) {
    lines.push(h('div', { class: 't' }, ''));
    for (const [k, ms] of result.times) lines.push(h('div', { class: 't' }, `${k}: ${ms} ms`));
  }
  if (result.error) lines.push(h('div', { style: { color: 'var(--error)' } }, result.error));
  el.replaceChildren(...lines);
}

