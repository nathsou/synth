// The chip view: the iCE40 die as tiles, the logic cells, IOs and RAMs
// each placed cell occupies with its configuration, and every routed net
// over the wires it uses.

import { Renderer, rgba, zoomBar } from './gl.js';
import { settings } from './settings.js';
import { h, fill, icon, rgb, cssVar, mix, hsl, hashHue, heat, esc, showTooltip, hideTooltip } from './util.js';
import { lutTable } from './schematic.js';

const T = 100; // world units per tile
const FONT = '11px ui-monospace, "JetBrains Mono", Menlo, Consolas, monospace';

// Positions inside a tile, as fractions of its size.
const LC_X0 = 0.6, LC_X1 = 0.95, LC_Y0 = 0.06, LC_DY = 0.112, LC_H = 0.094;
const H4_Y0 = 0.035, H4_Y1 = 0.25, H12_Y0 = 0.265, H12_Y1 = 0.36;
const V4_X0 = 0.035, V4_X1 = 0.25, V12_X0 = 0.265, V12_X1 = 0.36;
const LOC_X = 0.47, LOC_Y0 = 0.42, LOC_Y1 = 0.95;

function trackOf(wire) {
  const m = /(\d+)$/.exec(wire);
  return m ? Number(m[1]) : 0;
}

/** Where a wire runs: 'h'/'v' spans with a lane, or a point in the tile. */
function wireShape(wire, cls) {
  if (/^(sp4_h|span4_horz)/.test(wire)) return { o: 'h', lane: H4_Y0 + (trackOf(wire) % 48) / 48 * (H4_Y1 - H4_Y0) };
  if (/^(sp12_h|span12_horz)/.test(wire)) return { o: 'h', lane: H12_Y0 + (trackOf(wire) % 24) / 24 * (H12_Y1 - H12_Y0) };
  if (/^(sp4_v|sp4_r_v|span4_vert)/.test(wire)) return { o: 'v', lane: V4_X0 + (trackOf(wire) % 48) / 48 * (V4_X1 - V4_X0) };
  if (/^(sp12_v|span12_vert)/.test(wire)) return { o: 'v', lane: V12_X0 + (trackOf(wire) % 24) / 24 * (V12_X1 - V12_X0) };
  let m = /^local_g(\d)_(\d)$/.exec(wire);
  if (m) return { o: 'p', px: LOC_X, py: LOC_Y0 + (Number(m[1]) * 8 + Number(m[2]) + 0.5) / 32 * (LOC_Y1 - LOC_Y0) };
  m = /^lutff_(\d)\/in_(\d)$/.exec(wire);
  if (m) return { o: 'p', px: LC_X0, py: LC_Y0 + Number(m[1]) * LC_DY + (Number(m[2]) + 0.5) / 4 * LC_H };
  m = /^lutff_(\d)\/(out|lout)$/.exec(wire);
  if (m) return { o: 'p', px: LC_X1, py: LC_Y0 + Number(m[1]) * LC_DY + LC_H / 2 };
  m = /^lutff_(\d)\/(cout|cin)/.exec(wire);
  if (m) return { o: 'p', px: LC_X0 + 0.02, py: LC_Y0 + Number(m[1]) * LC_DY + (m[2] === 'cout' ? LC_H : 0) };
  m = /^lutff_(global|\d)\/(clk|cen|s_r)/.exec(wire);
  if (m) return { o: 'p', px: LC_X0 + 0.15, py: 0.985 };
  m = /^glb2local_(\d)/.exec(wire);
  if (m) return { o: 'p', px: 0.4, py: LOC_Y0 + Number(m[1]) * 0.03 };
  if (cls === 'Global') return { o: 'g' };
  m = /^io_(\d)\/(D_IN|D_OUT|OUT_ENB)/.exec(wire);
  if (m) return { o: 'p', px: 0.5, py: 0.5 + (Number(m[1]) - 0.5) * 0.4 };
  return { o: 'p', px: 0.5, py: 0.55 };
}

