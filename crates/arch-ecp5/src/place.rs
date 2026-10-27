//! Placement of logic cells and carry chains by simulated annealing.
//!
//! Units are logic cells (a LUT and/or flip-flop, one of a tile's eight
//! slots), carry chains (consecutive slices of a row, moved whole) and
//! block RAMs and multipliers (on their sites); I/O cells are fixed to their pins. The cost is each net's bounding box half
//! perimeter, weighted for nets of many cells as VPR weights them, and,
//! timing-driven as VPR, each connection's estimated delay weighted by its
//! criticality to the eighth power (criticalities following the placement
//! at every temperature), the two terms normalised and weighted; under a
//! VPR schedule: the temperature falls and the move window narrows by the
//! share of moves accepted. A tile is legal when its flip-flops share one
//! clock and set/reset, and each slice's flip-flops one clock enable.

use synth_core::{FxHashMap, Rng};
use synth_netlist::{CellId, Netlist};

use crate::pack::Packed;
use crate::timing::{Connection, TimingGraph};

/// Where a placeable thing is: a logic cell slot, or a fixed position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Site {
    pub x: usize,
    pub y: usize,
    /// The cell's slot in its tile (0-7: slice `z / 2`, half `z % 2`).
    pub z: usize,
}

/// A placement: the site of every placed cell.
#[derive(Clone, Debug, Default)]
pub struct Placement {
    pub sites: FxHashMap<CellId, Site>,
}

#[derive(Clone, Debug)]
pub struct PlaceOptions {
    pub seed: u64,
    pub effort: f64,
    /// Weight of timing in the cost (0..1, 0 = wirelength only).
    pub timing_weight: f64,
}

/// Default weight of timing against wirelength.
pub const TIMING_WEIGHT: f64 = 0.5;

impl Default for PlaceOptions {
    fn default() -> Self {
        PlaceOptions { seed: 1, effort: 1.0, timing_weight: TIMING_WEIGHT }
    }
}

/// What timing-driven placement needs: the design's timing graph and the
/// speed grade its delays are estimated for.
#[derive(Clone, Copy, Debug)]
pub struct PlaceTiming<'a> {
    pub graph: &'a TimingGraph,
    pub grade: &'a str,
}

/// Slots and their previous contents, to undo a chain move.
type Saved = Vec<(usize, Occ)>;

/// What occupies a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Occ {
    Free,
    Lc(u32),
    /// A chain and which of its slices.
    Chain(u32, u32),
    /// A block RAM or multiplier (not in a logic slot).
    Block(u32),
}

/// The logic tiles of a device: positions, and the rows' runs of
/// adjacent tiles that carry chains run along.
#[derive(Clone, Debug)]
pub struct Grid {
    pub tiles: Vec<(usize, usize)>,
    at: FxHashMap<(usize, usize), usize>,
    /// Each row's tiles, west to east (all adjacent).
    rows: FxHashMap<usize, Vec<usize>>,
    /// Block RAM and multiplier sites: `(x, y, z)` and the cell kind
    /// they take.
    pub blocks: Vec<(usize, usize, usize, &'static str)>,
}

impl Grid {
    pub fn new(
        mut tiles: Vec<(usize, usize)>,
        mut blocks: Vec<(usize, usize, usize, &'static str)>,
    ) -> Grid {
        blocks.sort_unstable();
        tiles.sort_by_key(|&(x, y)| (y, x));
        let at = tiles.iter().enumerate().map(|(i, &p)| (p, i)).collect();
        let mut rows: FxHashMap<usize, Vec<usize>> = FxHashMap::default();
        for (i, &(_, y)) in tiles.iter().enumerate() {
            rows.entry(y).or_default().push(i);
        }
        Grid { tiles, at, rows, blocks }
    }
}

struct Annealer<'a> {
    nl: &'a Netlist,
    packed: &'a Packed,
    grid: &'a Grid,
    rng: Rng,
    /// Slot `tile * 8 + z`.
    occ: Vec<Occ>,
    lc_slot: Vec<usize>,
    /// Each chain's first slice: its row and index along the row.
    chain_at: Vec<(usize, usize)>,
    /// Each block RAM's site, and each site's block RAM.
    block_site: Vec<usize>,
    block_occ: Vec<Option<u32>>,
    /// Each cell's unit (by cell index): `Occ::Lc`, `Occ::Chain` or
    /// `Occ::Block`, and its offset in a chain (slices times two plus half).
    cell_unit: Vec<Option<(Occ, usize)>>,
    fixed: Vec<Option<(usize, usize)>>,
    /// Each cell's position (by cell index), following its unit's.
    xy: Vec<(i16, i16)>,
    /// Each net's cells, bounding box and cost, and each cell's nets.
    nets: Vec<Vec<CellId>>,
    net_bb: Vec<BBox>,
    net_cost: Vec<f64>,
    cell_nets: Vec<Vec<u32>>,
    width: usize,
    height: usize,
    timing: Option<PlaceTiming<'a>>,
    /// Connections between units, by timing graph connection: driver,
    /// sink and index; each one's weight (criticality^8) and cost, and
    /// each cell's connections.
    conns: Vec<(CellId, CellId, u32)>,
    conn_weight: Vec<f64>,
    conn_cost: Vec<f64>,
    cell_conns: Vec<Vec<u32>>,
    /// Timing graph connections that are dedicated (no delay).
    dedicated: Vec<bool>,
    lambda: f64,
    /// Normalisers of the wirelength and timing terms.
    bb_norm: f64,
    t_norm: f64,
}

/// A net's bounding box, with the number of its cells on each side (to
/// update it as cells move, as VPR does).
#[derive(Clone, Copy, Debug, Default)]
struct BBox {
    x0: i16,
    x1: i16,
    y0: i16,
    y1: i16,
    on_x0: u32,
    on_x1: u32,
    on_y0: u32,
    on_y1: u32,
}

impl BBox {
    /// The box of points `at` (an empty box if there are none).
    fn of(at: impl Iterator<Item = (i16, i16)>) -> BBox {
        let mut b: Option<BBox> = None;
        for (x, y) in at {
            let Some(b) = b.as_mut() else {
                b = Some(BBox {
                    x0: x,
                    x1: x,
                    y0: y,
                    y1: y,
                    on_x0: 1,
                    on_x1: 1,
                    on_y0: 1,
                    on_y1: 1,
                });
                continue;
            };
            side(&mut b.x0, &mut b.on_x0, x, |a, b| a < b);
            side(&mut b.x1, &mut b.on_x1, x, |a, b| a > b);
            side(&mut b.y0, &mut b.on_y0, y, |a, b| a < b);
            side(&mut b.y1, &mut b.on_y1, y, |a, b| a > b);
        }
        b.unwrap_or_default()
    }

