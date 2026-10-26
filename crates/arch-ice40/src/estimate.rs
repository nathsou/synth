//! Delay estimates of unrouted connections, by tile offset, for
//! timing-driven placement and the router's first criticalities.
//!
//! The fastest routes from each logic cell output of a few source tiles to
//! the logic cell inputs of every other tile are searched on the routing
//! graph and timed as the timing analysis times routes (a span wire costs
//! its driver's delay for how far along the wire the route leaves it). The
//! table keeps their mean by `(|dx|, |dy|)`. No function of the Manhattan
//! distance comes close: tiles next to each other, diagonally too, are
//! reached through neighbour outputs alone, and straight runs along span
//! wires are much faster than routes that turn.

use crate::arch::{Arch, BelKind, SPAN4_H, SPAN4_V, SPAN12_H, SPAN12_V};
use crate::device::TileKind;

/// Mean fastest delays (ps) from a logic cell to the logic cells of a tile
/// `(|dx|, |dy|)` away.
#[derive(Clone, Debug)]
pub struct DelayTable {
    side: usize,
    delay: Vec<f64>,
}

impl DelayTable {
    pub fn new(arch: &Arch) -> DelayTable {
        let die = arch.db.die;
        let (w, h) = (die.width(), die.height());
        let side = w.max(h) + 1;
        let is_logic = |x: usize, y: usize| die.tile_kind(x, y) == TileKind::Logic;
        let logic: Vec<(usize, usize)> = (1..=w)
            .flat_map(|x| (1..=h).map(move |y| (x, y)))
            .filter(|&(x, y)| is_logic(x, y))
            .collect();
        // Sources: the logic tiles nearest the quarter points of the die,
        // typical of where logic sits, and two opposite corners, from which
        // the longest offsets are reached.
        let mut sources: Vec<(usize, usize)> = Vec::new();
        let points =
            [(w / 4, h / 4), (3 * w / 4, h / 4), (w / 4, 3 * h / 4), (3 * w / 4, 3 * h / 4)];
        for (tx, ty) in points.into_iter().chain([(1, 1), (w, h)]) {
            let near =
                logic.iter().copied().min_by_key(|&(x, y)| (x.abs_diff(tx) + y.abs_diff(ty), x, y));
            if let Some(s) = near
                && !sources.contains(&s)
            {
                sources.push(s);
            }
        }
        let outputs: Vec<((usize, usize), u32)> = sources
            .iter()
            .flat_map(|&(x, y)| {
                (0..8).filter_map(move |z| {
                    let bel = arch.bel_at(x as u8, y as u8, z, BelKind::Lc)?;
                    Some(((x, y), arch.bel(bel).pin("O")?.raw()))
                })
            })
            .collect();
        // The logical inputs of each logic tile's cells.
        let inputs: Vec<((usize, usize), Vec<usize>)> = logic
            .iter()
            .map(|&(x, y)| {
                let pins = (0..8u8)
                    .filter_map(|z| arch.bel_at(x as u8, y as u8, z, BelKind::Lc))
                    .flat_map(|b| ["PERM0", "PERM1", "PERM2", "PERM3"].map(|p| arch.bel(b).pin(p)))
                    .flatten()
                    .map(|n| n.index())
                    .collect();
                ((x, y), pins)
            })
            .collect();
        let delays = Delays::new(arch);
        // Each output's fastest delay to each tile, searched on threads of
        // their own and summed in a fixed order (results do not depend on
        // the number of threads).
        let threads = synth_core::par::threads(8);
        let chunks: Vec<&[((usize, usize), u32)]> =
            outputs.chunks(outputs.len().div_ceil(threads).max(1)).collect();
        let found: Vec<Vec<f32>> = synth_core::par::map(&chunks, threads, |chunk| {
            let mut search = Search::new(arch, &delays);
            chunk
                .iter()
                .map(|&(_, out)| {
                    search.run(arch, &delays, out);
                    inputs
                        .iter()
                        .map(|(_, pins)| {
                            let d = pins.iter().map(|&n| search.dist[n]).min().unwrap_or(u32::MAX);
                            if d == u32::MAX { f32::INFINITY } else { d as f32 }
                        })
                        .collect::<Vec<f32>>()
                })
                .collect::<Vec<_>>()
        })
        .into_iter()
        .flatten()
        .collect();
        let mut sum = vec![0.0; side * side];
        let mut count = vec![0u32; side * side];
        for (&((sx, sy), _), delays) in outputs.iter().zip(&found) {
            for (&((x, y), _), &d) in inputs.iter().zip(delays) {
                if d.is_finite() {
                    let k = y.abs_diff(sy) * side + x.abs_diff(sx);
                    sum[k] += d as f64;
                    count[k] += 1;
                }
            }
        }
        // Offsets between no two logic tiles take their nearest offset's.
        let reached: Vec<usize> = (0..side * side).filter(|&k| count[k] > 0).collect();
        let delay = (0..side * side)
            .map(|k| {
                let j = if count[k] > 0 {
                    k
                } else {
                    let (x, y) = (k % side, k / side);
                    let near = reached
                        .iter()
                        .min_by_key(|&&j| ((j % side).abs_diff(x) + (j / side).abs_diff(y), j));
                    match near {
                        Some(&j) => j,
                        None => return 0.0,
                    }
                };
                sum[j] / count[j] as f64
            })
            .collect();
        DelayTable { side, delay }
    }