export class ChipView {
  constructor(el, app) {
    this.el = el;
    this.app = app;
    this.data = null;
    this.sel = null; // {type: 'net'|'cell'|'tile', i}
    this.hoverNet = -1;
    this.crit = null; // clock index of the critical path overlay
    // The step shown: 'place' (the cells on their sites) or 'route' (with
    // the routed wires).
    this.step = 'route';
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
    el.tabIndex = 0;
    el.addEventListener('keydown', (e) => { if (e.key === 'f') this.fit(); if (e.key === 'Escape') this.select(null); });
    this.tileBatch = this.r.batch('rect');
    this.wireBatch = this.r.batch('line');
    this.cellBatch = this.r.batch('rect');
    this.hiBatch = this.r.batch('line');
    this.hiCells = this.r.batch('rect');
    this.r.onOverlay = (ctx) => this.overlay(ctx);
    this.r.interact({
      onHover: (x, y, e) => this.onHover(x, y, e),
      onClick: (x, y) => this.onClick(x, y),
      onLeave: () => { hideTooltip(); this.setHoverNet(-1); },
    });
    settings.on('change', (k) => {
      if (k.startsWith('chip') || k.startsWith('colTile') || ['colLc', 'colFf', 'colCarry', 'colIo', 'colRam', 'colSpan4', 'colSpan12', 'colLocal', 'colGlobal', 'theme', 'accent'].includes(k)) {
        this.renderToolbar();
        this.paint();
      }
    });
    this.renderToolbar();
  }

  show(result) {
    const first = !this.data;
    this.data = result.chip || null;
    this.timing = result.timing || null;
    this.sel = null;
    this.crit = null;
    this.inspector.hidden = true;
    if (!this.data || !this.r) return;
    this.prepare();
    this.renderToolbar();
    this.paint();
    if (first || this.lastDie !== this.data.die) this.fit();
    this.lastDie = this.data.die;
  }

  /** Geometry of every wire and pip, and lookup tables. */
  prepare() {
    const d = this.data;
    this.gh = d.gh;
    this.cellsAt = new Map();
    d.cells.forEach((c, i) => {
      const k = `${c.x},${c.y}`;
      if (!this.cellsAt.has(k)) this.cellsAt.set(k, []);
      this.cellsAt.get(k).push(i);
    });
    this.pins = new Map(d.pins.map(([name, x, y, z]) => [`${x},${y},${z}`, name]));
    this.nodeShapes = d.nodes.map(([w, cls]) => wireShape(w, cls));
    // Segments per net, in world coordinates: [x0, y0, x1, y1, class].
    this.segs = d.nets.map((net) => {
      const out = [];
      const spans = new Set();
      for (const [a, b, px, py] of net.pips) {
        for (const nd of [a, b]) {
          if (spans.has(nd)) continue;
          const sh = this.nodeShapes[nd];
          if (sh.o === 'h' || sh.o === 'v') {
            spans.add(nd);
            const [, cls, , , x0, y0, x1, y1] = d.nodes[nd];
            if (sh.o === 'h') {
              const wy = this.ty(y0) + sh.lane * T;
              out.push([x0 * T, wy, (x1 + 1) * T, wy, cls]);
            } else {
              const wx = x0 * T + sh.lane * T;
              out.push([wx, this.ty(y1), wx, this.ty(y0) + T, cls]);
            }
          }
        }
        const pa = this.anchor(a, px, py), pb = this.anchor(b, px, py);
        if (pa && pb) out.push([pa[0], pa[1], pb[0], pb[1], 'pip']);
      }
      return out;
    });
    // Spatial index of segments for picking.
    const cell = T;
    const grid = new Map();
    this.segs.forEach((list, ni) => list.forEach((s, si) => {
      const gx0 = Math.floor(Math.min(s[0], s[2]) / cell), gx1 = Math.floor(Math.max(s[0], s[2]) / cell);
      const gy0 = Math.floor(Math.min(s[1], s[3]) / cell), gy1 = Math.floor(Math.max(s[1], s[3]) / cell);
      for (let gx = gx0; gx <= gx1; gx++) for (let gy = gy0; gy <= gy1; gy++) {
        const k = gx * 4096 + gy;
        if (!grid.has(k)) grid.set(k, []);
        grid.get(k).push(ni, si);
      }
    }));
    this.grid = grid;
    this.usedTiles = new Set(d.cells.map((c) => `${c.x},${c.y}`));
    for (const net of d.nets) for (const [, , px, py] of net.pips) this.usedTiles.add(`${px},${py}`);
  }

  /** World y of the top of tile row `y` (row 0 is at the bottom of the die). */
  ty(y) { return (this.gh - 1 - y) * T; }