    /// Moves one cell of the box from `from` to `to`; false if a side it
    /// alone was on may shrink, which only a new scan finds.
    fn shift(&mut self, from: (i16, i16), to: (i16, i16)) -> bool {
        shift(&mut self.x0, &mut self.on_x0, &mut self.x1, &mut self.on_x1, from.0, to.0)
            && shift(&mut self.y0, &mut self.on_y0, &mut self.y1, &mut self.on_y1, from.1, to.1)
    }

    fn half_perimeter(&self) -> f64 {
        (self.x1 - self.x0) as f64 + (self.y1 - self.y0) as f64
    }
}

/// Adds coordinate `v` to a side at `at` with `on` points, beyond it when
/// `beyond(v, at)`.
#[inline]
fn side(at: &mut i16, on: &mut u32, v: i16, beyond: impl Fn(i16, i16) -> bool) {
    if beyond(v, *at) {
        (*at, *on) = (v, 1);
    } else if v == *at {
        *on += 1;
    }
}

/// Moves one point of the span `lo..=hi` (with `on_lo` and `on_hi` points
/// on its ends) from `from` to `to`; false if an end it leaves alone may
/// move inward.
#[inline]
fn shift(lo: &mut i16, on_lo: &mut u32, hi: &mut i16, on_hi: &mut u32, from: i16, to: i16) -> bool {
    if to < from {
        if from == *hi {
            if *on_hi == 1 {
                return false;
            }
            *on_hi -= 1;
        }
        side(lo, on_lo, to, |a, b| a < b);
    } else if to > from {
        if from == *lo {
            if *on_lo == 1 {
                return false;
            }
            *on_lo -= 1;
        }
        side(hi, on_hi, to, |a, b| a > b);
    }
    true
}

/// Nets with at most this many cells get their bounding boxes by a scan
/// rather than updates.
const SCANNED_NET: usize = 8;

/// A move's effect on the costs: the moved cells' previous positions, the
/// touched nets' new boxes and costs, the connections' new costs, and the
/// normalised cost change.
struct Effect {
    from: Vec<(CellId, (i16, i16))>,
    nets: Vec<(u32, BBox, f64)>,
    conns: Vec<(u32, f64)>,
    delta: f64,
}

/// VPR's weights of a net's bounding box by its number of cells.
fn net_weight(n: usize) -> f64 {
    const W: [f64; 50] = [
        1.0, 1.0, 1.0, 1.0828, 1.1536, 1.2206, 1.2823, 1.3385, 1.3991, 1.4493, 1.4974, 1.5455,
        1.5937, 1.6418, 1.6899, 1.7304, 1.7709, 1.8114, 1.8519, 1.8924, 1.9288, 1.9652, 2.0015,
        2.0379, 2.0743, 2.1061, 2.1379, 2.1698, 2.2016, 2.2334, 2.2646, 2.2958, 2.3271, 2.3583,
        2.3895, 2.4187, 2.4479, 2.4772, 2.5064, 2.5356, 2.5610, 2.5864, 2.6117, 2.6371, 2.6625,
        2.6887, 2.7148, 2.7410, 2.7671, 2.7933,
    ];
    if n < W.len() { W[n] } else { 2.7933 + 0.02616 * (n - 49) as f64 }
}

impl<'a> Annealer<'a> {
    fn tile_slot(&self, t: usize, z: usize) -> usize {
        t * 8 + z
    }

