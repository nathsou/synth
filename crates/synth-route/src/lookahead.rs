//! A lower bound on the remaining cost of a route by node class and
//! distance: the A* estimate for graphs whose wires span many positions.
//!
//! The cheapest cost and the least delay from a node to a sink are found by
//! searching backwards from sample sinks on the uncongested graph, and kept
//! per node class and offset from the sink (folded over the four quadrants)
//! as the minimum over the nodes and samples there. Congestion only adds
//! cost, so the table bounds every sink like the samples. A class never
//! seen at an offset falls back to the minimum over all classes there, and
//! past the searched radius it grows at the mean rate per tile of the outer
//! half of the table, so it is an estimate rather than a bound there.

use std::collections::BinaryHeap;

use crate::{Graph, NetRequest, QueueEntry};

#[derive(Debug, Clone)]
pub(crate) struct Lookahead {
    radius: i16,
    /// Per class, then `|dy|`, then `|dx|`.
    cost: Vec<f32>,
    delay: Vec<f32>,
    /// Cost and delay per tile beyond the radius.
    cost_slope: f32,
    delay_slope: f32,
}

/// Sinks searched from: spread over the sinks of `nets` in node order.
const SAMPLES: usize = 16;

/// The graph searched backwards.
pub(crate) struct Searched<'g> {
    pub graph: &'g Graph<'g>,
    pub preds: &'g [u32],
    pub pred_edges: &'g [u32],
    pub edge_src: &'g [u32],
}

fn class_of(class: &[u16], node: usize) -> usize {
    class.get(node).map_or(0, |&c| c as usize)
}

impl Lookahead {
    pub(crate) fn new(
        g: &Searched<'_>,
        class: &[u16],
        nets: &[NetRequest],
        radius: i16,
    ) -> Lookahead {
        let mut sinks: Vec<u32> = nets.iter().flat_map(|n| n.sinks.iter().copied()).collect();
        sinks.sort_unstable();
        sinks.dedup();
        let step = sinks.len().div_ceil(SAMPLES).max(1);
        let samples: Vec<u32> = sinks.iter().copied().step_by(step).collect();
        let side = radius as usize + 1;
        let cells = side * side;
        let classes = class.iter().map(|&c| c as usize + 1).max().unwrap_or(1);
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        let chunk = samples.len().div_ceil(threads).max(1);
        let search = move |chunk: &[u32]| {
            let mut search = Search::new(g.graph.num_nodes(), classes * cells);
            for (i, &sink) in chunk.iter().enumerate() {
                // The first tenth of routing (reports from worker threads
                // are dropped).
                synth_core::progress::advance(0.1 * i as f64 / chunk.len() as f64, || {
                    "building the routing lookahead".into()
                });
                for by_delay in [false, true] {
                    search.run(g, class, sink, radius, by_delay);
                }
            }
            (search.cost, search.delay)
        };
        // One thread searches in place (WebAssembly cannot spawn threads).
        let found: Vec<(Vec<f32>, Vec<f32>)> = if threads == 1 {
            vec![search(&samples)]
        } else {
            std::thread::scope(|s| {
                let workers: Vec<_> =
                    samples.chunks(chunk).map(|chunk| s.spawn(move || search(chunk))).collect();
                workers.into_iter().map(|w| w.join().expect("lookahead search panicked")).collect()
            })
        };
        let mut cost = vec![f32::INFINITY; classes * cells];
        let mut delay = cost.clone();
        for (c, d) in found {
            for (t, v) in cost.iter_mut().zip(c) {
                *t = t.min(v);
            }
            for (t, v) in delay.iter_mut().zip(d) {
                *t = t.min(v);
            }
        }
        let fill = |t: &mut [f32]| {
            let mut any = vec![f32::INFINITY; cells];
            for class in t.chunks(cells) {
                for (a, &v) in any.iter_mut().zip(class) {
                    *a = a.min(v);
                }
            }
            // Offsets no search reached bound nothing.
            for a in &mut any {
                if !a.is_finite() {
                    *a = 0.0;
                }
            }
            for class in t.chunks_mut(cells) {
                for (v, &a) in class.iter_mut().zip(&any) {
                    if !v.is_finite() {
                        *v = a;
                    }
                }
            }
            // The mean growth over the outer half of the table, along rows
            // and columns (the long wires that cover far distances).
            let at = |x: i16, y: i16| any[y as usize * side + x as usize];
            let half = (radius / 2).max(1);
            let mut growth = 0.0;
            for k in 0..=radius {
                growth +=
                    at(radius, k) - at(radius - half, k) + at(k, radius) - at(k, radius - half);
            }
            (growth / (2 * (radius as usize + 1)) as f32 / half as f32).max(0.0)
        };
        let cost_slope = fill(&mut cost);
        let delay_slope = fill(&mut delay);
        Lookahead { radius, cost, delay, cost_slope, delay_slope }
    }