  anchor(node, px, py) {
    const sh = this.nodeShapes[node];
    const tx = px * T, ty = this.ty(py);
    if (sh.o === 'h') return [tx + 0.42 * T, ty + sh.lane * T];
    if (sh.o === 'v') return [tx + sh.lane * T, ty + 0.42 * T];
    if (sh.o === 'g') return null;
    return [tx + sh.px * T, ty + sh.py * T];
  }

  colors() {
    return {
      fg: rgb(cssVar('--fg')), muted: rgb(cssVar('--muted')), bg: rgb(cssVar('--bg')),
      accent: rgb(settings.get('accent')), dark: settings.get('theme') === 'dark',
      logic: rgb(settings.get('colTileLogic')), ram: rgb(settings.get('colTileRam')), io: rgb(settings.get('colTileIo')),
      lc: rgb(settings.get('colLc')), ff: rgb(settings.get('colFf')), carry: rgb(settings.get('colCarry')),
      iocell: rgb(settings.get('colIo')), bram: rgb(settings.get('colRam')),
      span4: rgb(settings.get('colSpan4')), span12: rgb(settings.get('colSpan12')), local: rgb(settings.get('colLocal')), global: rgb(settings.get('colGlobal')),
    };
  }

  tileColor(kind, c) {
    const base = kind === 1 ? c.io : kind >= 3 ? c.ram : c.logic;
    return c.dark ? base : mix(base, [1, 1, 1], 0.78);
  }

  netColor(ni, cls, c) {
    const mode = settings.get('chipColorBy');
    const net = this.data.nets[ni];
    if (mode === 'crit') return net.crit !== undefined ? heat(net.crit) : mix(c.muted, c.bg, 0.3);
    if (mode === 'class') {
      if (/Sp12|IoSpan12/.test(cls)) return c.span12;
      if (/Sp4|IoSpan4/.test(cls)) return c.span4;
      if (cls === 'pip') return mix(c.local, c.fg, 0.2);
      if (/Global/.test(cls)) return c.global;
      return c.local;
    }
    return hsl(hashHue(net.name), 0.75, c.dark ? 0.62 : 0.45);
  }

  paint() {
    if (!this.data || !this.r) return;
    const d = this.data;
    const c = this.colors();
    this.c = c;
    this.r.background = [...c.bg, 1];
    const tb = this.tileBatch.inst;
    tb.clear();
    const dim = settings.get('chipUnusedDim');
    for (let y = 0; y < d.gh; y++) {
      for (let x = 0; x < d.gw; x++) {
        const k = d.tiles[y * d.gw + x];
        if (k === 0) continue;
        const used = this.usedTiles.has(`${x},${y}`);
        let col = this.tileColor(k, c);
        if (dim && !used) col = mix(col, c.bg, 0.45);
        tb.rect(x * T + 1.5, this.ty(y) + 1.5, T - 3, T - 3, [...col, 1], [...mix(col, c.fg, 0.12), 1], 4, 1);
        if (k === 2) {
          // Empty logic cell slots.
          for (let z = 0; z < 8; z++) {
            tb.rect(x * T + LC_X0 * T, this.ty(y) + (LC_Y0 + z * LC_DY) * T, (LC_X1 - LC_X0) * T, LC_H * T, [...mix(col, c.fg, 0.06), 1], null, 1.5, 0);
          }
        }
      }
    }
    // Wires.
    const wb = this.wireBatch.inst;
    wb.clear();
    if (this.routing()) {
      const alpha = settings.get('chipWireAlpha');
      const globals = settings.get('chipGlobals');
      this.segs.forEach((list, ni) => {
        if (d.nets[ni].global && !globals) return;
        for (const s of list) {
          const col = this.netColor(ni, s[4], c);
          wb.line(s[0], s[1], s[2], s[3], rgba(col, alpha), s[4] === 'pip' ? 1 : 1.3, s[4] === 'pip' ? 0.5 : 0.7);
        }
      });
    }
    // Cells.
    const cb = this.cellBatch.inst;
    cb.clear();
    d.cells.forEach((cell) => {
      const x = cell.x * T, y = this.ty(cell.y);
      if (cell.bel === 'lc') {
        const info = Object.fromEntries(cell.info);
        const sy = y + (LC_Y0 + cell.z * LC_DY) * T;
        const w = (LC_X1 - LC_X0) * T;
        if (info.LUT) cb.rect(x + LC_X0 * T, sy, w * 0.62, LC_H * T, [...c.lc, 1], null, 1.5, 0);
        if (info['flip-flop']) cb.rect(x + LC_X0 * T + w * 0.66, sy, w * 0.34, LC_H * T, [...c.ff, 1], null, 1.5, 0);
        if (info.carry) cb.rect(x + LC_X0 * T - 2.5, sy, 2, LC_H * T, [...c.carry, 1], null, 1, 0);
      } else if (cell.bel === 'io') {
        const [ix, iy, iw, ih] = this.ioRect(cell.x, cell.y, cell.z);
        cb.rect(ix, iy, iw, ih, [...c.iocell, 1], null, 3, 0);
      } else if (cell.bel === 'ram') {
        cb.rect(x + 0.5 * T, this.ty(cell.y + 1) + 0.12 * T, 0.45 * T, 1.76 * T, [...c.bram, 1], null, 4, 0);
      } else if (cell.bel === 'gb') {
        cb.rect(x + 0.4 * T, y + 0.4 * T, 0.2 * T, 0.2 * T, [...c.global, 1], null, 10, 0);
      } else if (cell.bel === 'pll') {
        const [px, py, pw, ph] = this.cellRect(cell);
        cb.rect(px, py, pw, ph, [...c.global, 1], null, 4, 0);
      }
    });
    this.paintHighlight();
    this.r.update(this.tileBatch, this.wireBatch, this.cellBatch, this.hiBatch, this.hiCells);
    this.legendUpdate();
  }

