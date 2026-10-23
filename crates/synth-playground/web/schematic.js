// The schematic view: any stage's graph (RTL, gates, cells), with instances
// collapsible from the module level down to individual gates.

import { Renderer, rgba, zoomBar } from './gl.js';
import { layout } from './layout.js';
import { settings } from './settings.js';
import { h, fill, icon, rgb, cssVar, mix, esc, showTooltip, hideTooltip, toast } from './util.js';

const PORT_DY = 16;
const HEADER = 22;
const FONT = '11px ui-monospace, "JetBrains Mono", Menlo, Consolas, monospace';
const FONT_UI = '600 11.5px system-ui, -apple-system, "Segoe UI", sans-serif';

const KIND_COLOR = {
  op: 'colLogic', and: 'colLogic', dff: 'colReg', ff: 'colReg', latch: 'colReg',
  mux: 'colMux', pmux: 'colMux', shiftx: 'colMux', carry: 'colMux',
  mem: 'colMem', inst: 'colInst', group: 'colInst', lut: 'colLogic', lc: 'colInst', cell: 'colInst',
  io: 'colPort', gb: 'colPort', in: 'colPort', out: 'colPort',
};

let measureCtx = null;
const widthCache = new Map();
function textWidth(s, font = FONT) {
  const k = font + s;
  let w = widthCache.get(k);
  if (w === undefined) {
    measureCtx = measureCtx || document.createElement('canvas').getContext('2d');
    measureCtx.font = font;
    w = measureCtx.measureText(s).width;
    widthCache.set(k, w);
  }
  return w;
}

function ancestors(path) {
  if (!path) return [];
  const parts = path.split('.');
  return parts.map((_, i) => parts.slice(0, i + 1).join('.'));
}

function shortName(name, path) {
  return path && name.startsWith(`${path}.`) ? name.slice(path.length + 1) : name;
}

export class Schematic {
  constructor(el, app) {
    this.el = el;
    this.app = app;
    this.stages = [];
    this.stage = null;
    this.collapsed = new Set();
    this.selected = -1;
    this.hover = -1;
    this.built = null;
    el.classList.add('canvas-wrap');
    this.toolbar = h('div', { class: 'toolbar' });
    this.inspector = h('div', { class: 'inspector', hidden: true });
    this.legend = h('div', { class: 'legend' });
    el.append(this.toolbar, this.inspector, this.legend);
    try {
      this.r = new Renderer(el);
    } catch (e) {
      el.append(h('div', { class: 'empty-state' }, String(e.message)));
      return;
    }
    this.r.insets = { top: 46, right: 0, bottom: 40, left: 0 };
    el.append(zoomBar(this.r, () => this.fit(), h, icon));
    this.frames = this.r.batch('rect');
    this.edgeBatch = this.r.batch('line');
    this.nodeBatch = this.r.batch('rect');
    this.hiBatch = this.r.batch('line');
    this.portBatch = this.r.batch('rect');
    this.r.onOverlay = (ctx) => this.overlay(ctx);
    this.r.interact({
      onHover: (x, y, e) => this.onHover(x, y, e),
      onClick: (x, y) => this.select(this.pick(x, y)),
      onDoubleClick: (x, y) => this.onDouble(x, y),
      onLeave: () => { this.setHover(-1); hideTooltip(); },
    });
    el.tabIndex = 0;
    el.addEventListener('keydown', (e) => {
      if (e.key === 'f') this.fit();
      if (e.key === 'Escape') this.select(-1);
    });
    settings.on('change', (k) => {
      if (k.startsWith('sch') || k.startsWith('col') || k === 'theme' || k === 'accent') {
        if (k === 'schDepth') this.resetCollapse();
        this.rebuild(k === 'schDepth');
      }
    });
    this.renderToolbar();
  }

  /** New run results. */
  show(result) {
    this.stages = (result.stages || []).filter((s) => s.graph);
    const keep = this.stage && this.stages.find((s) => s.id === this.stage.id);
    this.stage = keep || this.stages.find((s) => s.id === 'optimised') || this.stages[0] || null;
    this.selected = -1;
    this.inspector.hidden = true;
    this.resetCollapse();
    this.renderToolbar();
    this.rebuild(true);
  }

  setStage(id) {
    const s = this.stages.find((x) => x.id === id);
    if (!s) return;
    this.stage = s;
    this.selected = -1;
    this.inspector.hidden = true;
    this.resetCollapse();
    this.renderToolbar();
    this.rebuild(true);
  }