    /// The tile and slice of a chain's `i`-th slice.
    fn chain_slice(&self, row: usize, start: usize, i: usize) -> Option<(usize, usize)> {
        let tiles = self.grid.rows.get(&row)?;
        let j = start + i;
        tiles.get(j / 4).map(|&t| (t, j % 4))
    }

    /// A cell's position, from its unit's.
    fn pos(&self, c: CellId) -> (usize, usize) {
        if let Some(p) = self.fixed[c.index()] {
            return p;
        }
        match self.cell_unit[c.index()] {
            Some((Occ::Lc(u), _)) => self.grid.tiles[self.lc_slot[u as usize] / 8],
            Some((Occ::Chain(ch, _), off)) => {
                let (row, start) = self.chain_at[ch as usize];
                let (t, _) = self.chain_slice(row, start, off / 2).expect("chain within its row");
                self.grid.tiles[t]
            }
            Some((Occ::Block(u), _)) => {
                let (x, y, _, _) = self.grid.blocks[self.block_site[u as usize]];
                (x, y)
            }
            _ => (0, 0),
        }
    }

    /// Sets the positions of `cells` from their units', returning their
    /// previous ones.
    fn refresh(&mut self, cells: &[CellId]) -> Vec<(CellId, (i16, i16))> {
        cells
            .iter()
            .map(|&c| {
                let (x, y) = self.pos(c);
                (c, std::mem::replace(&mut self.xy[c.index()], (x as i16, y as i16)))
            })
            .collect()
    }

    fn scan_bb(&self, n: usize) -> BBox {
        BBox::of(self.nets[n].iter().map(|c| self.xy[c.index()]))
    }

    fn bb_cost(&self, n: usize, bb: &BBox) -> f64 {
        net_weight(self.nets[n].len()) * bb.half_perimeter()
    }

    /// Every net's box and cost from scratch.
    fn reset_costs(&mut self) {
        self.net_bb = (0..self.nets.len()).map(|n| self.scan_bb(n)).collect();
        self.net_cost = (0..self.nets.len()).map(|n| self.bb_cost(n, &self.net_bb[n])).collect();
    }

    /// The effect of a move just applied to the units of `cells`, their
    /// positions updated (and to be restored with [`Annealer::undo_xy`] if
    /// the move is not taken).
    fn effect(&mut self, cells: &[CellId]) -> Effect {
        let from = self.refresh(cells);
        let nets = self.touched_nets(cells);
        let mut bb_delta = 0.0;
        let mut boxes = Vec::with_capacity(nets.len());
        for n in nets {
            let i = n as usize;
            let bb = if self.nets[i].len() <= SCANNED_NET {
                self.scan_bb(i)
            } else {
                let mut bb = self.net_bb[i];
                let ok = from.iter().all(|&(c, old)| {
                    !self.cell_nets[c.index()].contains(&n) || bb.shift(old, self.xy[c.index()])
                });
                if ok { bb } else { self.scan_bb(i) }
            };
            let cost = self.bb_cost(i, &bb);
            bb_delta += cost - self.net_cost[i];
            boxes.push((n, bb, cost));
        }
        let (dt, conns) = self.timing_delta(cells);
        let delta = (1.0 - self.lambda) * bb_delta / self.bb_norm + self.lambda * dt / self.t_norm;
        Effect { from, nets: boxes, conns, delta }
    }

    /// Restores the positions of a move not taken.
    fn undo_xy(&mut self, e: &Effect) {
        for &(c, p) in &e.from {
            self.xy[c.index()] = p;
        }
    }

    /// Records a taken move's costs.
    fn commit(&mut self, e: &Effect) {
        for &(n, bb, cost) in &e.nets {
            self.net_bb[n as usize] = bb;
            self.net_cost[n as usize] = cost;
        }
        for &(k, c) in &e.conns {
            self.conn_cost[k as usize] = c;
        }
    }

    /// Estimated delay between two cells' positions.
    fn delay(&self, a: CellId, b: CellId) -> f64 {
        let Some(t) = self.timing else { return 0.0 };
        let ((x0, y0), (x1, y1)) = (self.xy[a.index()], self.xy[b.index()]);
        crate::timing::estimate(t.grade, x0.abs_diff(x1) as usize, y0.abs_diff(y1) as usize)
    }

