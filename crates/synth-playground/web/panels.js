// The summary, timing, bitstream, problems and log panels, and the steps of
// the flow.

import { Renderer, zoomBar } from './gl.js';
import { settings } from './settings.js';
import { h, fill, icon, rgb, cssVar, mix, esc, fmtNum, hsl, showTooltip, hideTooltip, download, fromBase64, isPins } from './util.js';

// ----- the steps of the flow -----

const STAGE_INFO = {
  elaborated: ['Elaborate', 'The source becomes a flat word-level netlist: operators, multiplexers, registers, memories.'],
  optimised: ['Optimise', 'Constant folding, merging duplicate cells, sharing, multiplexer trees, FSM extraction, width reduction.'],
  memories_mapped: ['Memories', 'Memories become block RAMs, or flip-flops and logic when small.'],
  gates_lowered: ['Gates', 'Word-level cells become an and-inverter graph, flip-flops and carry chains.'],
  gates_optimised: ['Gate opt.', 'Rewriting, refactoring and SAT sweeping shrink the graph.'],
  mapped: ['LUTs', 'The gates are covered by 4-input LUTs; flip-flops and carries become iCE40 primitives.'],
  mapped_xc7: ['Synthesise', 'Optimisation, then 6-input LUTs, MUXF7/MUXF8, carry chains, block RAMs, DSPs and shift registers: Artix-7 primitives.'],
  packed: ['Pack', 'LUTs, flip-flops and carries are paired into logic cells; IOs and global buffers are added.'],
  place: ['Place', 'Simulated annealing assigns every cell to a site on the die, minimising wire length and delay.'],
  route: ['Route', 'A negotiated-congestion router connects every net through the programmable switches.'],
  pnr: ['Place & route', 'Packing into slices, placement and routing on the Artix-7 fabric.'],
  timing: ['Timing', 'Static timing analysis of the routed design.'],
  bitstream: ['Bitstream', 'The configuration image, read back tile by tile.'],
};

/** A step's name and what it does. */
export const stageInfo = (id) => STAGE_INFO[id] || [id, ''];

const SYNTH_STEPS = {
  iCE40: ['elaborated', 'optimised', 'memories_mapped', 'gates_lowered', 'gates_optimised', 'mapped'],
  'Artix-7': ['elaborated', 'mapped_xc7'],
};

/** The headline number of a synthesis stage, with its unit. */
function headline(stage) {
  const st = stage.stats || {};
  const sum = (f) => Object.entries(st).filter(([k]) => f(k)).reduce((a, [, n]) => a + n, 0);
  if (stage.kind === 'aig') return { n: st.AND || 0, unit: 'AND' };
  if (stage.id === 'packed') return { n: st.ICESTORM_LC || sum((k) => !k.startsWith('$')), unit: 'LC' };
  const luts = sum((k) => /^(SB_)?LUT/.test(k));
  if (stage.kind === 'netlist' && luts) return { n: luts, unit: stage.id === 'mapped' ? 'LUT4' : 'LUT' };
  return { n: sum((k) => !k.startsWith('$')), unit: 'cells' };
}

/**
 * The steps of the flow for the rail, in two groups (synthesis and
 * implementation): each {id, label, metric, delta, bad, view, stage, has},
 * `metric` null when the run did not get there.
 */