  ioRect(x, y, z) {
    const d = this.data;
    const tx = x * T, ty = this.ty(y);
    if (y === 0 || y === d.gh - 1) {
      const top = y === d.gh - 1;
      return [tx + (0.12 + z * 0.44) * T, ty + (top ? 0.06 : 0.62) * T, 0.32 * T, 0.32 * T];
    }
    const left = x === 0;
    return [tx + (left ? 0.06 : 0.62) * T, ty + (0.12 + z * 0.44) * T, 0.32 * T, 0.32 * T];
  }

  paintHighlight() {
    const hb = this.hiBatch.inst, hc = this.hiCells.inst;
    hb.clear();
    hc.clear();
    if (!this.data) return;
    const c = this.c;
    const netHi = (ni, col, w) => {
      for (const s of this.segs[ni]) hb.line(s[0], s[1], s[2], s[3], col, w, w * 0.6);
    };
    const cellHi = (ci, col) => {
      const cell = this.data.cells[ci];
      const [x, y, w, hh] = this.cellRect(cell);
      hc.rect(x - 2, y - 2, w + 4, hh + 4, [0, 0, 0, 0], col, 3, 2);
    };
    if (this.crit !== null && this.timing) {
      const clk = this.timing.clocks[this.crit];
      for (const p of clk?.path || []) {
        if (p.net !== undefined) netHi(p.net, [1, 0.35, 0.35, 0.95], 2.4);
        if (p.ci !== undefined) cellHi(p.ci, [1, 0.35, 0.35, 1]);
      }
    }
    if (this.hoverNet >= 0) netHi(this.hoverNet, rgba(mix(c.accent, c.fg, 0.3), 0.85), 2.2);
    if (this.sel?.type === 'net') {
      netHi(this.sel.i, [...c.accent, 1], 3);
      const net = this.data.nets[this.sel.i];
      if (net.driver !== undefined) cellHi(net.driver, [...c.accent, 1]);
      for (const s of net.sinks || []) cellHi(s, rgba(mix(c.accent, c.fg, 0.4), 1));
    }
    if (this.sel?.type === 'cell') {
      cellHi(this.sel.i, [...c.accent, 1]);
      for (const p of this.data.cells[this.sel.i].pins) if (p[1] !== null && !this.data.nets[p[1]]?.global) netHi(p[1], rgba(c.accent, 0.8), 2);
    }
    if (this.sel?.type === 'tile') {
      const [x, y] = this.sel.xy;
      hc.rect(x * T + 1, this.ty(y) + 1, T - 2, T - 2, [0, 0, 0, 0], [...c.accent, 1], 4, 2);
    }
  }

  cellRect(cell) {
    const x = cell.x * T, y = this.ty(cell.y);
    if (cell.bel === 'lc') return [x + LC_X0 * T, y + (LC_Y0 + cell.z * LC_DY) * T, (LC_X1 - LC_X0) * T, LC_H * T];
    if (cell.bel === 'io') return this.ioRect(cell.x, cell.y, cell.z);
    if (cell.bel === 'ram') return [x + 0.5 * T, this.ty(cell.y + 1) + 0.12 * T, 0.45 * T, 1.76 * T];
    // A PLL: a block over its IO tile.
    if (cell.bel === 'pll') return [x + 0.1 * T, y + 0.25 * T, 0.8 * T, 0.5 * T];
    return [x + 0.4 * T, y + 0.4 * T, 0.2 * T, 0.2 * T];
  }