    /// Recomputes criticalities from the current placement and
    /// renormalises the cost terms.
    fn update_timing(&mut self) {
        self.bb_norm = self.net_cost.iter().sum::<f64>().max(1.0);
        let Some(t) = self.timing else { return };
        if self.lambda <= 0.0 {
            return;
        }
        let delays: Vec<f64> = t
            .graph
            .conns
            .iter()
            .enumerate()
            .map(|(k, c)| if self.dedicated[k] { 0.0 } else { self.delay(c.driver, c.sink) })
            .collect();
        let crit = t.graph.analyse(&delays).criticality;
        for k in 0..self.conns.len() {
            let (a, b, i) = self.conns[k];
            self.conn_weight[k] = crit[i as usize].powi(8);
            self.conn_cost[k] = self.conn_weight[k] * self.delay(a, b);
        }
        self.t_norm = self.conn_cost.iter().sum::<f64>().max(1.0);
    }

    /// The change of the timing term over the connections of `cells` (moved
    /// already), and their new costs.
    fn timing_delta(&self, cells: &[CellId]) -> (f64, Vec<(u32, f64)>) {
        if self.lambda <= 0.0 {
            return (0.0, Vec::new());
        }
        let mut ks: Vec<u32> =
            cells.iter().flat_map(|c| self.cell_conns[c.index()].iter().copied()).collect();
        ks.sort_unstable();
        ks.dedup();
        let mut d = 0.0;
        let mut out = Vec::with_capacity(ks.len());
        for k in ks {
            let w = self.conn_weight[k as usize];
            if w == 0.0 {
                continue;
            }
            let (a, b, _) = self.conns[k as usize];
            let new = w * self.delay(a, b);
            d += new - self.conn_cost[k as usize];
            out.push((k, new));
        }
        (d, out)
    }

    /// The flip-flops of a slot.
    fn slot_ff(&self, s: usize) -> Option<CellId> {
        match self.occ[s] {
            Occ::Lc(u) => self.packed.lcs[u as usize].ff,
            Occ::Chain(ch, i) => self.packed.chains[ch as usize][i as usize].ff[s % 2],
            Occ::Free | Occ::Block(_) => None,
        }
    }

    /// Whether tile `t`'s flip-flops share controls as they must.
    fn tile_ok(&self, t: usize) -> bool {
        let mut ctrl = None;
        for slice in 0..4 {
            let mut ce = None;
            for h in 0..2 {
                let Some(ff) = self.slot_ff(self.tile_slot(t, slice * 2 + h)) else { continue };
                let c = self.packed.ff_tile[&ff];
                if *ctrl.get_or_insert(c) != c {
                    return false;
                }
                let e = self.packed.ff_ce[&ff];
                if *ce.get_or_insert(e) != e {
                    return false;
                }
            }
        }
        true
    }

    fn touched_nets(&self, cells: &[CellId]) -> Vec<u32> {
        let mut nets: Vec<u32> =
            cells.iter().flat_map(|c| self.cell_nets[c.index()].iter().copied()).collect();
        nets.sort_unstable();
        nets.dedup();
        nets
    }

    fn unit_cells(&self, u: Occ) -> Vec<CellId> {
        match u {
            Occ::Lc(i) => {
                let lc = self.packed.lcs[i as usize];
                lc.lut.into_iter().chain(lc.ff).collect()
            }
            Occ::Chain(i, _) => self.packed.chains[i as usize]
                .iter()
                .flat_map(|s| std::iter::once(s.ccu).chain(s.ff.iter().flatten().copied()))
                .collect(),
            Occ::Block(u) => vec![self.packed.blocks[u as usize]],
            Occ::Free => Vec::new(),
        }
    }

    /// Tries moving logic cell `u` to slot `to` (swapping with what is
    /// there); if legal, applies it and returns the slot it left and the
    /// cells moved.
    fn move_lc(&mut self, u: u32, to: usize) -> Option<(usize, Vec<CellId>)> {
        let from = self.lc_slot[u as usize];
        if from == to {
            return None;
        }
        let other = self.occ[to];
        if matches!(other, Occ::Chain(..)) {
            return None;
        }
        self.occ[to] = Occ::Lc(u);
        self.occ[from] = other;
        self.lc_slot[u as usize] = to;
        if let Occ::Lc(o) = other {
            self.lc_slot[o as usize] = from;
        }
        if !self.tile_ok(to / 8) || !self.tile_ok(from / 8) {
            self.undo_lc(u, from, to);
            return None;
        }
        let mut cells = self.unit_cells(Occ::Lc(u));
        cells.extend(self.unit_cells(other));
        Some((from, cells))
    }

    fn undo_lc(&mut self, u: u32, from: usize, to: usize) {
        let other = self.occ[from];
        self.occ[from] = Occ::Lc(u);
        self.occ[to] = other;
        self.lc_slot[u as usize] = from;
        if let Occ::Lc(o) = other {
            self.lc_slot[o as usize] = to;
        }
    }

    /// The slots a chain's slices take at `(row, start)`.
    fn chain_slots(&self, ch: usize, row: usize, start: usize) -> Option<Vec<usize>> {
        let n = self.packed.chains[ch].len();
        let mut slots = Vec::with_capacity(2 * n);
        for i in 0..n {
            let (t, s) = self.chain_slice(row, start, i)?;
            slots.push(self.tile_slot(t, 2 * s));
            slots.push(self.tile_slot(t, 2 * s + 1));
        }
        Some(slots)
    }

