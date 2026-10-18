// Layered graph layout for schematics (in the spirit of Sugiyama et al.):
// break cycles, assign layers by longest path, order each layer by
// barycentres (keeping members of an instance together), then place nodes
// vertically by isotonic regression towards the ports they connect to.
//
// layout({nodes: [{w, h, ins: [dy], outs: [dy], group, role}], edges: [{s, sp, d, dp}]})
// → {x, y, layerOf, back: Set of reversed edge indices}

export function layout(graph, opts = {}) {
  const nodes = graph.nodes;
  const edges = graph.edges;
  const n = nodes.length;
  const gapMin = opts.gap ?? 70;
  const sep = opts.sep ?? 14;
  const groupSep = opts.groupSep ?? 30;
  const succ = Array.from({ length: n }, () => []);
  const pred = Array.from({ length: n }, () => []);
  edges.forEach((e, i) => {
    if (e.s === e.d) return;
    succ[e.s].push(i);
    pred[e.d].push(i);
  });

  // 1. Cycle breaking: depth-first, reversing edges to nodes on the stack.
  const back = new Set();
  const state = new Uint8Array(n); // 0 new, 1 on stack, 2 done
  const visitOrder = new Int32Array(n).fill(-1);
  let visited = 0;
  const roots = [...nodes.keys()].sort((a, b) => rootRank(nodes[a]) - rootRank(nodes[b]) || pred[a].length - pred[b].length || a - b);
  for (const r of roots) {
    if (state[r]) continue;
    const stack = [[r, 0]];
    state[r] = 1;
    visitOrder[r] = visited++;
    while (stack.length) {
      const top = stack[stack.length - 1];
      const [v, k] = top;
      if (k < succ[v].length) {
        top[1]++;
        const ei = succ[v][k];
        const w = edges[ei].d;
        if (state[w] === 1) back.add(ei);
        else if (state[w] === 0) {
          state[w] = 1;
          visitOrder[w] = visited++;
          stack.push([w, 0]);
        }
      } else {
        state[v] = 2;
        stack.pop();
      }
    }
  }
  const from = (ei) => (back.has(ei) ? edges[ei].d : edges[ei].s);
  const to = (ei) => (back.has(ei) ? edges[ei].s : edges[ei].d);
  const dsucc = Array.from({ length: n }, () => []);
  const dpred = Array.from({ length: n }, () => []);
  edges.forEach((e, i) => {
    if (e.s === e.d) return;
    dsucc[from(i)].push(to(i));
    dpred[to(i)].push(from(i));
  });

  // 2. Layers: longest path, then sources pulled next to their sinks.
  const layer = new Int32Array(n);
  const indeg = Int32Array.from(dpred, (p) => p.length);
  const queue = [];
  for (let v = 0; v < n; v++) if (indeg[v] === 0) queue.push(v);
  const topo = [];
  while (queue.length) {
    const v = queue.pop();
    topo.push(v);
    for (const w of dsucc[v]) {
      layer[w] = Math.max(layer[w], layer[v] + 1);
      if (--indeg[w] === 0) queue.push(w);
    }
  }
  for (let i = topo.length - 1; i >= 0; i--) {
    const v = topo[i];
    if (nodes[v].role === 'in' || dsucc[v].length === 0) continue;
    let m = Infinity;
    for (const w of dsucc[v]) m = Math.min(m, layer[w]);
    if (m - 1 > layer[v]) layer[v] = m - 1;
  }
  let maxLayer = 0;
  for (let v = 0; v < n; v++) maxLayer = Math.max(maxLayer, layer[v]);
  for (let v = 0; v < n; v++) {
    if (nodes[v].role === 'out') layer[v] = Math.max(maxLayer, 1);
    if (nodes[v].role === 'in') layer[v] = 0;
  }
  maxLayer = 0;
  for (let v = 0; v < n; v++) maxLayer = Math.max(maxLayer, layer[v]);

  // 3. Order within layers.
  const layers = Array.from({ length: maxLayer + 1 }, () => []);
  const byVisit = [...nodes.keys()].sort((a, b) => visitOrder[a] - visitOrder[b]);
  for (const v of byVisit) layers[layer[v]].push(v);
  const pos = new Float64Array(n);
  const setPos = (L) => layers[L].forEach((v, i) => { pos[v] = (i + 0.5) / layers[L].length; });
  for (let L = 0; L <= maxLayer; L++) {
    // Keep groups together from the start.
    layers[L] = groupStable(layers[L], nodes);
    setPos(L);
  }
  const nbrs = (v, down) => (down ? dpred[v] : dsucc[v]);
  const sweeps = n > 4000 ? 4 : 10;
  for (let it = 0; it < sweeps; it++) {
    const down = it % 2 === 0;
    const order = down ? [...layers.keys()].slice(1) : [...layers.keys()].reverse().slice(1);
    for (const L of order) {
      const bary = new Map();
      for (const v of layers[L]) {
        const ns = nbrs(v, down);
        if (ns.length === 0) {
          bary.set(v, pos[v]);
          continue;
        }
        let s = 0;
        for (const u of ns) s += pos[u];
        bary.set(v, s / ns.length);
      }
      const gsum = new Map(), gcnt = new Map();
      for (const v of layers[L]) {
        const g = nodes[v].group;
        if (!g) continue;
        gsum.set(g, (gsum.get(g) || 0) + bary.get(v));
        gcnt.set(g, (gcnt.get(g) || 0) + 1);
      }
      const key = (v) => {
        const g = nodes[v].group;
        return g ? gsum.get(g) / gcnt.get(g) : bary.get(v);
      };
      layers[L].sort((a, b) => key(a) - key(b) || (nodes[a].group || '').localeCompare(nodes[b].group || '') || bary.get(a) - bary.get(b));
      setPos(L);
    }
  }

  // 4. Coordinates. Layers left to right, gaps growing with their wires.
  const x = new Float64Array(n);
  const y = new Float64Array(n);
  const between = new Int32Array(maxLayer + 1);
  edges.forEach((e, i) => {
    const a = layer[from(i)];
    if (a < maxLayer) between[a]++;
  });
  let cx = 0;
  for (let L = 0; L <= maxLayer; L++) {
    let w = 0;
    for (const v of layers[L]) w = Math.max(w, nodes[v].w);
    for (const v of layers[L]) x[v] = cx + (w - nodes[v].w) / 2;
    cx += w + gapMin + Math.min(160, 5 * Math.sqrt(between[L]));
  }
  // Initial stacking.
  const gap = (a, b) => (nodes[a].group !== nodes[b].group ? groupSep : sep);
  for (const lay of layers) {
    let cy = 0;
    lay.forEach((v, i) => {
      if (i > 0) cy += gap(lay[i - 1], v);
      y[v] = cy;
      cy += nodes[v].h;
    });
  }
  // Port positions per edge end.
  const outY = (ei) => nodes[edges[ei].s].outs[edges[ei].sp] ?? nodes[edges[ei].s].h / 2;
  const inY = (ei) => nodes[edges[ei].d].ins[edges[ei].dp] ?? nodes[edges[ei].d].h / 2;
  const incident = Array.from({ length: n }, () => []);
  edges.forEach((e, i) => {
    if (e.s === e.d) return;
    incident[e.s].push(i);
    incident[e.d].push(i);
  });
  const rounds = n > 4000 ? 4 : 8;
  for (let it = 0; it < rounds; it++) {
    const order = it % 2 === 0 ? [...layers.keys()] : [...layers.keys()].reverse();
    for (const L of order) {
      const lay = layers[L];
      if (!lay.length) continue;
      const desired = lay.map((v) => {
        let s = 0, c = 0;
        for (const ei of incident[v]) {
          const e = edges[ei];
          const other = e.s === v ? e.d : e.s;
          if (layer[other] === L) continue;
          const span = Math.abs(layer[other] - L);
          const wgt = 1 / span;
          const target = e.s === v ? y[e.d] + inY(ei) - outY(ei) : y[e.s] + outY(ei) - inY(ei);
          s += target * wgt;
          c += wgt;
        }
        return c ? s / c : y[v];
      });
      // Isotonic regression of desired - offset under the ordering.
      const off = new Float64Array(lay.length);
      for (let i = 1; i < lay.length; i++) off[i] = off[i - 1] + nodes[lay[i - 1]].h + gap(lay[i - 1], lay[i]);
      const blocks = [];
      for (let i = 0; i < lay.length; i++) {
        blocks.push({ sum: desired[i] - off[i], cnt: 1, start: i });
        while (blocks.length > 1) {
          const b = blocks[blocks.length - 1], a = blocks[blocks.length - 2];
          if (a.sum / a.cnt <= b.sum / b.cnt) break;
          a.sum += b.sum;
          a.cnt += b.cnt;
          blocks.pop();
        }
      }
      for (const b of blocks) {
        const z = b.sum / b.cnt;
        for (let i = b.start; i < b.start + b.cnt; i++) y[lay[i]] = z + off[i];
      }
    }
  }
  return { x, y, layer, back, layers };
}

function rootRank(node) {
  return node.role === 'in' ? 0 : node.role === 'out' ? 2 : 1;
}

/** Stable order that keeps nodes of a group contiguous, groups at the
 *  position of their first member. */
function groupStable(list, nodes) {
  const first = new Map();
  list.forEach((v, i) => {
    const g = nodes[v].group || `\u0000${v}`;
    if (!first.has(g)) first.set(g, i);
  });
  return [...list].sort((a, b) => {
    const ga = nodes[a].group || `\u0000${a}`, gb = nodes[b].group || `\u0000${b}`;
    return first.get(ga) - first.get(gb) || 0;
  });
}