    /// The estimated delay of a connection `(dx, dy)` tiles long.
    #[inline]
    pub fn at(&self, dx: i32, dy: i32) -> f64 {
        let (x, y) = (dx.unsigned_abs() as usize, dy.unsigned_abs() as usize);
        self.delay[y.min(self.side - 1) * self.side + x.min(self.side - 1)]
    }
}

/// Dijkstra's search of the fastest routes from one node, timed as
/// `timing::routed_node_delays` times routes, in whole picoseconds: with
/// delays bounded, a queue of buckets by delay (Dial's) serves the nodes in
/// order without a heap.
struct Search {
    /// Delay of each node reached (ps; `u32::MAX` if not).
    dist: Vec<u32>,
    /// For span wires reached: their kind and the position of the pip the
    /// route enters them by.
    entry: Vec<(u8, (usize, usize))>,
    /// Nodes by their delay modulo the bucket count, which exceeds any
    /// edge's delay.
    buckets: Vec<Vec<u32>>,
}

/// Delays in whole picoseconds: of each edge (0 into span wires, whose
/// delay comes on leaving them), and of span drivers by kind and distance.
struct Delays {
    edge: Vec<u32>,
    span: [[u32; 13]; 5],
}

impl Delays {
    fn new(arch: &Arch) -> Delays {
        let g = &arch.graph;
        let edge = (0..g.edge_dst.len())
            .map(|e| if g.edge_span[e] == 0 { g.edge_delay[e].round() as u32 } else { 0 })
            .collect();
        let mut span = [[0; 13]; 5];
        for k in [SPAN4_H, SPAN4_V, SPAN12_H, SPAN12_V] {
            for (t, d) in span[k as usize].iter_mut().enumerate() {
                *d = arch.span_delay(k, t).round() as u32;
            }
        }
        Delays { edge, span }
    }
}

impl Search {
    fn new(arch: &Arch, delays: &Delays) -> Search {
        let edge = delays.edge.iter().max().copied().unwrap_or(0);
        let span = delays.span.iter().flatten().max().copied().unwrap_or(0);
        let n = arch.graph.num_nodes();
        Search {
            dist: vec![u32::MAX; n],
            entry: vec![(0, (0, 0)); n],
            buckets: vec![Vec::new(); (edge + span) as usize + 1],
        }
    }

    fn run(&mut self, arch: &Arch, delays: &Delays, source: u32) {
        let g = &arch.graph;
        let len = self.buckets.len();
        self.dist.fill(u32::MAX);
        self.dist[source as usize] = 0;
        self.entry[source as usize] = (0, (0, 0));
        self.buckets[0].push(source);
        let (mut at_delay, mut queued) = (0u32, 1usize);
        while queued > 0 {
            let b = at_delay as usize % len;
            while let Some(u) = self.buckets[b].pop() {
                queued -= 1;
                if self.dist[u as usize] != at_delay {
                    continue;
                }
                let (kind, (ex, ey)) = self.entry[u as usize];
                for e in g.edges(u) {
                    let v = g.edge_dst[e] as usize;
                    let span = g.edge_span[e];
                    let mut nd = at_delay + delays.edge[e];
                    // Where the pip is matters only on span wires.
                    let at =
                        if kind != 0 || span != 0 { arch.pip_xy(g.edge_pip[e]) } else { (0, 0) };
                    if kind != 0 {
                        let horizontal = kind == SPAN4_H || kind == SPAN12_H;
                        let far = if horizontal { at.0.abs_diff(ex) } else { at.1.abs_diff(ey) };
                        let max = if kind == SPAN4_H || kind == SPAN4_V { 4 } else { 12 };
                        nd += delays.span[kind as usize][far.min(max)];
                    }
                    if nd < self.dist[v] {
                        self.dist[v] = nd;
                        self.entry[v] = (span, at);
                        self.buckets[nd as usize % len].push(v as u32);
                        queued += 1;
                    }
                }
            }
            at_delay += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::Part;

    #[test]
    fn neighbours_are_fast_and_straight_runs_beat_turns() {
        let arch = Arch::new(Part::Hx8k, "cb132").unwrap();
        let t = DelayTable::new(&arch);
        // Neighbour outputs reach the eight tiles around a tile directly.
        assert!(t.at(1, 1) < t.at(2, 0) && t.at(1, 1) < t.at(0, 2));
        assert_eq!(t.at(1, -1), t.at(-1, 1));
        // Along a row, span wires carry a route ten tiles for less than a
        // route turning halfway.
        assert!(t.at(10, 0) < t.at(5, 5));
        // Delays grow with distance overall.
        assert!(t.at(0, 0) < t.at(8, 8) && t.at(8, 8) < t.at(30, 30));
    }
}