    /// Tries moving chain `ch` to start at `(row, start)`, the logic cells
    /// there taking the slots it leaves; if legal, applies it and returns
    /// the slots' previous contents and the cells moved.
    fn move_chain(
        &mut self,
        ch: usize,
        row: usize,
        start: usize,
    ) -> Option<(Saved, Vec<CellId>)> {
        let (orow, ostart) = self.chain_at[ch];
        if (orow, ostart) == (row, start) {
            return None;
        }
        let new = self.chain_slots(ch, row, start)?;
        let old = self.chain_slots(ch, orow, ostart).expect("placed chain");
        if new.iter().any(|&s| matches!(self.occ[s], Occ::Chain(c, _) if c as usize != ch)) {
            return None;
        }
        let saved: Vec<(usize, Occ)> = new.iter().chain(&old).map(|&s| (s, self.occ[s])).collect();
        let displaced: Vec<u32> = new
            .iter()
            .filter_map(|&s| match self.occ[s] {
                Occ::Lc(u) => Some(u),
                _ => None,
            })
            .collect();
        let vacated: Vec<usize> = old.iter().copied().filter(|s| !new.contains(s)).collect();
        for &s in &old {
            self.occ[s] = Occ::Free;
        }
        for (i, &s) in new.iter().enumerate() {
            self.occ[s] = Occ::Chain(ch as u32, (i / 2) as u32);
        }
        for (&u, &s) in displaced.iter().zip(&vacated) {
            self.occ[s] = Occ::Lc(u);
            self.lc_slot[u as usize] = s;
        }
        self.chain_at[ch] = (row, start);
        let mut tiles: Vec<usize> = new.iter().chain(&old).map(|s| s / 8).collect();
        tiles.sort_unstable();
        tiles.dedup();
        if !tiles.iter().all(|&t| self.tile_ok(t)) {
            self.restore(&saved, ch, (orow, ostart));
            return None;
        }
        let mut cells = self.unit_cells(Occ::Chain(ch as u32, 0));
        for &u in &displaced {
            cells.extend(self.unit_cells(Occ::Lc(u)));
        }
        Some((saved, cells))
    }

    fn restore(&mut self, saved: &[(usize, Occ)], ch: usize, at: (usize, usize)) {
        for &(s, o) in saved.iter().rev() {
            self.occ[s] = o;
            if let Occ::Lc(u) = o {
                self.lc_slot[u as usize] = s;
            }
        }
        self.chain_at[ch] = at;
    }

    /// Moves block `u` to site `to`, swapping with the one there; returns
    /// the cells moved.
    fn move_block(&mut self, u: u32, to: usize) -> Option<Vec<CellId>> {
        let from = self.block_site[u as usize];
        if from == to {
            return None;
        }
        let other = self.block_occ[to];
        self.swap_block(u, other, from, to);
        let mut cells = self.unit_cells(Occ::Block(u));
        if let Some(o) = other {
            cells.extend(self.unit_cells(Occ::Block(o)));
        }
        Some(cells)
    }

    /// Puts block RAM `u` (at `from`) on `to`, and `other` (at `to`) on
    /// `from`.
    fn swap_block(&mut self, u: u32, other: Option<u32>, from: usize, to: usize) {
        self.block_occ[to] = Some(u);
        self.block_occ[from] = other;
        self.block_site[u as usize] = to;
        if let Some(o) = other {
            self.block_site[o as usize] = from;
        }
    }