  resetCollapse() {
    this.collapsed = new Set();
    if (!this.stage) return;
    const depth = settings.get('schDepth');
    for (const n of this.stage.graph.nodes) {
      for (const a of ancestors(n.path)) if (a.split('.').length > depth) this.collapsed.add(a);
    }
    // Large graphs: collapse further until under the limit.
    const max = settings.get('schMaxNodes');
    let d = depth;
    while (d >= 0 && this.visibleCount() > max) {
      d--;
      for (const n of this.stage.graph.nodes) for (const a of ancestors(n.path)) if (a.split('.').length > Math.max(d, 0)) this.collapsed.add(a);
      if (d <= 0) break;
    }
    this.autoCollapsed = d !== depth;
  }

  visibleCount() {
    const seen = new Set();
    for (const n of this.stage.graph.nodes) seen.add(this.itemKey(n));
    return seen.size;
  }

  itemKey(n) {
    for (const a of ancestors(n.path)) if (this.collapsed.has(a)) return `g:${a}`;
    return n;
  }

  renderToolbar() {
    const tb = this.toolbar;
    const search = h('input', {
      type: 'search', placeholder: 'Find a cell or signal…',
      onkeydown: (e) => { if (e.key === 'Enter') this.find(e.target.value); },
    });
    fill(tb, 
      h('div', { class: 'group' },
        h('button', { title: 'Collapse every instance (module level)', onclick: () => this.collapseAll() }, icon('minus'), 'Modules'),
        h('button', { title: 'Expand every instance (down to the cells)', onclick: () => this.expandAll() }, icon('plus'), 'Cells')),
      h('div', { class: 'group' }, h('label', {}, icon('search')), search),
    );
  }

  collapseAll() {
    if (!this.stage) return;
    for (const n of this.stage.graph.nodes) for (const a of ancestors(n.path)) this.collapsed.add(a);
    this.rebuild(true);
  }

  expandAll() {
    if (!this.stage) return;
    const n = this.stage.graph.nodes.length;
    if (n > settings.get('schMaxNodes') * 2) toast(`${n} nodes: expanding everything may be slow`);
    this.collapsed.clear();
    this.rebuild(true);
  }

  toggle(path) {
    if (this.collapsed.has(path)) this.collapsed.delete(path);
    else {
      this.collapsed.add(path);
      for (const c of [...this.collapsed]) if (c.startsWith(`${path}.`)) this.collapsed.delete(c);
    }
    this.rebuild(false, path);
  }