    /// Lower bounds on the cost and delay of reaching a sink `(dx, dy)`
    /// tiles away from a node of `class`.
    pub(crate) fn at(&self, class: usize, dx: i16, dy: i16) -> (f32, f32) {
        let (dx, dy) = (dx.unsigned_abs() as i16, dy.unsigned_abs() as i16);
        let beyond = ((dx - self.radius).max(0) + (dy - self.radius).max(0)) as f32;
        let side = self.radius as usize + 1;
        let row = class * side + dy.min(self.radius) as usize;
        let k = row * side + dx.min(self.radius) as usize;
        (self.cost[k] + beyond * self.cost_slope, self.delay[k] + beyond * self.delay_slope)
    }
}

/// Backward Dijkstra state, reused across samples.
struct Search {
    dist: Vec<f32>,
    touched: Vec<u32>,
    cost: Vec<f32>,
    delay: Vec<f32>,
}

impl Search {
    fn new(n: usize, entries: usize) -> Search {
        Search {
            dist: vec![f32::INFINITY; n],
            touched: Vec::new(),
            cost: vec![f32::INFINITY; entries],
            delay: vec![f32::INFINITY; entries],
        }
    }

    /// Records, for every node within `radius` of `sink`, the cheapest cost
    /// of the nodes after it on a path to `sink` (what a route pays from
    /// there on), or the least delay of the edges.
    fn run(&mut self, g: &Searched<'_>, class: &[u16], sink: u32, radius: i16, by_delay: bool) {
        for &k in &self.touched {
            self.dist[k as usize] = f32::INFINITY;
        }
        self.touched.clear();
        let graph = g.graph;
        let (tx, ty) = (graph.node_x[sink as usize], graph.node_y[sink as usize]);
        let side = radius as usize + 1;
        let table = if by_delay { &mut self.delay } else { &mut self.cost };
        let own = |k: usize| if by_delay { 0.0 } else { graph.node_cost[k] };
        // Distances include the node's own cost, which its predecessors pay.
        let first = own(sink as usize);
        self.dist[sink as usize] = first;
        self.touched.push(sink);
        let mut heap = BinaryHeap::new();
        heap.push(QueueEntry { f: first, g: first, node: sink });
        while let Some(QueueEntry { g: d, node, .. }) = heap.pop() {
            let k = node as usize;
            if d > self.dist[k] {
                continue;
            }
            let (dx, dy) = ((graph.node_x[k] - tx).abs(), (graph.node_y[k] - ty).abs());
            let cell = &mut table[(class_of(class, k) * side + dy as usize) * side + dx as usize];
            *cell = cell.min(d - own(k));
            for &e in &g.pred_edges[g.preds[k] as usize..g.preds[k + 1] as usize] {
                let s = g.edge_src[e as usize] as usize;
                if (graph.node_x[s] - tx).abs() > radius || (graph.node_y[s] - ty).abs() > radius {
                    continue;
                }
                let nd = d + if by_delay { graph.edge_delay[e as usize] } else { own(s) };
                if nd < self.dist[s] {
                    if !self.dist[s].is_finite() {
                        self.touched.push(s as u32);
                    }
                    self.dist[s] = nd;
                    heap.push(QueueEntry { f: nd, g: nd, node: s as u32 });
                }
            }
        }
    }
}