    /// One move at temperature `temp` within `rlim` tiles; returns whether
    /// it was accepted (`None` if illegal or impossible).
    fn try_move(&mut self, temp: f64, rlim: usize) -> Option<bool> {
        let lcs = self.packed.lcs.len();
        let chains = self.packed.chains.len();
        let blocks = self.packed.blocks.len();
        let pick = self.rng.below(lcs + chains + blocks);
        let decide = |rng: &mut Rng, delta: f64| delta <= 0.0 || rng.unit() < (-delta / temp).exp();
        if pick >= lcs + chains {
            let u = (pick - lcs - chains) as u32;
            let (x, y, _, kind) = self.grid.blocks[self.block_site[u as usize]];
            // A random site of its kind within the window.
            let r = rlim;
            let near: Vec<usize> = (0..self.grid.blocks.len())
                .filter(|&s| {
                    let (sx, sy, _, k) = self.grid.blocks[s];
                    k == kind && sx.abs_diff(x) <= r && sy.abs_diff(y) <= r
                })
                .collect();
            let to = near[self.rng.below(near.len())];
            let from = self.block_site[u as usize];
            let cells = self.move_block(u, to)?;
            let e = self.effect(&cells);
            if decide(&mut self.rng, e.delta) {
                self.commit(&e);
                Some(true)
            } else {
                // The one now at `from` goes back to `to`.
                let back = self.block_occ[from];
                self.swap_block(u, back, to, from);
                self.undo_xy(&e);
                Some(false)
            }
        } else if pick < lcs {
            let u = pick as u32;
            let (x, y) = self.grid.tiles[self.lc_slot[pick] / 8];
            let r = rlim as i64;
            let tx =
                self.rng.range_inclusive(x as i64 - r, x as i64 + r).clamp(0, self.width as i64)
                    as usize;
            let ty =
                self.rng.range_inclusive(y as i64 - r, y as i64 + r).clamp(0, self.height as i64)
                    as usize;
            let &t = self.grid.at.get(&(tx, ty))?;
            let z = self.rng.below(8);
            let to = self.tile_slot(t, z);
            let (from, cells) = self.move_lc(u, to)?;
            let e = self.effect(&cells);
            if decide(&mut self.rng, e.delta) {
                self.commit(&e);
                Some(true)
            } else {
                self.undo_lc(u, from, to);
                self.undo_xy(&e);
                Some(false)
            }
        } else {
            let ch = pick - lcs;
            let (row, start) = self.chain_at[ch];
            let len = self.packed.chains[ch].len();
            let r = rlim as i64;
            let nrow = (row as i64 + self.rng.range_inclusive(-r, r)).clamp(0, self.height as i64)
                as usize;
            let row_len = self.grid.rows.get(&nrow).map_or(0, |t| t.len() * 4);
            if row_len < len {
                return None;
            }
            let shift = self.rng.range_inclusive(-4 * r, 4 * r);
            let nstart = (start as i64 + shift).clamp(0, (row_len - len) as i64) as usize;
            let (saved, cells) = self.move_chain(ch, nrow, nstart)?;
            let e = self.effect(&cells);
            if decide(&mut self.rng, e.delta) {
                self.commit(&e);
                Some(true)
            } else {
                self.restore(&saved, ch, (row, start));
                self.undo_xy(&e);
                Some(false)
            }
        }
    }

    /// The normalised cost.
    fn total(&self) -> f64 {
        let bb: f64 = self.net_cost.iter().sum();
        let t: f64 = self.conn_cost.iter().sum();
        (1.0 - self.lambda) * bb / self.bb_norm + self.lambda * t / self.t_norm
    }