  /** Builds the visible graph and its layout. */
  rebuild(fit, focusPath) {
    if (!this.r) return;
    if (!this.stage) {
      this.built = null;
      for (const b of [this.frames, this.edgeBatch, this.nodeBatch, this.hiBatch, this.portBatch]) b.inst.clear();
      this.r.update(this.frames, this.edgeBatch, this.nodeBatch, this.hiBatch, this.portBatch);
      return;
    }
    const g = this.stage.graph;
    const items = [];
    const itemOf = new Int32Array(g.nodes.length);
    const groupItem = new Map();
    // Constants become labels of the inputs they drive.
    const constOf = new Map();
    g.edges.forEach((e) => {
      const src = g.nodes[e.s];
      if (src.k === 'const') constOf.set(`${e.d}:${e.dp}`, e.inv ? (src.label === '0' ? '1' : `~${src.label}`) : src.label);
    });
    g.nodes.forEach((n, i) => {
      if (n.k === 'const') {
        itemOf[i] = -1;
        return;
      }
      const key = this.itemKey(n);
      if (typeof key === 'string') {
        let gi = groupItem.get(key);
        if (gi === undefined) {
          const path = key.slice(2);
          gi = items.length;
          groupItem.set(key, gi);
          items.push({ k: 'group', label: path.split('.').pop(), name: path, path: path.split('.').slice(0, -1).join('.'), groupPath: path, members: [], ins: [], outs: [], inKeys: new Map(), outKeys: new Map() });
        }
        items[gi].members.push(i);
        itemOf[i] = gi;
      } else {
        itemOf[i] = items.length;
        items.push({
          ...n, orig: i,
          ins: n.ins.map((p, k) => {
            const c = constOf.get(`${i}:${k}`);
            return { name: c ? `${p[0] ? `${p[0]} = ` : ''}${c}` : p[0], w: p[1], konst: c };
          }),
          outs: n.outs.map((p) => ({ name: p[0], w: p[1] })),
        });
      }
    });
    // Edges between visible items; boundary ports for collapsed instances.
    const edges = [];
    const edgeKey = new Map();
    g.edges.forEach((e) => {
      const S = itemOf[e.s], D = itemOf[e.d];
      if (S === D || S < 0 || D < 0) return;
      let sp = e.sp, dp = e.dp;
      if (items[S].k === 'group') {
        const k = `${e.s}:${e.sp}`;
        let p = items[S].outKeys.get(k);
        if (p === undefined) {
          p = items[S].outs.length;
          items[S].outKeys.set(k, p);
          items[S].outs.push({ name: shortName(e.n || g.nodes[e.s].outs[e.sp]?.[0] || '', items[S].groupPath), w: e.w });
        }
        items[S].outs[p].w = Math.max(items[S].outs[p].w, e.w);
        sp = p;
      }
      if (items[D].k === 'group') {
        const k = `${e.s}:${e.sp}`;
        let p = items[D].inKeys.get(k);
        if (p === undefined) {
          p = items[D].ins.length;
          items[D].inKeys.set(k, p);
          items[D].ins.push({ name: shortName(e.n || '', items[D].groupPath), w: e.w });
        }
        items[D].ins[p].w = Math.max(items[D].ins[p].w, e.w);
        dp = p;
      }
      const key = `${S}:${sp}>${D}:${dp}`;
      const prev = edgeKey.get(key);
      if (prev !== undefined) {
        edges[prev].w = Math.max(edges[prev].w, e.w);
        return;
      }
      edgeKey.set(key, edges.length);
      edges.push({ s: S, sp, d: D, dp, w: e.w, n: e.n || '', inv: !!e.inv });
    });
    // Sizes and port offsets.
    const portNames = settings.get('schPortNames');
    for (const it of items) {
      const nIn = it.ins.length, nOut = it.outs.length;
      const ports = Math.max(nIn, nOut, 1);
      const label = it.k === 'group' ? `${it.label}` : it.label;
      const lw = textWidth(label, FONT_UI);
      const longIn = portNames || it.ins.some((p) => p.konst) ? Math.max(0, ...it.ins.map((p) => textWidth(p.name))) : 0;
      const longOut = portNames ? Math.max(0, ...it.outs.map((p) => textWidth(p.name))) : 0;
      let w, hgt, top;
      switch (it.k) {
        case 'in': case 'out': w = Math.max(40, lw + 26); hgt = 22; top = 11; break;
        case 'const': w = Math.max(30, textWidth(it.label) + 14); hgt = 18; top = 9; break;
        case 'and': w = 26; hgt = 26; top = 6; break;
        case 'op': w = Math.max(40, lw + 18, longIn + 22); hgt = Math.max(30, ports * PORT_DY + 8); top = (hgt - (ports - 1) * PORT_DY) / 2; break;
        case 'mux': case 'pmux': case 'shiftx': w = Math.max(38, lw + 14); hgt = Math.max(34, ports * PORT_DY + 10); top = (hgt - (ports - 1) * PORT_DY) / 2; break;
        case 'dff': case 'ff': case 'latch': w = Math.max(64, (portNames ? longIn + longOut + 26 : 0)); hgt = Math.max(40, ports * PORT_DY + 14); top = (hgt - (ports - 1) * PORT_DY) / 2 + 3; break;
        default:
          w = Math.max(110, lw + 30, longIn + longOut + 34);
          hgt = HEADER + ports * PORT_DY + 6;
          top = HEADER + PORT_DY / 2;
      }
      const span = (k) => (k === 1 ? [hgt / 2] : null);
      it.w = w;
      it.h = hgt;
      it.inY = span(nIn) && it.k !== 'group' && !['inst', 'cell', 'lut', 'lc', 'io', 'gb', 'mem', 'carry'].includes(it.k) ? [hgt / 2] : it.ins.map((_, i) => top + i * PORT_DY);
      it.outY = span(nOut) && it.k !== 'group' && !['inst', 'cell', 'lut', 'lc', 'io', 'gb', 'mem', 'carry'].includes(it.k) ? [hgt / 2] : it.outs.map((_, i) => top + i * PORT_DY);
      it.role = it.k === 'in' ? 'in' : it.k === 'out' ? 'out' : null;
      it.group = it.k === 'group' ? it.path : it.path || '';
    }
    const lay = layout({
      nodes: items.map((it) => ({ w: it.w, h: it.h, ins: it.inY, outs: it.outY, group: it.group, role: it.role })),
      edges,
    });
    this.built = { items, edges, lay, itemOf, g };
    this.buildIndex();
    this.selected = Math.min(this.selected, items.length - 1);
    this.legendUpdate();
    this.paint();
    if (fit) this.fit();
    else if (focusPath !== undefined) {
      const i = items.findIndex((it) => it.groupPath === focusPath || it.path === focusPath || (it.path || '').startsWith(`${focusPath}.`));
      if (i >= 0 && this.r.cam.scale < 0.4) this.r.panTo(lay.x[i] + items[i].w / 2, lay.y[i] + items[i].h / 2, 0.8);
    }
    if (this.autoCollapsed) {
      toast(`Large graph (${g.nodes.length} nodes): instances start collapsed`);
      this.autoCollapsed = false;
    }
  }

