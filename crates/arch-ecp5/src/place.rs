//! Placement of logic cells and carry chains by simulated annealing.
//!
//! Units are logic cells (a LUT and/or flip-flop, one of a tile's eight
//! slots) and carry chains (consecutive slices of a row, moved whole); I/O
//! cells are fixed to their pins. The cost is each net's bounding box half
//! perimeter, weighted for nets of many cells as VPR weights them, under a
//! VPR schedule: the temperature falls and the move window narrows by the
//! share of moves accepted. A tile is legal when its flip-flops share one
//! clock and set/reset, and each slice's flip-flops one clock enable.

use synth_core::{FxHashMap, Rng};
use synth_netlist::{CellId, Netlist};

use crate::pack::Packed;

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
}

impl Default for PlaceOptions {
    fn default() -> Self {
        PlaceOptions { seed: 1, effort: 1.0 }
    }
}

/// What occupies a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Occ {
    Free,
    Lc(u32),
    /// A chain and which of its slices.
    Chain(u32, u32),
}

/// The logic tiles of a device: positions, and the rows' runs of
/// adjacent tiles that carry chains run along.
#[derive(Clone, Debug)]
pub struct Grid {
    pub tiles: Vec<(usize, usize)>,
    at: FxHashMap<(usize, usize), usize>,
    /// Each row's tiles, west to east (all adjacent).
    rows: FxHashMap<usize, Vec<usize>>,
}