export function flowSteps(result, family) {
  const xc7 = family === 'Artix-7';
  const stages = new Map((result?.stages || []).map((s) => [s.id, s]));
  const chip = result?.chip, placed = result?.placed, timing = result?.timing, bits = result?.bitstream;
  const summary = chip ? `${fmtNum(chip.usage.lc)} LC` : placed ? `${fmtNum(placed.slices)} slices` : result ? (result.ok ? 'done' : 'failed') : null;
  const synth = [{ id: 'summary', label: 'Summary', metric: summary, bad: result && !result.ok, view: 'pipeline', has: true }];
  let prev = null;
  for (const id of [...SYNTH_STEPS[xc7 ? 'Artix-7' : 'iCE40'], ...(xc7 ? [] : ['packed'])]) {
    const s = stages.get(id);
    const m = s ? headline(s) : null;
    const delta = m && prev && prev.unit === m.unit && prev.n !== m.n ? m.n - prev.n : null;
    const step = { id, label: stageInfo(id)[0], metric: m ? `${fmtNum(m.n)} ${m.unit}` : null, delta, view: 'schematic', stage: id, has: !!s?.graph };
    if (id === 'packed') step.group = 1;
    synth.push(step);
    if (m) prev = m;
  }
  const impl = synth.filter((s) => s.group === 1);
  const clk = timing?.clocks?.[0];
  const missed = timing?.clocks?.some((c) => c.target_mhz && !c.met);
  const timingStep = { id: 'timing', label: 'Timing', metric: timing ? (clk ? `${clk.fmax_mhz.toFixed(1)} MHz` : 'no clocks') : null, bad: missed, view: 'timing', has: !!timing };
  if (xc7) {
    impl.push(
      { id: 'pnr', label: stageInfo('pnr')[0], metric: placed ? `${fmtNum(placed.slices)} slices` : null, view: 'pipeline', has: !!placed },
      timingStep,
      { id: 'bitstream', label: 'Bitstream', metric: result?.bin ? `${Math.round((result.bin.length * 3) / 4 / 1024)} KB` : null, view: 'pipeline', has: !!result?.bin });
  } else {
    const pct = chip ? Math.max(1, Math.round((100 * chip.usage.lc) / chip.usage.lc_total)) : 0;
    impl.push(
      { id: 'place', label: 'Place', metric: chip ? `${pct}% die` : null, view: 'chip', has: !!chip },
      { id: 'route', label: 'Route', metric: chip ? `${fmtNum(chip.nodes.length)} wires` : null, view: 'chip', has: !!chip },
      timingStep,
      { id: 'bitstream', label: 'Bitstream', metric: bits ? `${Math.round(bits.bytes / 1024)} KB` : null, view: 'bitstream', has: !!bits });
  }
  return [
    { title: 'Synthesis', steps: synth.filter((s) => s.group !== 1) },
    { title: 'Implementation', steps: impl },
  ];
}

// ----- summary -----

/** What a stage's statistics say, the largest counts first. */
function statLine(stats, max = 4) {
  return Object.entries(stats || {}).filter(([k, n]) => n && !k.startsWith('$'))
    .sort((a, b) => b[1] - a[1]).slice(0, max).map(([k, n]) => `${fmtNum(n)} ${k}`).join(' · ');
}

/** The critical path of a clock as steps: {cell, pin, d, t, wire}; a step
 *  that ends on a cell's input is the wire to it, one that ends on an output
 *  the cell itself. */
function pathSteps(c) {
  return c.path.map((p, i) => ({ ...p, d: i ? Math.max(0, p.t - c.path[i - 1].t) : p.t, wire: i > 0 && !/^(O|Q|CO|LO|COUT)$/.test(p.pin) }));
}

const fmtMs = (ms) => (ms >= 1000 ? `${(ms / 1000).toFixed(2)} s` : `${Math.round(ms)} ms`);

function kpi(label, value, of, frac, note, cls = '') {
  return h('div', { class: 'card kpi' },
    h('span', { class: 'k' }, label),
    h('div', { class: 'v' }, h('span', { class: 'big' }, value), of ? h('span', { class: 'of' }, of) : null),
    frac === null ? null : h('div', { class: 'meter' }, h('i', { class: cls, style: { width: `${Math.min(100, Math.max(frac > 0 ? 1 : 0, 100 * frac))}%` } })),
    note ? h('span', { class: 'n' }, note) : null);
}