  buildIndex() {
    const { items, lay } = this.built;
    const cell = 200;
    const grid = new Map();
    items.forEach((it, i) => {
      const x0 = Math.floor(lay.x[i] / cell), x1 = Math.floor((lay.x[i] + it.w) / cell);
      const y0 = Math.floor(lay.y[i] / cell), y1 = Math.floor((lay.y[i] + it.h) / cell);
      for (let gx = x0; gx <= x1; gx++) for (let gy = y0; gy <= y1; gy++) {
        const k = `${gx},${gy}`;
        if (!grid.has(k)) grid.set(k, []);
        grid.get(k).push(i);
      }
    });
    this.grid = { cell, map: grid };
    // Bounding boxes of expanded instances.
    const boxes = new Map();
    items.forEach((it, i) => {
      for (const a of ancestors(it.k === 'group' ? it.path : it.path)) {
        if (this.collapsed.has(a)) continue;
        const b = boxes.get(a) || { x0: Infinity, y0: Infinity, x1: -Infinity, y1: -Infinity, n: 0 };
        b.x0 = Math.min(b.x0, lay.x[i]);
        b.y0 = Math.min(b.y0, lay.y[i]);
        b.x1 = Math.max(b.x1, lay.x[i] + it.w);
        b.y1 = Math.max(b.y1, lay.y[i] + it.h);
        b.n++;
        boxes.set(a, b);
      }
    });
    this.boxes = [...boxes.entries()].sort((a, b) => a[0].split('.').length - b[0].split('.').length);
  }

  colors() {
    const theme = settings.get('theme');
    return {
      fg: rgb(cssVar('--fg')),
      muted: rgb(cssVar('--muted')),
      bg: rgb(cssVar('--bg')),
      line: rgb(cssVar('--line-2')),
      accent: rgb(settings.get('accent')),
      dark: theme === 'dark',
    };
  }

  nodeColor(it, c) {
    if (it.k === 'const') return mix(c.bg, c.fg, 0.12);
    const key = KIND_COLOR[it.k] || 'colInst';
    const base = rgb(settings.get(key));
    return c.dark ? base : mix(base, [1, 1, 1], 0.55);
  }

  paint() {
    if (!this.built) return;
    const { items, edges, lay } = this.built;
    const c = this.colors();
    this.c = c;
    this.r.background = [...c.bg, 1];
    // Instance frames.
    const fr = this.frames.inst;
    fr.clear();
    for (const [path, b] of this.boxes) {
      const d = path.split('.').length;
      const pad = 16 + 6 * Math.max(0, 4 - d);
      const fill = rgba(mix(c.bg, c.accent, 0.05 + 0.025 * d), 1);
      fr.rect(b.x0 - pad, b.y0 - pad - 16, b.x1 - b.x0 + 2 * pad, b.y1 - b.y0 + 2 * pad + 16, fill, rgba(mix(c.bg, c.accent, 0.35), 0.8), 10, 1);
    }
    // Wires.
    const eb = this.edgeBatch.inst;
    eb.clear();
    const wire = rgba(mix(c.bg, c.fg, 0.42), 0.9);
    for (const e of edges) this.drawEdge(eb, e, wire, e.w > 1 ? 2.2 : 1.1);
    // Nodes and ports.
    const nb = this.nodeBatch.inst;
    nb.clear();
    const pb = this.portBatch.inst;
    pb.clear();
    items.forEach((it, i) => {
      const x = lay.x[i], y = lay.y[i];
      const col = this.nodeColor(it, c);
      const sel = i === this.selected, hov = i === this.hover;
      const border = sel ? c.accent : hov ? mix(col, c.fg, 0.6) : mix(col, c.fg, 0.25);
      const radius = it.k === 'and' ? 13 : it.k === 'in' || it.k === 'out' ? 11 : it.k === 'op' ? 9 : 5;
      nb.rect(x, y, it.w, it.h, [...col, 1], [...border, 1], radius, sel ? 2.2 : 1);
      if (it.k === 'group') nb.rect(x + 3, y + 3, it.w - 6, HEADER - 6, [...mix(col, c.fg, 0.12), 1], null, 3, 0);
      const pc = [...mix(col, c.fg, 0.55), 1];
      it.inY.forEach((py) => pb.rect(x - 3, y + py - 3, 6, 6, pc, null, 3, 0));
      it.outY.forEach((py) => pb.rect(x + it.w - 3, y + py - 3, 6, 6, pc, null, 3, 0));
    });
    // Inverted AIG edges: a bubble at the input.
    edges.forEach((e) => {
      if (!e.inv) return;
      const d = items[e.d];
      const px = lay.x[e.d], py = lay.y[e.d] + (d.inY[e.dp] ?? d.h / 2);
      pb.rect(px - 9, py - 4, 8, 8, [...c.bg, 1], [...c.fg, 1], 4, 1.2);
    });
    this.paintHighlight();
    this.r.update(this.frames, this.edgeBatch, this.nodeBatch, this.portBatch, this.hiBatch);
  }