  overlay(ctx) {
    if (!this.data) return;
    const r = this.r, s = r.cam.scale, d = this.data, c = this.c;
    const v = r.viewRect();
    const css = (col) => `rgb(${col.map((x) => Math.round(x * 255)).join(',')})`;
    ctx.font = FONT;
    ctx.textBaseline = 'middle';
    // Package pins around the IO ring.
    if (settings.get('chipPins') && s > 0.5) {
      ctx.fillStyle = css(c.muted);
      for (const [name, x, y, z] of d.pins) {
        const [ix, iy, iw, ih] = this.ioRect(x, y, z);
        if (ix > v.x1 || iy > v.y1 || ix + iw < v.x0 || iy + ih < v.y0) continue;
        const [sx, sy] = r.toScreen(ix + iw / 2, iy + ih / 2);
        ctx.textAlign = 'center';
        const off = 0.42 * T * s;
        const pos = y === 0 ? [sx, sy + off] : y === d.gh - 1 ? [sx, sy - off] : x === 0 ? [sx - off, sy] : [sx + off, sy];
        ctx.fillText(name, pos[0], pos[1]);
      }
    }
    if (s > 0.55) {
      ctx.textAlign = 'left';
      ctx.fillStyle = css(mix(c.muted, c.bg, 0.2));
      for (let y = 0; y < d.gh; y++) for (let x = 0; x < d.gw; x++) {
        if (d.tiles[y * d.gw + x] === 0) continue;
        const wx = x * T, wy = this.ty(y);
        if (wx > v.x1 || wy > v.y1 || wx + T < v.x0 || wy + T < v.y0) continue;
        const [sx, sy] = r.toScreen(wx + 0.05 * T, wy + 0.97 * T);
        ctx.fillText(`${x},${y}`, sx, sy - 6);
      }
    }
    if (s > 2.2) {
      ctx.textAlign = 'left';
      d.cells.forEach((cell) => {
        if (cell.bel !== 'lc') return;
        const [x, y, , hh] = this.cellRect(cell);
        if (x > v.x1 || y > v.y1 || x + T < v.x0 || y + hh < v.y0) return;
        ctx.fillStyle = css(c.fg);
        const [sx, sy] = r.toScreen(x - 0.24 * T, y + hh / 2);
        ctx.fillText(clipText(ctx, cell.name.replace(/\$lc$/, ''), 0.22 * T * s), sx, sy);
        // The LUT's truth table, inside the cell.
        const lut = cell.info.find(([k]) => k === 'LUT');
        if (lut && s > 4.5) {
          const [lx, ly] = r.toScreen(x + 2, y + hh / 2);
          ctx.fillStyle = '#fff';
          ctx.fillText(lut[1].replace("16'h", ''), lx, ly);
        }
      });
    }
    // Tile kinds when zoomed out a little.
    if (s < 0.25 && s > 0.06) {
      ctx.fillStyle = css(c.muted);
      ctx.textAlign = 'center';
      const [sx, sy] = r.toScreen(d.gw * T / 2, -14);
      ctx.fillText(`iCE40 ${d.part.toUpperCase()} · ${d.package} · ${d.gw - 2}×${d.gh - 2} tiles`, sx, sy);
    }
  }

  pickNet(x, y) {
    if (!this.data || !this.routing()) return -1;
    const k = Math.floor(x / T) * 4096 + Math.floor(y / T);
    const tol = 5 / this.r.cam.scale;
    let best = -1, bd = tol;
    const list = this.grid.get(k) || [];
    const globals = settings.get('chipGlobals');
    for (let i = 0; i < list.length; i += 2) {
      const ni = list[i];
      if (this.data.nets[ni].global && !globals) continue;
      const s = this.segs[ni][list[i + 1]];
      const dd = distSeg(x, y, s[0], s[1], s[2], s[3]);
      if (dd < bd) { bd = dd; best = ni; }
    }
    return best;
  }