export function renderSummary(el, result, app) {
  const stages = result.stages || [];
  const times = Object.fromEntries(result.times || []);
  const chip = result.chip, placed = result.placed;
  const board = app.boardName();
  const where = board ? `the ${board}` : (chip?.part || app.project.part || '').toUpperCase();
  const clocks = result.timing?.clocks || [];
  const clk = clocks[0];
  const missed = clocks.filter((c) => c.target_mhz && !c.met);
  const diags = result.diagnostics || [];
  const errors = diags.filter((d) => d.severity === 'error');

  // The sentence under the title.
  let line;
  if (!result.ok) line = result.error || `${errors.length} error${errors.length === 1 ? '' : 's'}`;
  else {
    const parts = [];
    if (chip) parts.push(`uses ${Math.max(1, Math.round((100 * chip.usage.lc) / chip.usage.lc_total))}% of the logic`);
    if (clocks.length) parts.push(missed.length ? `misses its ${missed[0].target_mhz} MHz target on ${missed[0].clock}` : clocks.some((c) => c.target_mhz) ? 'meets timing' : `runs up to ${clk.fmax_mhz.toFixed(1)} MHz`);
    if (result.bin) parts.push('the bitstream is ready');
    line = parts.length ? `${parts.join(', ').replace(/^./, (c) => c.toUpperCase())}.` : 'Synthesised.';
  }

  // Figures.
  let kpis = [];
  if (chip) {
    const u = chip.usage;
    kpis = [
      kpi('Logic cells', fmtNum(u.lc), `/ ${fmtNum(u.lc_total)}`, u.lc / u.lc_total, statLine(stages.find((s) => s.id === 'mapped')?.stats, 3)),
      clk ? kpi(`Fmax · ${clk.clock}`, clk.fmax_mhz.toFixed(1), 'MHz', clk.target_mhz ? Math.min(1, clk.fmax_mhz / clk.target_mhz / 2) : null,
        clk.target_mhz ? h('span', {}, `Target ${clk.target_mhz} MHz · `, h('span', { class: clk.met ? 'good' : 'bad' }, `${clk.met ? '+' : ''}${(1000 / clk.target_mhz - clk.period_ns).toFixed(1)} ns slack`)) : 'No target set') : null,
      kpi('IO pins', fmtNum(u.io), `/ ${fmtNum(u.io_total)}`, u.io / u.io_total, null, 'alt'),
      kpi('Block RAM', fmtNum(u.ram), `/ ${fmtNum(u.ram_total)}`, u.ram_total ? u.ram / u.ram_total : 0, null, 'alt'),
    ];
  } else if (placed) {
    kpis = [
      kpi('Slices', fmtNum(placed.slices), null, null),
      clk ? kpi(`Fmax · ${clk.clock}`, clk.fmax_mhz.toFixed(1), 'MHz', null, clk.target_mhz ? `Target ${clk.target_mhz} MHz` : null) : null,
      kpi('Block RAMs', fmtNum(placed.block_rams), null, null),
      kpi('DSPs', fmtNum(placed.dsps), null, null),
      kpi('IOs', fmtNum(placed.ios), null, null),
    ];
  }

  // The steps, with their results and times.
  const total = Object.values(times).reduce((a, b) => a + b, 0) || 1;
  const timeOf = { elaborated: 'elaborate', mapped: 'synthesise', mapped_xc7: 'synthesise', route: 'place and route', pnr: 'place and route', bitstream: 'export' };
  const timeTitle = { synthesise: 'all of synthesis', 'place and route': 'packing, placement, routing and timing', export: 'the views and the bitstream' };
  const rows = [];
  const row = (id, result, delta, opts = {}) => {
    const t = times[timeOf[id]];
    rows.push(h('div', { class: `srow${opts.open ? ' link' : ''}${opts.failed ? ' failed' : ''}`, onclick: opts.open, title: stageInfo(id)[1] },
      h('span', {}, opts.label || stageInfo(id)[0]),
      h('span', { class: 's' }, result),
      h('span', { class: `d${delta < 0 ? ' down' : ''}` }, delta ? `${delta < 0 ? '−' : '+'}${fmtNum(Math.abs(delta))}` : '—'),
      h('span', { class: 'tm', title: t !== undefined ? timeTitle[timeOf[id]] : undefined },
        h('span', { class: 'meter' }, t !== undefined ? h('i', { class: t / total > 0.3 ? '' : 'quiet', style: { width: `${Math.max(2, (100 * t) / total)}%` } }) : null),
        h('span', {}, t !== undefined ? fmtMs(t) : ''))));
  };
  let prev = null;
  for (const s of stages) {
    const m = headline(s);
    row(s.id, statLine(s.stats) || '—', prev && prev.unit === m.unit ? m.n - prev.n : 0, { open: s.graph ? () => app.openStep(s.id) : undefined });
    prev = m;
  }
  if (chip) {
    const pips = chip.nets.reduce((a, x) => a + x.pips.length, 0);
    row('place', `${fmtNum(chip.cells.length)} sites · ${fmtNum(chip.usage.lc)} of ${fmtNum(chip.usage.lc_total)} logic cells`, 0, { open: () => app.openStep('place') });
    row('route', `${fmtNum(chip.nets.length)} nets · ${fmtNum(pips)} switches · ${fmtNum(chip.nodes.length)} wires`, 0, { open: () => app.openStep('route') });
  }
  if (placed) row('pnr', `${fmtNum(placed.slices)} slices · ${placed.block_rams} block RAMs · ${placed.dsps} DSPs · ${placed.ios} IOs`, 0);
  if (result.timing) row('timing', clocks.length ? clocks.map((c) => `${c.clock} ${c.fmax_mhz.toFixed(1)} MHz`).join(' · ') : 'no clocks', 0, { open: () => app.openStep('timing') });
  if (result.bitstream) row('bitstream', `${fmtNum(result.bitstream.bytes)} bytes · ${fmtNum(result.bitstream.set_bits)} of ${fmtNum(result.bitstream.tile_bits)} bits set`, 0, { open: () => app.openStep('bitstream') });
  else if (result.bin && placed) row('bitstream', `${fmtNum(Math.round((result.bin.length * 3) / 4))} bytes`, 0);
  if (!result.ok) row('stopped', result.error || 'failed', 0, { label: 'Stopped', failed: true });

  // The critical path.
  let pathCard = null;
  if (clk && clk.path.length) {
    const steps = pathSteps(clk);
    const logic = steps.filter((s) => !s.wire).reduce((a, s) => a + s.d, 0);
    const wires = steps.filter((s) => s.wire).reduce((a, s) => a + s.d, 0);
    const end = steps[steps.length - 1], start = steps[0];
    pathCard = h('div', { class: 'card', style: { display: 'flex', flexDirection: 'column', gap: '12px' } },
      h('div', { class: 'card-head' }, h('h2', {}, `Critical path · ${clk.clock}`), h('span', { class: 'mono muted', style: { fontSize: '11.5px' } }, `${end.t.toFixed(1)} ns`)),
      h('div', { class: 'pathbar' }, steps.map((s) => h('i', { style: { flex: `${Math.max(s.d, 0.01)}`, background: s.wire ? 'var(--faint)' : 'var(--note)' } }))),
      h('div', { class: 'legend-row' },
        h('span', {}, h('i', { class: 'sw', style: { background: 'var(--note)' } }), `Cells ${logic.toFixed(1)} ns`),
        h('span', {}, h('i', { class: 'sw', style: { background: 'var(--faint)' } }), `Wires ${wires.toFixed(1)} ns`)),
      h('span', { class: 'mono', style: { fontSize: '11.5px', color: 'var(--fg-1)', wordBreak: 'break-all' } }, `${start.cell} → ${end.cell}`),
      h('a', { href: '#', style: { fontSize: '12px', textDecoration: 'none' }, onclick: (e) => { e.preventDefault(); app.openStep('timing'); } }, 'Open timing →'));
  }

  // Problems.
  const shown = diags.filter((d) => d.severity === 'error' || d.severity === 'warning');
  const probCard = h('div', { class: 'card', style: { display: 'flex', flexDirection: 'column', gap: '8px' } },
    h('div', { class: 'card-head' }, h('h2', {}, 'Problems'),
      h('span', { class: `badge ${errors.length ? 'error' : shown.length ? 'warning' : ''}` }, shown.length || '')),
    shown.length ? shown.slice(0, 4).map((d) => h('button', { class: 'mini-problem', onclick: () => d.at && app.revealSource(d.at.file, d.at.start, d.at.end, app.resultTexts) },
      h('span', { class: d.severity === 'error' ? 'e' : 'w', style: { color: d.severity === 'error' ? 'var(--error)' : 'var(--warning)' } }, icon(d.severity === 'error' ? 'error' : 'warning')),
      h('span', {}, d.message, d.at ? h('span', { class: 'w' }, `${d.at.file}:${d.at.line}`) : null)))
      : h('span', { class: 'muted' }, 'None.'),
    shown.length > 4 ? h('a', { href: '#', style: { fontSize: '12px', textDecoration: 'none' }, onclick: (e) => { e.preventDefault(); app.showProblems(); } }, `All ${shown.length} →`) : null);

  const dl = result.bin ? h('button', { onclick: () => app.downloadBitstream() }, icon('download'), result.bin_name || 'bitstream') : null;
  fill(el, h('div', { class: 'page' },
    h('div', { class: 'page-head' },
      h('div', {},
        h('p', { class: 'h4' }, 'Summary'),
        h('h1', {}, result.ok ? `${result.top || 'design'} on ${where}` : 'The flow stopped'),
        h('p', {}, line)),
      dl),
    kpis.length ? h('div', { class: 'kpis' }, kpis) : null,
    h('div', { class: 'cols' },
      h('div', { class: 'card steps-table wide' },
        h('div', { class: 'card-head' }, h('h2', {}, 'Steps'), h('span', { class: 'muted', style: { fontSize: '12px' } }, 'Click a step to open it')),
        h('div', { class: 'srow head' }, h('span', {}, 'Step'), h('span', {}, 'Result'), h('span', { style: { textAlign: 'right' } }, 'Change'), h('span', {}, 'Time')),
        rows),
      h('div', { class: 'narrow' }, pathCard, probCard))));
}

