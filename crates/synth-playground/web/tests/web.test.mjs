// Unit tests of the web app's pure modules: `node --test` (no dependencies).

import test from 'node:test';
import assert from 'node:assert/strict';
import { layout } from '../layout.js';
import { tokenize, highlightHtml, langOf } from '../highlight.js';
import * as progress from '../progress.js';
import { Terminal } from '../terminal.js';

function graph(n, edges) {
  return {
    nodes: Array.from({ length: n }, (_, i) => ({ w: 40, h: 30 + (i % 3) * 10, ins: [10], outs: [10], group: i % 2 ? 'a' : '' })),
    edges: edges.map(([s, d]) => ({ s, sp: 0, d, dp: 0 })),
  };
}

test('layout: edges go left to right, except reversed feedback edges', () => {
  const g = graph(6, [[0, 1], [1, 2], [2, 3], [3, 1], [0, 4], [4, 5], [5, 3]]);
  const l = layout(g);
  g.edges.forEach((e, i) => {
    if (l.back.has(i)) assert.ok(l.layer[e.s] >= l.layer[e.d], `edge ${i} is reversed`);
    else assert.ok(l.layer[e.s] < l.layer[e.d], `edge ${i} points forward`);
  });
  assert.equal(l.back.size, 1);
});

test('layout: nodes of a layer do not overlap', () => {
  const edges = [];
  for (let i = 0; i < 40; i++) edges.push([i % 7, 7 + ((i * 13) % 33)]);
  const g = graph(40, edges);
  const l = layout(g);
  for (const lay of l.layers) {
    const sorted = [...lay].sort((a, b) => l.y[a] - l.y[b]);
    for (let k = 1; k < sorted.length; k++) {
      const a = sorted[k - 1], b = sorted[k];
      assert.ok(l.y[b] >= l.y[a] + g.nodes[a].h - 1e-6, 'separated');
    }
  }
});

test('layout: input ports come first and output ports last', () => {
  const g = graph(4, [[0, 1], [1, 2], [2, 3]]);
  g.nodes[0].role = 'in';
  g.nodes[3].role = 'out';
  g.edges.push({ s: 0, sp: 0, d: 3, dp: 0 });
  const l = layout(g);
  assert.equal(l.layer[0], 0);
  assert.equal(l.layer[3], Math.max(...l.layer));
});

test('highlighting: tokens of Weft', () => {
  const src = 'module top(in clk: clock) { reg x: uint<4> = 0x3; // hi\n x := x +% 1; }';
  const kinds = tokenize(src, 'weft').map(([s, e, c]) => [src.slice(s, e), c]);
  const has = (t, c) => assert.ok(kinds.some(([a, b]) => a === t && b === c), `${t} as ${c}`);
  has('module', 'tok-kw');
  has('top', 'tok-type');
  has('clock', 'tok-type');
  has('0x3', 'tok-num');
  has('// hi', 'tok-com');
  has(':=', 'tok-op');
  has('+%', 'tok-op');
  assert.equal(highlightHtml('a < b', 'weft').includes('&lt;'), true);
});

test('highlighting: semantic kinds and languages', () => {
  const src = 'q = count;';
  const sem = new Map([[4, 'Register']]);
  assert.ok(tokenize(src, 'weft', sem).some(([s, , c]) => s === 4 && c === 'tok-reg'));
  assert.equal(langOf('a.v'), 'verilog');
  assert.equal(langOf('pins.pcf'), 'pcf');
  const pcf = tokenize('set_io clk 15 # pin', 'pcf');
  assert.equal(pcf[0][2], 'tok-kw');
  assert.equal(pcf[pcf.length - 1][2], 'tok-com');
});

test('highlight: XDC constraints read as pin files', () => {
  assert.equal(langOf('pins.xdc'), 'pcf');
  const toks = tokenize('set_property -dict {PACKAGE_PIN E3 IOSTANDARD LVCMOS33} [get_ports {clk}] # c', 'pcf');
  const kw = toks.filter((t) => t[2] === 'tok-kw').length;
  assert.ok(kw >= 4, `keywords: ${kw}`);
});

test('progress: steps before the current one are full, the bar adds up', () => {
  for (const family of Object.keys(progress.STEPS)) {
    const start = progress.layout(family, null);
    assert.equal(start.fraction, 0);
    assert.ok(Math.abs(start.segments.reduce((a, s) => a + s.share, 0) - 1) < 1e-9);
    let last = 0;
    for (const s of start.segments) {
      const mid = progress.layout(family, { step: s.step, fraction: 0.5, detail: '' });
      assert.ok(mid.fraction > last, `${family} ${s.step} moves the bar on`);
      last = mid.fraction;
      const unknown = progress.layout(family, { step: s.step, fraction: null, detail: '' });
      assert.equal(unknown.segments[unknown.index].fill, null);
      assert.ok(unknown.segments.slice(0, unknown.index).every((x) => x.fill === 1));
    }
  }
  assert.equal(progress.describe({ step: 'route', fraction: 0.5, detail: 'iteration 3' }, 2500), 'Routing · iteration 3 · 2.5 s');
});

test('progress: learning moves the weights toward a run\'s shares', () => {
  const run = { elaborate: 0, synthesize: 600, pack: 0, device: 100, place: 100, route: 200, bitstream: 0, timing: 0 };
  let w = {};
  for (let i = 0; i < 30; i++) w = progress.learn('Artix-7', w, run);
  assert.ok(Math.abs(w.synthesize - 60) < 1, `synthesize ${w.synthesize}`);
  assert.equal(w.timing, 0.5);
  // Halfway through synthesis is now about 30% of the run.
  const l = progress.layout('Artix-7', { step: 'synthesize', fraction: 0.5, detail: '' }, w);
  assert.ok(Math.abs(l.fraction - 0.3) < 0.02, `fraction ${l.fraction}`);
  assert.deepEqual(progress.learn('Artix-7', w, {}), w);
});

test('terminal: text, line endings, cursor home and clearing', () => {
  const t = new Terminal(10, 4);
  const bytes = (s) => [...s].map((c) => c.charCodeAt(0));
  t.write(bytes('ab\r\ncd'));
  assert.equal(t.text(), 'ab\ncd');
  // A design redrawing in place: home, then the new frame over the old.
  t.write(bytes('\x1b[Hxy'));
  assert.equal(t.text(), 'xy\ncd');
  assert.equal(t.frames, 1);
  t.write(bytes('\x1b[2J\x1b[2;3Hz'));
  assert.equal(t.text(), '\n  z');
  t.write(bytes('\b\bq\x1b[K'));
  assert.equal(t.text(), '\n q');
  // Lines past the bottom scroll the screen up.
  t.write(bytes('\r\n1\r\n2\r\n3'));
  assert.equal(t.text().split('\n').length, 4);
  assert.equal(t.text().split('\n')[3], '3');
});