impl Grid {
    pub fn new(mut tiles: Vec<(usize, usize)>) -> Grid {
        tiles.sort_by_key(|&(x, y)| (y, x));
        let at = tiles.iter().enumerate().map(|(i, &p)| (p, i)).collect();
        let mut rows: FxHashMap<usize, Vec<usize>> = FxHashMap::default();
        for (i, &(_, y)) in tiles.iter().enumerate() {
            rows.entry(y).or_default().push(i);
        }
        Grid { tiles, at, rows }
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
    /// Each cell's unit: `Occ::Lc` or `Occ::Chain`, and its offset in a
    /// chain (slices times two plus half).
    cell_unit: FxHashMap<CellId, (Occ, usize)>,
    fixed: FxHashMap<CellId, (usize, usize)>,
    /// Each net's cells, and each cell's nets.
    nets: Vec<Vec<CellId>>,
    cell_nets: FxHashMap<CellId, Vec<u32>>,
    net_cost: Vec<f64>,
    width: usize,
    height: usize,
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

    fn pos(&self, c: CellId) -> (usize, usize) {
        if let Some(&p) = self.fixed.get(&c) {
            return p;
        }
        match self.cell_unit.get(&c) {
            Some(&(Occ::Lc(u), _)) => self.grid.tiles[self.lc_slot[u as usize] / 8],
            Some(&(Occ::Chain(ch, _), off)) => {
                let (row, start) = self.chain_at[ch as usize];
                let (t, _) = self.chain_slice(row, start, off / 2).expect("chain within its row");
                self.grid.tiles[t]
            }
            _ => (0, 0),
        }
    }

    fn net_bb_cost(&self, n: usize) -> f64 {
        let cells = &self.nets[n];
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        for &c in cells {
            let (x, y) = self.pos(c);
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
        net_weight(cells.len()) * ((x1 - x0) + (y1 - y0)) as f64
    }

    /// The flip-flops of a slot.
    fn slot_ff(&self, s: usize) -> Option<CellId> {
        match self.occ[s] {
            Occ::Lc(u) => self.packed.lcs[u as usize].ff,
            Occ::Chain(ch, i) => self.packed.chains[ch as usize][i as usize].ff[s % 2],
            Occ::Free => None,
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
        let mut nets: Vec<u32> = cells
            .iter()
            .flat_map(|c| self.cell_nets.get(c).into_iter().flatten().copied())
            .collect();
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
            Occ::Free => Vec::new(),
        }
    }

    /// Tries moving logic cell `u` to slot `to` (swapping with what is
    /// there); returns the cost change if legal, with the move applied.
    fn move_lc(&mut self, u: u32, to: usize) -> Option<(Vec<u32>, f64, usize)> {
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
        let nets = self.touched_nets(&cells);
        let delta =
            nets.iter().map(|&n| self.net_bb_cost(n as usize) - self.net_cost[n as usize]).sum();
        Some((nets, delta, from))
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
    /// there taking the slots it leaves.
    #[allow(clippy::type_complexity)]
    fn move_chain(
        &mut self,
        ch: usize,
        row: usize,
        start: usize,
    ) -> Option<(Vec<u32>, f64, Vec<(usize, Occ)>)> {
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
        let nets = self.touched_nets(&cells);
        let delta =
            nets.iter().map(|&n| self.net_bb_cost(n as usize) - self.net_cost[n as usize]).sum();
        Some((nets, delta, saved))
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

    /// One move at temperature `temp` within `rlim` tiles; returns whether
    /// it was accepted (`None` if illegal or impossible).
    fn try_move(&mut self, temp: f64, rlim: usize) -> Option<bool> {
        let lcs = self.packed.lcs.len();
        let chains = self.packed.chains.len();
        let pick = self.rng.below(lcs + chains);
        let decide = |rng: &mut Rng, delta: f64| delta <= 0.0 || rng.unit() < (-delta / temp).exp();
        if pick < lcs {
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
            let (nets, delta, from) = self.move_lc(u, to)?;
            if decide(&mut self.rng, delta) {
                for n in nets {
                    self.net_cost[n as usize] = self.net_bb_cost(n as usize);
                }
                Some(true)
            } else {
                self.undo_lc(u, from, to);
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
            let (nets, delta, saved) = self.move_chain(ch, nrow, nstart)?;
            if decide(&mut self.rng, delta) {
                for n in nets {
                    self.net_cost[n as usize] = self.net_bb_cost(n as usize);
                }
                Some(true)
            } else {
                self.restore(&saved, ch, (row, start));
                Some(false)
            }
        }
    }

    fn total(&self) -> f64 {
        self.net_cost.iter().sum()
    }

    fn anneal(&mut self, effort: f64) -> usize {
        let units = (self.packed.lcs.len() + self.packed.chains.len()).max(1) as f64;
        let moves = ((effort * units.powf(4.0 / 3.0)) as usize).max(100);
        let nets = self.nets.len().max(1) as f64;
        let mut rlim = self.width.max(self.height);
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
        cell_unit: FxHashMap::default(),
        fixed: fixed.clone(),
        nets: Vec::new(),
        cell_nets: FxHashMap::default(),
        net_cost: Vec::new(),
        width,
        height,
    };
    for (i, lc) in packed.lcs.iter().enumerate() {
        for c in lc.lut.into_iter().chain(lc.ff) {
            a.cell_unit.insert(c, (Occ::Lc(i as u32), 0));
        }
    }
    for (i, ch) in packed.chains.iter().enumerate() {
        for (j, s) in ch.iter().enumerate() {
            a.cell_unit.insert(s.ccu, (Occ::Chain(i as u32, 0), 2 * j));
            for (h, ff) in s.ff.iter().enumerate() {
                if let Some(ff) = ff {
                    a.cell_unit.insert(*ff, (Occ::Chain(i as u32, 0), 2 * j + h));
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
            .filter(|c| a.cell_unit.contains_key(c) || fixed.contains_key(c))
            .collect();
        cells.sort_unstable();
        cells.dedup();
        if cells.len() < 2 {
            continue;
        }
        let i = a.nets.len() as u32;
        for &c in &cells {
            a.cell_nets.entry(c).or_default().push(i);
        }
        a.nets.push(cells);
    }
    initial(&mut a)?;
    a.net_cost = (0..a.nets.len()).map(|n| a.net_bb_cost(n)).collect();
    let start = a.total();
    let steps = a.anneal(opts.effort);
    a.net_cost = (0..a.nets.len()).map(|n| a.net_bb_cost(n)).collect();
    log.push(format!(
        "placement: {} logic cells, {} carry chains, {} nets, wirelength {start:.0} -> {:.0} in {steps} temperature steps",
        packed.lcs.len(),
        packed.chains.len(),
        a.nets.len(),
        a.total()
    ));
    let mut p = Placement::default();
    for (u, &s) in a.lc_slot.iter().enumerate() {
        let (x, y) = grid.tiles[s / 8];
        let lc = packed.lcs[u];
        for c in lc.lut.into_iter().chain(lc.ff) {
            p.sites.insert(c, Site { x, y, z: s % 8 });
        }
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

/// A legal starting placement: chains along the rows, then logic cells
/// grouped by flip-flop controls into the tiles in order.
fn initial(a: &mut Annealer) -> Result<(), String> {
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