  drawEdge(batch, e, col, width) {
    const { items, lay } = this.built;
    const s = items[e.s], d = items[e.d];
    const x0 = lay.x[e.s] + s.w, y0 = lay.y[e.s] + (s.outY[e.sp] ?? s.h / 2);
    const x1 = lay.x[e.d], y1 = lay.y[e.d] + (d.inY[e.dp] ?? d.h / 2);
    const style = settings.get('schEdges');
    if (style === 'straight') {
      batch.line(x0, y0, x1, y1, col, width);
    } else if (style === 'orthogonal') {
      const forward = x1 > x0 + 10;
      const mx = forward ? x0 + Math.min(40, (x1 - x0) / 2) + ((e.s * 7 + e.sp * 3) % 5) * 3 : x0 + 18;
      if (forward) {
        batch.line(x0, y0, mx, y0, col, width);
        batch.line(mx, y0, mx, y1, col, width);
        batch.line(mx, y1, x1, y1, col, width);
      } else {
        const yb = Math.max(lay.y[e.s] + s.h, lay.y[e.d] + d.h) + 18 + (e.s % 4) * 4;
        batch.line(x0, y0, mx, y0, col, width);
        batch.line(mx, y0, mx, yb, col, width);
        batch.line(mx, yb, x1 - 18, yb, col, width);
        batch.line(x1 - 18, yb, x1 - 18, y1, col, width);
        batch.line(x1 - 18, y1, x1, y1, col, width);
      }
    } else if (x1 > x0 + 10) {
      const dx = Math.max(30, (x1 - x0) * 0.45);
      batch.bezier(x0, y0, x0 + dx, y0, x1 - dx, y1, x1, y1, col, width, 0, 14);
    } else {
      // A feedback wire (into a register, usually): around the nodes.
      const below = Math.max(lay.y[e.s] + s.h, lay.y[e.d] + d.h) + 16 + ((e.s * 5 + e.sp) % 4) * 5;
      const r = 16;
      roundedPath(batch, [[x0, y0], [x0 + r, y0], [x0 + r, below], [x1 - r, below], [x1 - r, y1], [x1, y1]], col, width, 7);
    }
  }

  paintHighlight() {
    const hb = this.hiBatch.inst;
    hb.clear();
    if (!this.built) return;
    const c = this.c;
    for (const [i, alpha] of [[this.selected, 1], [this.hover, 0.8]]) {
      if (i < 0) continue;
      for (const e of this.built.edges) {
        if (e.s !== i && e.d !== i) continue;
        const col = e.s === i ? rgba(c.accent, alpha) : rgba(mix(c.accent, c.fg, 0.35), alpha);
        this.drawEdge(hb, e, col, e.w > 1 ? 3 : 2);
      }
    }
  }