  pickCell(x, y) {
    if (!this.data) return -1;
    const tx = Math.floor(x / T), tyy = this.gh - 1 - Math.floor(y / T);
    for (const yy of [tyy, tyy - 1]) {
      for (const ci of this.cellsAt.get(`${tx},${yy}`) || []) {
        const [cx, cy, w, hh] = this.cellRect(this.data.cells[ci]);
        if (x >= cx - 2 && x <= cx + w + 2 && y >= cy - 1 && y <= cy + hh + 1) return ci;
      }
    }
    return -1;
  }

  tileAt(x, y) {
    const tx = Math.floor(x / T), tyy = this.gh - 1 - Math.floor(y / T);
    if (tx < 0 || tyy < 0 || tx >= this.data.gw || tyy >= this.data.gh) return null;
    const k = this.data.tiles[tyy * this.data.gw + tx];
    return k ? [tx, tyy, k] : null;
  }

  setHoverNet(i) {
    if (i === this.hoverNet) return;
    this.hoverNet = i;
    this.paintHighlight();
    this.r.update(this.hiBatch, this.hiCells);
  }

  onHover(x, y, e) {
    if (!this.data) return;
    const ci = this.pickCell(x, y);
    if (ci >= 0) {
      this.setHoverNet(-1);
      const cell = this.data.cells[ci];
      const info = cell.info.map(([k, val]) => `<div class="t">${esc(k)}: <code>${esc(val)}</code></div>`).join('');
      showTooltip(`<div><b>${esc(cell.t)}</b> <span class="t">${cell.x},${cell.y}/${cell.z}</span></div><div>${esc(cell.name)}</div>${cell.pin ? `<div class="t">pin ${esc(cell.pin)}</div>` : ''}${info}`, e.clientX, e.clientY);
      this.r.canvas.style.cursor = 'pointer';
      return;
    }
    const ni = this.pickNet(x, y);
    this.setHoverNet(ni);
    if (ni >= 0) {
      const net = this.data.nets[ni];
      const crit = net.crit !== undefined ? `<div class="t">criticality ${(net.crit * 100).toFixed(0)}%${net.slack !== undefined ? `, slack ${net.slack} ns` : ''}</div>` : '';
      showTooltip(`<div><b>${esc(net.name)}</b></div><div class="t">${net.pips.length} switches · ${(net.sinks || []).length} sinks</div>${crit}`, e.clientX, e.clientY);
      this.r.canvas.style.cursor = 'pointer';
      return;
    }
    this.r.canvas.style.cursor = '';
    const t = this.tileAt(x, y);
    if (t && this.r.cam.scale > 0.15) {
      const kinds = ['', 'IO', 'logic', 'RAM (bottom)', 'RAM (top)'];
      const n = (this.cellsAt.get(`${t[0]},${t[1]}`) || []).length;
      showTooltip(`<div><b>${kinds[t[2]]} tile</b> <span class="t">${t[0]},${t[1]}</span></div><div class="t">${n} cell${n === 1 ? '' : 's'} placed</div>`, e.clientX, e.clientY);
    } else hideTooltip();
  }

  onClick(x, y) {
    if (!this.data) return;
    const ci = this.pickCell(x, y);
    if (ci >= 0) return this.select({ type: 'cell', i: ci });
    const ni = this.pickNet(x, y);
    if (ni >= 0) return this.select({ type: 'net', i: ni });
    const t = this.tileAt(x, y);
    if (t) return this.select({ type: 'tile', xy: [t[0], t[1]], k: t[2] });
    this.select(null);
  }

  select(sel) {
    this.sel = sel;
    this.paintHighlight();
    this.r.update(this.hiBatch, this.hiCells);
    this.renderInspector();
  }

  selectNetByName(name) {
    const i = this.data?.nets.findIndex((n) => n.name === name) ?? -1;
    if (i >= 0) this.select({ type: 'net', i });
  }

  focusCell(ci) {
    const [x, y, w, hh] = this.cellRect(this.data.cells[ci]);
    this.select({ type: 'cell', i: ci });
    this.r.panTo(x + w / 2, y + hh / 2, Math.max(this.r.cam.scale, 1.6));
  }