    fn anneal(&mut self, effort: f64) -> usize {
        let units = (self.packed.lcs.len() + self.packed.chains.len() + self.packed.blocks.len())
            .max(1) as f64;
        let moves = ((effort * units.powf(4.0 / 3.0)) as usize).max(100);
        let nets = self.nets.len().max(1) as f64;
        let mut rlim = self.width.max(self.height);
        self.update_timing();
        // The starting temperature: twenty times the spread of random
        // moves' costs.
        let mut deltas = Vec::new();
        for _ in 0..(self.packed.lcs.len() + self.packed.chains.len()).min(500) {
            let before = self.total();
            if self.try_move(f64::INFINITY, rlim) == Some(true) {
                deltas.push(self.total() - before);
            }
        }
        let mean = deltas.iter().sum::<f64>() / deltas.len().max(1) as f64;
        let var =
            deltas.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / deltas.len().max(1) as f64;
        let mut temp = 20.0 * var.sqrt().max(1e-6);
        let mut steps = 0;
        loop {
            // Criticalities follow the placement; the terms renormalise.
            self.update_timing();
            let (mut tried, mut accepted) = (0usize, 0usize);
            for _ in 0..moves {
                if let Some(a) = self.try_move(temp, rlim) {
                    tried += 1;
                    accepted += a as usize;
                }
            }
            let rate = accepted as f64 / tried.max(1) as f64;
            temp *= if rate > 0.96 {
                0.5
            } else if rate > 0.8 {
                0.9
            } else if rate > 0.15 {
                0.95
            } else {
                0.8
            };
            rlim = ((rlim as f64 * (1.0 - 0.44 + rate)).round() as usize)
                .clamp(1, self.width.max(self.height));
            steps += 1;
            if temp < 0.005 * self.total() / nets || steps > 2000 {
                break;
            }
        }
        // A final greedy pass.
        for _ in 0..moves {
            self.try_move(1e-12, 1);
        }
        steps
    }
}

/// Places the logic of `nl` (`packed`) on the logic tiles of `grid`, the
/// cells in `fixed` staying at their positions.
pub fn place(
    nl: &Netlist,
    packed: &Packed,
    grid: &Grid,
    fixed: &FxHashMap<CellId, (usize, usize)>,
    timing: Option<PlaceTiming>,
    opts: &PlaceOptions,
    log: &mut Vec<String>,
) -> Result<Placement, String> {
    let width = grid.tiles.iter().map(|t| t.0).max().unwrap_or(0) + 1;
    let height = grid.tiles.iter().map(|t| t.1).max().unwrap_or(0) + 1;
    let mut a = Annealer {
        nl,
        packed,
        grid,
        rng: Rng::new(opts.seed),
        occ: vec![Occ::Free; grid.tiles.len() * 8],
        lc_slot: vec![usize::MAX; packed.lcs.len()],
        chain_at: vec![(0, 0); packed.chains.len()],
        block_site: vec![usize::MAX; packed.blocks.len()],
        block_occ: vec![None; grid.blocks.len()],
        cell_unit: vec![None; nl.cells.len()],
        fixed: vec![None; nl.cells.len()],
        xy: vec![(0, 0); nl.cells.len()],
        nets: Vec::new(),
        net_bb: Vec::new(),
        net_cost: Vec::new(),
        cell_nets: vec![Vec::new(); nl.cells.len()],
        width,
        height,
        timing,
        conns: Vec::new(),
        conn_weight: Vec::new(),
        conn_cost: Vec::new(),
        cell_conns: vec![Vec::new(); nl.cells.len()],
        dedicated: Vec::new(),
        lambda: if timing.is_some() { opts.timing_weight.clamp(0.0, 1.0) } else { 0.0 },
        bb_norm: 1.0,
        t_norm: 1.0,
    };
    for (&c, &p) in fixed {
        a.fixed[c.index()] = Some(p);
    }
    for (i, lc) in packed.lcs.iter().enumerate() {
        for c in lc.lut.into_iter().chain(lc.ff) {
            a.cell_unit[c.index()] = Some((Occ::Lc(i as u32), 0));
        }
    }
    for (i, &c) in packed.blocks.iter().enumerate() {
        a.cell_unit[c.index()] = Some((Occ::Block(i as u32), 0));
    }
    for (i, ch) in packed.chains.iter().enumerate() {
        for (j, s) in ch.iter().enumerate() {
            a.cell_unit[s.ccu.index()] = Some((Occ::Chain(i as u32, 0), 2 * j));
            for (h, ff) in s.ff.iter().enumerate() {
                if let Some(ff) = ff {
                    a.cell_unit[ff.index()] = Some((Occ::Chain(i as u32, 0), 2 * j + h));
                }
            }
        }
    }
    // Nets between placed or fixed cells (not the carries inside chains).
    for n in nl.live_nets() {
        let net = &nl.nets[n];
        let mut cells: Vec<CellId> = net
            .driver
            .iter()
            .chain(&net.users)
            .map(|p| p.cell)
            .filter(|c| a.cell_unit[c.index()].is_some() || fixed.contains_key(c))
            .collect();
        cells.sort_unstable();
        cells.dedup();
        if cells.len() < 2 {
            continue;
        }
        let i = a.nets.len() as u32;
        for &c in &cells {
            a.cell_nets[c.index()].push(i);
        }
        a.nets.push(cells);
    }
    // Timing connections between units (those inside one keep their
    // delay); the dedicated ones have none.
    if let Some(t) = timing {
        let placed = |c: CellId| a.cell_unit[c.index()].is_some() || fixed.contains_key(&c);
        let unit = |c: CellId| a.cell_unit[c.index()].map(|u| u.0);
        for (k, c) in t.graph.conns.iter().enumerate() {
            a.dedicated.push(dedicated(nl, c, unit(c.driver), unit(c.sink)));
            let between = unit(c.driver) != unit(c.sink) || unit(c.sink).is_none();
            if placed(c.driver) && placed(c.sink) && between && c.driver != c.sink {
                let i = a.conns.len() as u32;
                a.conns.push((c.driver, c.sink, k as u32));
                for cell in [c.driver, c.sink] {
                    a.cell_conns[cell.index()].push(i);
                }
            }
        }
        a.conn_weight = vec![0.0; a.conns.len()];
        a.conn_cost = vec![0.0; a.conns.len()];
    }
    initial(&mut a)?;
    let all: Vec<CellId> = nl.live_cells().collect();
    a.refresh(&all);
    a.reset_costs();
    let start = a.net_cost.iter().sum::<f64>();
    let steps = a.anneal(opts.effort);
    a.reset_costs();
    log.push(format!(
        "placement: {} logic cells, {} carry chains, {} block RAMs and multipliers, {} nets, wirelength {start:.0} -> {:.0} in {steps} temperature steps",
        packed.lcs.len(),
        packed.chains.len(),
        packed.blocks.len(),
        a.nets.len(),
        a.net_cost.iter().sum::<f64>()
    ));
    let mut p = Placement::default();
    for (u, &s) in a.lc_slot.iter().enumerate() {
        let (x, y) = grid.tiles[s / 8];
        let lc = packed.lcs[u];
        for c in lc.lut.into_iter().chain(lc.ff) {
            p.sites.insert(c, Site { x, y, z: s % 8 });
        }
    }
    for (u, &c) in packed.blocks.iter().enumerate() {
        let (x, y, z, _) = grid.blocks[a.block_site[u]];
        p.sites.insert(c, Site { x, y, z });
    }
    for (ch, slices) in packed.chains.iter().enumerate() {
        let (row, start) = a.chain_at[ch];
        for (i, s) in slices.iter().enumerate() {
            let (t, slice) = a.chain_slice(row, start, i).expect("chain within its row");
            let (x, y) = grid.tiles[t];
            p.sites.insert(s.ccu, Site { x, y, z: 2 * slice });
            for (h, ff) in s.ff.iter().enumerate() {
                if let Some(ff) = ff {
                    p.sites.insert(*ff, Site { x, y, z: 2 * slice + h });
                }
            }
        }
    }
    let _ = a.nl;
    Ok(p)
}

/// Whether a connection is dedicated (no routing, no delay): a carry, or
/// a flip-flop's data from the LUT or sum it shares a unit with (which
/// packing pairs it with).
fn dedicated(nl: &Netlist, c: &Connection, driver: Option<Occ>, sink: Option<Occ>) -> bool {
    let dpin = crate::pack::pin(nl, c.driver, c.driver_pin as u32);
    let spin = crate::pack::pin(nl, c.sink, c.pin as u32);
    (dpin == "COUT" && spin == "CIN")
        || (matches!(dpin, "Z" | "S0" | "S1") && spin == "DI" && driver.is_some() && driver == sink)
}

/// A legal starting placement: block RAMs and multipliers on the sites nearest the
/// device's center, chains along the rows, then logic cells grouped by
/// flip-flop controls into the tiles in order.
fn initial(a: &mut Annealer) -> Result<(), String> {
    // Each kind's sites nearest the center first.
    let (cx, cy) = (a.width / 2, a.height / 2);
    let mut sites: Vec<usize> = (0..a.grid.blocks.len()).collect();
    sites.sort_by_key(|&s| {
        let (x, y, z, _) = a.grid.blocks[s];
        (x.abs_diff(cx) + y.abs_diff(cy), x, y, z)
    });
    for u in 0..a.packed.blocks.len() {
        let kind = a.nl.cell_kind(a.packed.blocks[u]);
        let free = sites
            .iter()
            .copied()
            .find(|&s| a.grid.blocks[s].3 == kind && a.block_occ[s].is_none())
            .ok_or_else(|| {
                let n = a.packed.blocks.iter().filter(|&&c| a.nl.cell_kind(c) == kind).count();
                let m = a.grid.blocks.iter().filter(|b| b.3 == kind).count();
                format!("design does not fit: {n} {kind}, {m} on the device")
            })?;
        a.block_site[u] = free;
        a.block_occ[free] = Some(u as u32);
    }
    let mut rows: Vec<usize> = a.grid.rows.keys().copied().collect();
    rows.sort_unstable();
    // Next free slice index along each row.
    let mut fill: FxHashMap<usize, usize> = FxHashMap::default();
    let mut order: Vec<usize> = (0..a.packed.chains.len()).collect();
    order.sort_by_key(|&c| std::cmp::Reverse(a.packed.chains[c].len()));
    let mut r = 0;
    for ch in order {
        let len = a.packed.chains[ch].len();
        let mut placed = false;
        for k in 0..rows.len() {
            let row = rows[(r + k) % rows.len()];
            let start = fill.get(&row).copied().unwrap_or(0);
            if start + len <= a.grid.rows[&row].len() * 4 {
                a.chain_at[ch] = (row, start);
                for (i, s) in a.chain_slots(ch, row, start).expect("fits").into_iter().enumerate() {
                    a.occ[s] = Occ::Chain(ch as u32, (i / 2) as u32);
                }
                // Chains start on slice boundaries of their own tiles.
                fill.insert(row, (start + len).div_ceil(4) * 4);
                r = (r + k + 1) % rows.len();
                placed = true;
                break;
            }
        }
        if !placed {
            return Err(format!("no room for a carry chain of {len} slices"));
        }
    }
    // Logic cells with flip-flops by their controls, then the others.
    let mut order: Vec<u32> = (0..a.packed.lcs.len() as u32).collect();
    let key = |u: &u32| {
        let lc = a.packed.lcs[*u as usize];
        lc.ff
            .map(|f| (a.packed.ff_tile[&f], a.packed.ff_ce[&f].map_or(0, |n| n.index() + 1)))
            .map_or((1, 0, 0), |(t, c)| (0, t, c))
    };
    order.sort_by_key(key);
    let mut s = 0usize;
    for u in order {
        let mut placed = false;
        for _ in 0..a.occ.len() {
            if a.occ[s] == Occ::Free {
                a.occ[s] = Occ::Lc(u);
                a.lc_slot[u as usize] = s;
                if a.tile_ok(s / 8) {
                    placed = true;
                    break;
                }
                a.occ[s] = Occ::Free;
            }
            s = (s + 1) % a.occ.len();
        }
        if !placed {
            return Err("design does not fit: no legal slot for a logic cell".into());
        }
    }
    Ok(())
}