  overlay(ctx) {
    if (!this.built) return;
    const { items, edges, lay } = this.built;
    const r = this.r;
    const s = r.cam.scale;
    const v = r.viewRect();
    const c = this.c;
    const fg = `rgb(${c.fg.map((x) => x * 255).join(',')})`;
    const muted = `rgb(${c.muted.map((x) => x * 255).join(',')})`;
    ctx.textBaseline = 'middle';
    // Instance frame labels.
    if (s > 0.12) {
      ctx.font = FONT_UI;
      for (const [path, b] of this.boxes) {
        const d = path.split('.').length;
        const pad = 16 + 6 * Math.max(0, 4 - d);
        const [sx, sy] = r.toScreen(b.x0 - pad + 8, b.y0 - pad - 8);
        if (sx > r.w || sy > r.h) continue;
        ctx.fillStyle = `rgb(${mix(c.fg, c.accent, 0.5).map((x) => x * 255).join(',')})`;
        ctx.fillText(`▾ ${path}`, sx, sy);
      }
    }
    if (s < 0.22) return;
    for (let i = 0; i < items.length; i++) {
      const it = items[i];
      const x = lay.x[i], y = lay.y[i];
      if (x > v.x1 || y > v.y1 || x + it.w < v.x0 || y + it.h < v.y0) continue;
      const [sx, sy] = r.toScreen(x, y);
      const w = it.w * s, hh = it.h * s;
      ctx.fillStyle = fg;
      ctx.textAlign = 'center';
      const big = !['op', 'and', 'in', 'out', 'const', 'mux', 'pmux', 'shiftx', 'dff', 'ff', 'latch'].includes(it.k);
      if (big) {
        ctx.font = FONT_UI;
        const label = it.k === 'group' ? `▸ ${it.label}` : it.label;
        ctx.fillText(clip(ctx, label, w - 10), sx + w / 2, sy + (HEADER / 2) * s);
      } else if (s > 0.35) {
        ctx.font = it.k === 'op' || it.k === 'and' ? FONT_UI : FONT;
        ctx.fillText(clip(ctx, it.label, w - 6), sx + w / 2, sy + hh / 2 + (it.k === 'dff' || it.k === 'ff' ? -5 * s : 0));
        if ((it.k === 'dff' || it.k === 'ff') && s > 0.6) {
          ctx.font = FONT;
          ctx.fillStyle = muted;
          ctx.fillText(clip(ctx, shortName(it.name, it.path), w - 6), sx + w / 2, sy + hh / 2 + 8 * s);
        }
      }
      const konst = it.ins.some((p) => p.konst);
      if (s > 0.75 && ((settings.get('schPortNames') && (big || it.k === 'dff' || it.k === 'ff')) || konst)) {
        ctx.font = FONT;
        ctx.fillStyle = muted;
        ctx.textAlign = 'left';
        it.ins.forEach((p, k) => {
          if (!big && it.k !== 'dff' && it.k !== 'ff' && !p.konst) return;
          ctx.fillText(clip(ctx, p.name, big ? w / 2 - 8 : w - 10), sx + 6, sy + it.inY[k] * s);
        });
        ctx.textAlign = 'right';
        it.outs.forEach((p, k) => ctx.fillText(clip(ctx, p.name, w / 2 - 8), sx + w - 6, sy + it.outY[k] * s));
      }
      if (it.k === 'group' && s > 0.5) {
        ctx.font = FONT;
        ctx.fillStyle = muted;
        ctx.textAlign = 'center';
        ctx.fillText(`${it.members.length} cells`, sx + w / 2, sy + hh - 8 * s);
      }
    }
    // Signal names on wires of the selection, or everywhere when close.
    if (settings.get('schNetLabels') && s > 0.6) {
      ctx.font = FONT;
      ctx.textAlign = 'left';
      const all = s > 1.5;
      const seen = new Set();
      for (const e of edges) {
        if (!e.n) continue;
        const focus = e.s === this.selected || e.d === this.selected || e.s === this.hover || e.d === this.hover;
        if (!all && !focus) continue;
        const key = `${e.s}:${e.sp}`;
        if (seen.has(key)) continue;
        seen.add(key);
        const sItem = items[e.s];
        const x0 = lay.x[e.s] + sItem.w, y0 = lay.y[e.s] + (sItem.outY[e.sp] ?? sItem.h / 2);
        if (x0 > v.x1 || x0 < v.x0 - 200 || y0 < v.y0 || y0 > v.y1) continue;
        const [sx, sy] = r.toScreen(x0, y0);
        const label = e.w > 1 ? `${e.n} [${e.w}]` : e.n;
        ctx.fillStyle = focus ? `rgb(${c.accent.map((x) => x * 255).join(',')})` : muted;
        ctx.fillText(clip(ctx, label, 160), sx + 6, sy - 7);
      }
    }
  }

  pick(x, y) {
    if (!this.built) return -1;
    const { items, lay } = this.built;
    const k = `${Math.floor(x / this.grid.cell)},${Math.floor(y / this.grid.cell)}`;
    for (const i of this.grid.map.get(k) || []) {
      if (x >= lay.x[i] - 3 && x <= lay.x[i] + items[i].w + 3 && y >= lay.y[i] && y <= lay.y[i] + items[i].h) return i;
    }
    return -1;
  }

