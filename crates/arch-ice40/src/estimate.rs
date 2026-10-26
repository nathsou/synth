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

use std::collections::BinaryHeap;

use crate::arch::{Arch, BelKind, SPAN4_H, SPAN12_H};
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
        // Each output's fastest delay to each tile, searched on threads of
        // their own and summed in a fixed order (results do not depend on
        // the number of threads).
        let found = synth_core::par::map(&outputs, synth_core::par::threads(8), |&(_, out)| {
            let mut search = Search::new(arch.graph.num_nodes());
            search.run(arch, out);
            inputs
                .iter()
                .map(|(_, pins)| pins.iter().map(|&n| search.dist[n]).fold(f32::INFINITY, f32::min))
                .collect::<Vec<f32>>()
        });
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
/// `timing::routed_node_delays` times routes.
struct Search {
    dist: Vec<f32>,
    /// For span wires reached: their kind and the position of the pip the
    /// route enters them by.
    entry: Vec<(u8, (usize, usize))>,
}

impl Search {
    fn new(n: usize) -> Search {
        Search { dist: vec![f32::INFINITY; n], entry: vec![(0, (0, 0)); n] }
    }

    fn run(&mut self, arch: &Arch, source: u32) {
        let g = &arch.graph;
        self.dist.fill(f32::INFINITY);
        self.dist[source as usize] = 0.0;
        self.entry[source as usize] = (0, (0, 0));
        let mut heap = BinaryHeap::new();
        heap.push(Entry(0.0, source));
        while let Some(Entry(d, u)) = heap.pop() {
            if d > self.dist[u as usize] {
                continue;
            }
            let (kind, (ex, ey)) = self.entry[u as usize];
            for e in g.edges(u) {
                let v = g.edge_dst[e] as usize;
                let span = g.edge_span[e];
                let mut nd = d + if span == 0 { g.edge_delay[e] } else { 0.0 };
                let at = arch.pip_xy(g.edge_pip[e]);
                if kind != 0 {
                    let horizontal = kind == SPAN4_H || kind == SPAN12_H;
                    let far = if horizontal { at.0.abs_diff(ex) } else { at.1.abs_diff(ey) };
                    nd += arch.span_delay(kind, far);
                }
                if nd < self.dist[v] {
                    self.dist[v] = nd;
                    self.entry[v] = (span, at);
                    heap.push(Entry(nd, v as u32));
                }
            }
        }
    }
}

/// A search queue entry: least delay first, then node order.
#[derive(PartialEq)]
struct Entry(f32, u32);

impl Eq for Entry {}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.0.total_cmp(&self.0).then(other.1.cmp(&self.1))
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
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