  renderInspector() {
    const ins = this.inspector;
    const sel = this.sel;
    if (!sel || !this.data) {
      ins.hidden = true;
      return;
    }
    const d = this.data;
    const close = h('button', { class: 'close', title: 'Close', onclick: () => this.select(null) }, icon('x'));
    const netChip = (ni, label) => h('span', { class: 'chip', title: d.nets[ni]?.name, onclick: () => this.select({ type: 'net', i: ni }) }, label ?? d.nets[ni]?.name);
    const cellChip = (ci) => h('span', { class: 'chip', onclick: () => this.focusCell(ci) }, d.cells[ci].name);
    const bitsBtn = (x, y) => h('button', { onclick: () => this.app.showBits(x, y) }, icon('bits'), 'Configuration bits');
    if (sel.type === 'cell') {
      const cell = d.cells[sel.i];
      const pins = cell.pins.map(([pin, ni, out, name]) => [h('span', {}, `${out ? '→' : '←'} ${pin}`), ni !== null && ni !== undefined ? h('span', {}, netChip(ni)) : h('span', { class: 'muted' }, name)]);
      fill(ins, 
        h('h3', {}, cell.t, close),
        h('div', { class: 'sub' }, cell.name),
        h('div', { class: 'kv' },
          h('span', {}, 'location'), h('span', {}, `X${cell.x}/Y${cell.y}/${cell.bel}${cell.z}`),
          cell.pin ? [h('span', {}, 'package pin'), h('span', {}, cell.pin)] : null,
          cell.global !== undefined ? [h('span', {}, 'global network'), h('span', {}, cell.global)] : null,
          ...cell.info.flatMap(([k, val]) => [h('span', {}, k), h('span', {}, val)])),
        lutTable(cell.info),
        h('h4', {}, 'Pins'),
        h('div', { class: 'kv' }, pins.flat()),
        h('h4', {}, 'Bitstream'),
        bitsBtn(cell.x, cell.bel === 'ram' ? cell.y : cell.y),
      );
    } else if (sel.type === 'net') {
      const net = d.nets[sel.i];
      const classes = new Map();
      for (const [a, b] of net.pips) for (const n of [a, b]) classes.set(d.nodes[n][1], (classes.get(d.nodes[n][1]) || 0) + 1);
      fill(ins, 
        h('h3', {}, 'Net', close),
        h('div', { class: 'sub' }, net.name),
        h('div', { class: 'kv' },
          h('span', {}, 'switches'), h('span', {}, net.pips.length),
          net.crit !== undefined ? [h('span', {}, 'criticality'), h('span', {}, `${(net.crit * 100).toFixed(1)}%`)] : null,
          net.slack !== undefined ? [h('span', {}, 'slack'), h('span', {}, `${net.slack} ns`)] : null,
          net.global ? [h('span', {}, 'global'), h('span', {}, 'yes')] : null),
        h('h4', {}, 'Driver'),
        h('div', { class: 'chips' }, net.driver !== undefined ? cellChip(net.driver) : h('span', { class: 'muted' }, '—')),
        h('h4', {}, `Sinks (${(net.sinks || []).length})`),
        h('div', { class: 'chips' }, (net.sinks || []).slice(0, 60).map(cellChip)),
        h('h4', {}, 'Wires used'),
        h('div', { class: 'kv' }, [...classes.entries()].sort((a, b) => b[1] - a[1]).flatMap(([k, n]) => [h('span', {}, k), h('span', {}, n)])),
        h('h4', {}, 'Route'),
        h('div', { class: 'features', style: { maxHeight: '220px', overflow: 'auto' } },
          net.pips.slice(0, 400).map(([a, b, px, py]) => h('div', {}, h('span', { class: 'c' }, `${px},${py}`), h('span', { class: 'n' }, `${d.nodes[a][0]} → ${d.nodes[b][0]}`)))),
      );
    } else {
      const [x, y] = sel.xy;
      const kinds = ['', 'IO', 'Logic', 'RAM (bottom half)', 'RAM (top half)'];
      const here = this.cellsAt.get(`${x},${y}`) || [];
      const pips = d.nets.reduce((n, net) => n + net.pips.filter(([, , px, py]) => px === x && py === y).length, 0);
      fill(ins, 
        h('h3', {}, `${kinds[sel.k]} tile`, close),
        h('div', { class: 'sub' }, `X${x} Y${y}`),
        h('div', { class: 'kv' }, h('span', {}, 'cells'), h('span', {}, here.length), h('span', {}, 'switches on'), h('span', {}, pips)),
        here.length ? [h('h4', {}, 'Cells'), h('div', { class: 'chips' }, here.map(cellChip))] : null,
        h('h4', {}, 'Bitstream'),
        bitsBtn(x, y),
      );
    }
    ins.hidden = false;
  }

  /** Shows the placement, or the routing too. */
  setStep(step) {
    if (step === this.step) return;
    this.step = step;
    this.renderToolbar();
    if (this.data && this.r) this.paint();
  }