  pickFrame(x, y) {
    for (const [path, b] of [...this.boxes].reverse()) {
      const d = path.split('.').length;
      const pad = 16 + 6 * Math.max(0, 4 - d);
      if (x >= b.x0 - pad && x <= b.x1 + pad && y >= b.y0 - pad - 16 && y <= b.y0 - pad + 4) return path;
    }
    return null;
  }

  setHover(i) {
    if (i === this.hover) return;
    this.hover = i;
    this.paint();
  }

  onHover(x, y, e) {
    const i = this.pick(x, y);
    this.setHover(i);
    const frame = i < 0 ? this.pickFrame(x, y) : null;
    this.r.canvas.style.cursor = i >= 0 || frame ? 'pointer' : '';
    if (i < 0) {
      if (frame) showTooltip(`<b>${esc(frame)}</b><div class="t">double-click to collapse</div>`, e.clientX, e.clientY);
      else hideTooltip();
      return;
    }
    const it = this.built.items[i];
    const title = it.k === 'group' ? `instance ${esc(it.groupPath)}` : `${esc(it.t || it.k)} <span class="t">${esc(it.name || '')}</span>`;
    const lines = (it.info || []).slice(0, 6).map(([k, val]) => `<div class="t">${esc(k)}: <code>${esc(val)}</code></div>`).join('');
    const hint = it.k === 'group' ? '<div class="t">double-click to expand</div>' : '';
    showTooltip(`<div>${title}</div>${lines}${hint}`, e.clientX, e.clientY);
  }

  onDouble(x, y) {
    const i = this.pick(x, y);
    if (i >= 0 && this.built.items[i].k === 'group') {
      this.toggle(this.built.items[i].groupPath);
      return;
    }
    const f = this.pickFrame(x, y);
    if (f) this.toggle(f);
  }

  select(i) {
    this.selected = i;
    this.paint();
    this.renderInspector();
  }

  renderInspector() {
    const ins = this.inspector;
    if (this.selected < 0 || !this.built) {
      ins.hidden = true;
      return;
    }
    const { items, edges, g } = this.built;
    const it = items[this.selected];
    const close = h('button', { class: 'close', title: 'Close', onclick: () => this.select(-1) }, icon('x'));
    const goto = (j) => h('span', { class: 'chip', onclick: () => this.focusItem(j) }, items[j].k === 'group' ? `▸ ${items[j].groupPath}` : shortName(items[j].name || items[j].label, ''));
    const conns = (dir) => {
      const list = [];
      for (const e of edges) {
        if (dir === 'in' && e.d === this.selected) list.push([it.ins[e.dp]?.name || '', e.s, e.n, e.w]);
        if (dir === 'out' && e.s === this.selected) list.push([it.outs[e.sp]?.name || '', e.d, e.n, e.w]);
      }
      return list;
    };
    const section = (title, list) => list.length
      ? [h('h4', {}, title), h('div', { class: 'kv' }, list.flatMap(([port, j, n, w]) => [h('span', {}, port || n || '·'), h('span', {}, goto(j), n && n !== port ? h('span', { class: 'muted' }, ` ${n}${w > 1 ? ` [${w}]` : ''}`) : null)]))]
      : [];
    if (it.k === 'group') {
      const kinds = new Map();
      for (const m of it.members) kinds.set(g.nodes[m].t, (kinds.get(g.nodes[m].t) || 0) + 1);
      fill(ins, 
        h('h3', {}, `Instance ${it.label}`, close),
        h('div', { class: 'sub' }, it.groupPath),
        h('button', { onclick: () => this.toggle(it.groupPath) }, icon('plus'), 'Expand'),
        h('h4', {}, `${it.members.length} cells`),
        h('div', { class: 'kv' }, [...kinds.entries()].sort((a, b) => b[1] - a[1]).flatMap(([k, n]) => [h('span', {}, k), h('span', {}, n)])),
        ...section('Inputs', conns('in')),
        ...section('Outputs', conns('out')),
      );
    } else {
      fill(ins, 
        h('h3', {}, it.t || it.k, close),
        h('div', { class: 'sub' }, it.name),
        it.path ? h('div', { class: 'kv' }, h('span', {}, 'instance'), h('span', {}, h('span', { class: 'chip', onclick: () => this.toggle(it.path) }, it.path))) : null,
        it.info && it.info.length ? [h('h4', {}, 'Properties'), h('div', { class: 'kv' }, it.info.flatMap(([k, val]) => [h('span', {}, k), h('span', {}, val)]))] : null,
        it.t === 'SB_LUT4' || it.t === 'ICESTORM_LC' ? lutTable(it.info) : null,
        ...section('Inputs', conns('in')),
        ...section('Outputs', conns('out')),
      );
    }
    ins.hidden = false;
  }