// ----- timing -----

export function renderTiming(el, result, app) {
  const t = result.timing;
  const page = (...c) => fill(el, h('div', { class: 'page timing' }, h('div', { class: 'page-head' }, h('div', {}, h('p', { class: 'h4' }, 'Timing'), h('h1', {}, 'Clocks and critical paths'))), ...c));
  if (!t) return page(h('p', { class: 'muted' }, 'Run the flow through place and route to see timing.'));
  if (!t.clocks.length) return page(h('p', { class: 'muted' }, 'The design has no clocked paths.'));
  const cards = t.clocks.map((c, ci) => {
    const steps = pathSteps(c);
    const end = steps.length ? steps[steps.length - 1].t : 0;
    const budget = c.target_mhz ? 1000 / c.target_mhz : null;
    const slack = budget !== null ? budget - c.period_ns : null;
    const logic = steps.filter((s) => !s.wire).reduce((a, s) => a + s.d, 0);
    const pill = c.target_mhz
      ? h('span', { class: `pill ${c.met ? 'ok' : 'bad'}` }, icon(c.met ? 'check' : 'x'), c.met ? 'Meets timing' : 'Misses timing')
      : h('span', { class: 'pill none' }, 'No target');
    const fig = (k, v, style) => h('div', { class: 'fig' }, h('span', { class: 'k' }, k), h('span', { class: 'v', style }, v));
    const width = budget ? Math.min(100, (100 * end) / budget) : 100;
    return [
      h('div', { class: 'card clock-card' },
        h('div', { class: 'head' },
          h('div', { class: 'fig' }, h('span', { class: 'k' }, `Fmax · ${c.clock}`), h('span', { class: 'big' }, c.fmax_mhz.toFixed(1), ' ', h('small', {}, 'MHz'))),
          fig('Period', `${c.period_ns.toFixed(2)} ns`),
          c.target_mhz ? fig('Target', `${c.target_mhz} MHz · ${budget.toFixed(1)} ns`) : null,
          slack !== null ? fig('Slack', `${slack >= 0 ? '+' : ''}${slack.toFixed(2)} ns`, { color: slack >= 0 ? 'var(--accent-ink)' : 'var(--error)' }) : null,
          h('span', { class: 'spacer' }),
          pill),
        steps.length ? h('div', { class: 'bar-wrap' },
          h('div', { class: 'cap' }, h('span', {}, `Critical path · ${end.toFixed(2)} ns${budget ? ` of ${budget.toFixed(1)} ns` : ''}`), h('span', { class: 'mono' }, `${steps[0].cell} → ${steps[steps.length - 1].cell}`)),
          h('div', { class: 'pathbar' }, h('div', { style: { width: `${width}%`, display: 'flex', gap: '1px' } },
            steps.map((s) => h('i', { title: `${s.cell}.${s.pin}: +${s.d.toFixed(2)} ns`, style: { flex: `${Math.max(s.d, 0.01)}`, background: s.wire ? 'var(--faint)' : 'var(--note)' } })))),
          h('div', { class: 'legend-row' },
            h('span', {}, h('i', { class: 'sw', style: { background: 'var(--note)' } }), `Cells ${logic.toFixed(2)} ns`),
            h('span', {}, h('i', { class: 'sw', style: { background: 'var(--faint)' } }), `Wires ${(end - logic).toFixed(2)} ns`),
            slack !== null && slack > 0 ? h('span', {}, h('i', { class: 'sw', style: { background: 'var(--bg-3)', boxShadow: 'inset 0 0 0 1px var(--line-2)' } }), `Slack ${slack.toFixed(2)} ns`) : null)) : null),
      steps.length ? h('div', { class: 'card path-card' },
        h('div', { class: 'card-head' }, h('h2', {}, `Critical path of ${c.clock}, step by step`),
          result.chip ? h('button', { class: 'ghost', onclick: () => app.showCriticalPath(ci) }, icon('chip'), 'Show on chip') : null),
        h('table', { class: 'path' },
          h('tr', {}, h('th', {}, '#'), h('th', {}, 'Through'), h('th', {}, 'Pin'), h('th', {}, 'Kind'), h('th', { class: 'num' }, 'Delay'), h('th', { class: 'num' }, 'Arrival')),
          steps.map((s, i) => h('tr', { class: result.chip ? 'link' : '', onclick: result.chip ? () => app.focusChipCell(s.ci, ci) : null },
            h('td', { class: 'muted' }, i + 1),
            h('td', {}, s.cell),
            h('td', { class: 'muted' }, s.pin),
            h('td', {}, h('i', { class: 'sw', style: { background: s.wire ? 'var(--faint)' : 'var(--note)' } }), s.wire ? 'wire' : i ? 'cell' : 'launch'),
            h('td', { class: 'num' }, `+${s.d.toFixed(2)}`),
            h('td', { class: 'num muted' }, s.t.toFixed(2)))))) : null,
    ];
  });
  page(cards);
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

