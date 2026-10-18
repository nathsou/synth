// Unit tests of the web app's pure modules: `node --test` (no dependencies).

import test from 'node:test';
import assert from 'node:assert/strict';
import { layout } from '../layout.js';
import { tokenize, highlightHtml, langOf } from '../highlight.js';

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