  /** Whether the routed wires are drawn. */
  routing() {
    return this.step === 'route' && settings.get('chipRouting');
  }

  renderToolbar() {
    if (!this.toolbar) return;
    const route = this.step === 'route';
    const tog = (key, label, title) => h('button', { class: settings.get(key) ? 'on' : '', title, onclick: () => settings.set(key, !settings.get(key)) }, label);
    const clocks = this.timing?.clocks || [];
    const critSel = h('select', {
      title: 'Show the critical path of a clock',
      onchange: (e) => {
        this.crit = e.target.value === '' ? null : Number(e.target.value);
        this.paintHighlight();
        this.r.update(this.hiBatch, this.hiCells);
      },
    }, h('option', { value: '' }, 'No critical path'), clocks.map((c, i) => h('option', { value: i, selected: this.crit === i }, `Critical path: ${c.clock}`)));
    const colorSel = h('select', { title: 'Wire colours', onchange: (e) => settings.set('chipColorBy', e.target.value) },
      [['net', 'Colour by net'], ['crit', 'Colour by criticality'], ['class', 'Colour by wire type']].map(([v, l]) => h('option', { value: v, selected: settings.get('chipColorBy') === v }, l)));
    fill(this.toolbar, 
      h('div', { class: 'group' }, icon('layers'), route ? [tog('chipRouting', 'Routing', 'Show routed wires'), tog('chipGlobals', 'Globals', 'Show global networks')] : null, tog('chipPins', 'Pins', 'Show package pin names')),
      route ? h('div', { class: 'group' }, colorSel) : null,
      clocks.length ? h('div', { class: 'group' }, critSel) : null,
    );
  }

  legendUpdate() {
    const sw = (k, l) => h('span', {}, h('i', { style: { background: settings.get(k) } }), l);
    const mode = settings.get('chipColorBy');
    const wires = !this.routing() ? [] : mode === 'class'
      ? [sw('colSpan4', 'span 4'), sw('colSpan12', 'span 12'), sw('colLocal', 'local'), sw('colGlobal', 'global')]
      : mode === 'crit'
        ? [h('span', {}, 'slack', h('i', { class: 'gradient', style: { background: 'linear-gradient(90deg,#38bf73,#f2c740,#f24d4d)', width: '80px' } }), 'critical')]
        : [h('span', { class: 'muted' }, 'one colour per net')];
    const u = this.data?.usage;
    const usage = u ? h('span', { class: 'muted' }, `${u.lc}/${u.lc_total} LCs · ${u.io}/${u.io_total} IOs · ${u.ram}/${u.ram_total} RAMs`) : null;
    fill(this.legend, sw('colLc', 'LUT'), sw('colFf', 'flip-flop'), sw('colCarry', 'carry'), sw('colIo', 'IO'), sw('colRam', 'RAM'), ...wires, usage);
  }

  /** Frames the cells of a clock's critical path. */
  fitPath(ci) {
    const path = this.timing?.clocks?.[ci]?.path || [];
    let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    for (const p of path) {
      if (p.ci === undefined) continue;
      const [x, y, w, hh] = this.cellRect(this.data.cells[p.ci]);
      x0 = Math.min(x0, x); y0 = Math.min(y0, y); x1 = Math.max(x1, x + w); y1 = Math.max(y1, y + hh);
    }
    if (x0 < Infinity) this.r.fit(x0 - T, y0 - T, x1 + T, y1 + T, 30, 6);
  }

  fit() {
    if (!this.data || !this.r) return;
    this.r.fit(-T * 0.6, -T * 0.6, this.data.gw * T + T * 0.6, this.data.gh * T + T * 0.6, 20, 20);
  }

  visible() {
    if (this.r) this.r.resize();
  }
}

function distSeg(px, py, x0, y0, x1, y1) {
  const dx = x1 - x0, dy = y1 - y0;
  const l2 = dx * dx + dy * dy;
  let t = l2 ? ((px - x0) * dx + (py - y0) * dy) / l2 : 0;
  t = Math.max(0, Math.min(1, t));
  return Math.hypot(px - x0 - t * dx, py - y0 - t * dy);
}

function clipText(ctx, s, max) {
  if (ctx.measureText(s).width <= max) return s;
  while (s.length > 1 && ctx.measureText(`${s}…`).width > max) s = s.slice(0, -1);
  return `${s}…`;
}