  focusItem(i) {
    const { items, lay } = this.built;
    this.select(i);
    this.r.panTo(lay.x[i] + items[i].w / 2, lay.y[i] + items[i].h / 2, Math.max(this.r.cam.scale, 0.9));
  }

  find(q) {
    if (!this.stage || !q) return;
    q = q.toLowerCase();
    const g = this.stage.graph;
    const hit = g.nodes.findIndex((n) => (n.name || '').toLowerCase().includes(q) || (n.label || '').toLowerCase() === q);
    let node = hit;
    if (node < 0) {
      const e = g.edges.find((x) => (x.n || '').toLowerCase().includes(q));
      if (e) node = e.s;
    }
    if (node < 0) {
      toast(`Nothing matches “${q}”`);
      return;
    }
    // Expand the instances around it.
    for (const a of ancestors(g.nodes[node].path)) this.collapsed.delete(a);
    this.rebuild(false);
    this.focusItem(this.built.itemOf[node]);
  }

  legendUpdate() {
    const items = [['Logic', 'colLogic'], ['Registers', 'colReg'], ['Muxes', 'colMux'], ['Memory', 'colMem'], ['Instances', 'colInst'], ['Ports', 'colPort']];
    const n = this.built ? this.built.items.length : 0;
    const total = this.stage ? this.stage.graph.nodes.length : 0;
    fill(this.legend, 
      ...items.map(([l, k]) => h('span', {}, h('i', { style: { background: settings.get(k) } }), l)),
      h('span', { class: 'muted' }, `${n} shown · ${total} cells`),
    );
  }

  fit() {
    if (!this.built || !this.built.items.length) return;
    const { items, lay } = this.built;
    let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    items.forEach((it, i) => {
      x0 = Math.min(x0, lay.x[i]);
      y0 = Math.min(y0, lay.y[i]);
      x1 = Math.max(x1, lay.x[i] + it.w);
      y1 = Math.max(y1, lay.y[i] + it.h);
    });
    this.r.fit(x0 - 30, y0 - 50, x1 + 30, y1 + 30, 30, 1.5);
  }

  visible() {
    if (this.r) this.r.resize();
  }
}

/** A polyline with rounded corners. */
function roundedPath(batch, pts, col, width, rad) {
  let [px, py] = pts[0];
  for (let i = 1; i < pts.length; i++) {
    const [x, y] = pts[i];
    if (i === pts.length - 1) {
      batch.line(px, py, x, y, col, width);
      break;
    }
    const [nx, ny] = pts[i + 1];
    const l1 = Math.hypot(x - px, y - py), l2 = Math.hypot(nx - x, ny - y);
    const k = Math.min(rad, l1 / 2, l2 / 2);
    const ax = x - ((x - px) / (l1 || 1)) * k, ay = y - ((y - py) / (l1 || 1)) * k;
    const bx = x + ((nx - x) / (l2 || 1)) * k, by = y + ((ny - y) / (l2 || 1)) * k;
    batch.line(px, py, ax, ay, col, width);
    batch.bezier(ax, ay, x, y, x, y, bx, by, col, width, 0, 5);
    [px, py] = [bx, by];
  }
}

function clip(ctx, s, max) {
  if (max <= 0) return '';
  if (ctx.measureText(s).width <= max) return s;
  let lo = 0, hi = s.length;
  while (lo < hi) {
    const m = (lo + hi + 1) >> 1;
    if (ctx.measureText(`${s.slice(0, m)}…`).width <= max) lo = m;
    else hi = m - 1;
  }
  return lo > 0 ? `${s.slice(0, lo)}…` : '';
}

/** A LUT's truth table, from its LUT_INIT property. */
export function lutTable(info) {
  const init = (info || []).find(([k]) => k === 'LUT_INIT' || k === 'LUT');
  if (!init) return null;
  const m = /'h([0-9a-f]+)/i.exec(init[1]);
  if (!m) return null;
  const v = parseInt(m[1], 16);
  const rows = [];
  for (let i = 0; i < 16; i++) {
    rows.push(h('tr', {}, [3, 2, 1, 0].map((b) => h('td', {}, (i >> b) & 1)), h('td', { class: (v >> i) & 1 ? 'one' : '' }, (v >> i) & 1)));
  }
  return [
    h('h4', {}, 'Truth table'),
    h('table', { class: 'truth' }, h('tr', {}, ['I3', 'I2', 'I1', 'I0', 'O'].map((x) => h('th', {}, x))), rows),
  ];
}
