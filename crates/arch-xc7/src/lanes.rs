//! Lane order within slices. A slice output reaches six of its own tile's
//! 48 LUT inputs through a single pip (about 180 ps plus the pin's delay),
//! and the others through two or more (350-650 ps); which six depends on
//! the output's lane and slice. Packing and detailed placement choose a
//! connection's tile but not its lanes, so after them each slice's movable
//! lanes are reordered, and trade places with the other slice's, for the
//! critical connections within its tile: the order that leaves the least
//! criticality-weighted delay into its tile's LUTs, each lane's inputs
//! taking its fastest free pins.
use crate::device::Device;
use crate::pack::{self, Packed};
use crate::place::{Placement, Site};
use crate::routing::{RoutingGraph, VIRTUAL};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use synth_core::FxHashMap;
use synth_netlist::{CellId, NetId, Netlist};

/// Delays (ps) of the LUT input pins A1-A6 past their input multiplexers.
const PIN: [f64; 6] = [515.0, 494.0, 340.0, 275.0, 114.0, 0.0];
/// Paths searched within a tile (and the tiles beside it), and the delay
/// of the pins none reaches.
const REACH: u32 = 1200;
/// Passes over each tile's slices.
const PASSES: usize = 2;

/// The fastest delays (ps) from each output of a tile's slices to each LUT
/// input of its slices, by (source slice, output) and then by destination
/// slice, lane and pin.
type Table = FxHashMap<(usize, String), Vec<[[f64; 6]; 4]>>;

fn outputs() -> impl Iterator<Item = String> {
    ['A', 'B', 'C', 'D'].into_iter().flat_map(|l| ["", "MUX", "Q"].map(|s| format!("{l}{s}")))
}

fn table(db: &Device, graph: &RoutingGraph, tile: usize, slices: usize) -> Table {
    let site = |index| Site { tile, index, name: String::new(), x: 0, y: 0 };
    let mut table = Table::default();
    for src in 0..slices {
        for out in outputs() {
            let Ok(start) = site(src).node(db, &out) else { continue };
            let (x, y) = (graph.x[start as usize], graph.y[start as usize]);
            let near = |m: u32| {
                (graph.x[m as usize] - x).abs() <= 1 && (graph.y[m as usize] - y).abs() <= 1
            };
            let mut dist: FxHashMap<u32, u32> = FxHashMap::default();
            let mut heap = BinaryHeap::new();
            dist.insert(start, 0);
            heap.push(Reverse((0u32, start)));
            while let Some(Reverse((d, n))) = heap.pop() {
                if d > dist[&n] {
                    continue;
                }
                for e in graph.offsets[n as usize]..graph.offsets[n as usize + 1] {
                    if graph.pip[e as usize].0 == VIRTUAL {
                        continue;
                    }
                    let m = graph.dst[e as usize];
                    let nd = d + graph.delay[e as usize].round() as u32;
                    if nd <= REACH && near(m) && dist.get(&m).is_none_or(|&old| nd < old) {
                        dist.insert(m, nd);
                        heap.push(Reverse((nd, m)));
                    }
                }
            }
            let to = (0..slices)
                .map(|dst| {
                    std::array::from_fn(|l| {
                        std::array::from_fn(|i| {
                            let pin = format!("{}{}", char::from(b'A' + l as u8), i + 1);
                            site(dst)
                                .node(db, &pin)
                                .ok()
                                .and_then(|n| dist.get(&n))
                                .map_or(f64::from(REACH), |&d| f64::from(d))
                        })
                    })
                })
                .collect();
            table.insert((src, out), to);
        }
    }
    table
}

/// The delay tables of the tile templates met so far.
pub struct Tables<'a> {
    db: &'a Device,
    graph: &'a RoutingGraph,
    tables: std::cell::RefCell<FxHashMap<usize, std::rc::Rc<Table>>>,
}

impl<'a> Tables<'a> {
    pub fn new(db: &'a Device, graph: &'a RoutingGraph) -> Self {
        Tables { db, graph, tables: Default::default() }
    }

    fn of(&self, tile: usize) -> std::rc::Rc<Table> {
        let template = self.db.tiles[tile].template;
        let count = self.db.templates[template].sites.len();
        let mut tables = self.tables.borrow_mut();
        tables
            .entry(template)
            .or_insert_with(|| std::rc::Rc::new(table(self.db, self.graph, tile, count)))
            .clone()
    }
}

/// The output of its slice a cell drives its net through.
fn output(nl: &Netlist, p: &Packed, c: CellId) -> Option<(usize, String)> {
    let &(s, slot) = p.slots.get(&c)?;
    let lane = char::from(b'A' + (slot % 4) as u8);
    let k = nl.cell_kind(c);
    let suffix = if pack::is_lut(k) {
        if p.slices[s].lut5[slot % 4] == Some(c) { "MUX" } else { "" }
    } else if pack::is_ff(k) {
        if slot >= 4 { "MUX" } else { "Q" }
    } else {
        return None;
    };
    Some((s, format!("{lane}{suffix}")))
}

/// The cells of lane `l` of slice `s`: LUT6, O5 LUT and both flip-flops.
type Lane = [Option<CellId>; 4];

fn get(p: &Packed, s: usize, l: usize) -> Lane {
    let x = &p.slices[s];
    [x.luts[l], x.lut5[l], x.ffs[l], x.ffs[l + 4]]
}

fn set(p: &mut Packed, s: usize, l: usize, lane: Lane) {
    let x = &mut p.slices[s];
    [x.luts[l], x.lut5[l], x.ffs[l], x.ffs[l + 4]] = lane;
    for c in lane[..3].iter().flatten() {
        p.slots.insert(*c, (s, l));
    }
    if let Some(c) = lane[3] {
        p.slots.insert(c, (s, l + 4));
    }
}

/// Reorders the movable lanes of each slice for the connections within
/// its tile, weighted by criticality. The tiles reordered.
pub fn order(
    tables: &Tables,
    nl: &Netlist,
    p: &mut Packed,
    pl: &Placement,
    crit: &FxHashMap<(CellId, NetId), f32>,
) -> usize {
    let movable = crate::detail::movable(nl, p);
    let mut tiles: FxHashMap<usize, Vec<usize>> = FxHashMap::default();
    for (s, site) in pl.slices.iter().enumerate() {
        tiles.entry(site.tile).or_default().push(s);
    }
    let weight = |c: CellId, n: NetId| f64::from(crit.get(&(c, n)).copied().unwrap_or(0.0)).powi(8);
    let mut reordered = 0;
    let mut keys: Vec<usize> = tiles.keys().copied().collect();
    keys.sort_unstable();
    for tile in keys {
        let slices = &tiles[&tile];
        if !slices.iter().any(|&s| movable[s].iter().any(|&m| m)) {
            continue;
        }
        // Only tiles with a critical connection within them.
        let critical = slices.iter().any(|&s| {
            (0..4).any(|l| {
                let x = &p.slices[s];
                x.luts[l].iter().chain(&x.lut5[l]).any(|&c| {
                    nl.cells[c].pins.iter().filter_map(|pin| pin.net).any(|n| {
                        weight(c, n) > 1e-3
                            && nl.nets[n].driver.is_some_and(|d| {
                                p.slots
                                    .get(&d.cell)
                                    .is_some_and(|&(t, _)| pl.slices[t].tile == tile)
                            })
                    })
                })
            })
        });
        if !critical {
            continue;
        }
        let table = tables.of(tile);
        // Each LUT lane's inputs, by criticality, with their drivers, and
        // whether its O5 LUT holds A6: they move with the lane.
        type Inputs = (Vec<(f64, Option<CellId>)>, bool);
        let mut inputs: FxHashMap<CellId, Inputs> = FxHashMap::default();
        for &s in slices {
            let x = &p.slices[s];
            for l in 0..4 {
                let Some(&key) = x.luts[l].iter().chain(&x.lut5[l]).next() else { continue };
                if !crate::flow::permutable(nl, x, l) {
                    continue;
                }
                let cells: Vec<CellId> = x.luts[l].iter().chain(&x.lut5[l]).copied().collect();
                let mut list: Vec<(f64, Option<CellId>)> = x
                    .lane_inputs(nl, l)
                    .into_iter()
                    .map(|n| {
                        let w = cells.iter().map(|&c| weight(c, n)).fold(0.0, f64::max);
                        (w, nl.nets[n].driver.map(|d| d.cell))
                    })
                    .collect();
                list.sort_by(|a, b| b.0.total_cmp(&a.0));
                inputs.insert(key, (list, x.lut5[l].is_some()));
            }
        }
        let cost = |p: &Packed| -> f64 {
            let mut total = 0.0;
            for &s in slices {
                let x = &p.slices[s];
                let index = pl.slices[s].index;
                for l in 0..4 {
                    let Some(key) = x.luts[l].iter().chain(&x.lut5[l]).next() else { continue };
                    let Some((list, o5)) = inputs.get(key) else { continue };
                    // The most critical input first takes its fastest pin;
                    // an O5 LUT holds A6.
                    let mut free = [true, true, true, true, true, !o5];
                    for &(w, driver) in list {
                        let from = driver.and_then(|d| output(nl, p, d));
                        let d = match from {
                            Some((t, out)) if pl.slices[t].tile == tile => {
                                table.get(&(pl.slices[t].index, out)).map_or(PIN, |to| to[index][l])
                            }
                            _ => PIN,
                        };
                        let pick =
                            (0..6).filter(|&j| free[j]).min_by(|&a, &b| d[a].total_cmp(&d[b]));
                        if let Some(j) = pick {
                            free[j] = false;
                            total += w * d[j];
                        }
                    }
                }
            }
            total
        };
        let mut best = cost(p);
        let mut changed = false;
        for _ in 0..PASSES {
            for &s in slices {
                let lanes: Vec<usize> = (0..4).filter(|&l| movable[s][l]).collect();
                if lanes.len() < 2 {
                    continue;
                }
                let start: Vec<Lane> = lanes.iter().map(|&l| get(p, s, l)).collect();
                let mut keep: Vec<usize> = (0..lanes.len()).collect();
                let mut order: Vec<usize> = keep.clone();
                // Every order of the slice's movable lanes (at most 24).
                loop {
                    for (i, &l) in lanes.iter().enumerate() {
                        set(p, s, l, start[order[i]]);
                    }
                    let c = cost(p);
                    if c < best - 1e-9 {
                        best = c;
                        keep = order.clone();
                    }
                    if !next_permutation(&mut order) {
                        break;
                    }
                }
                for (i, &l) in lanes.iter().enumerate() {
                    set(p, s, l, start[keep[i]]);
                }
                changed |= keep.iter().enumerate().any(|(i, &k)| i != k);
            }
            // Lanes trade places between the tile's slices where their
            // flip-flops' controls allow.
            for (a, &s) in slices.iter().enumerate() {
                for &t in &slices[a + 1..] {
                    for l in (0..4).filter(|&l| movable[s][l]) {
                        for j in (0..4).filter(|&j| movable[t][j]) {
                            let x = crate::detail::Lane::get(p, s, l);
                            let y = crate::detail::Lane::get(p, t, j);
                            let x_in = (x, p.slices[s].controls.clone());
                            let y_in = (y, p.slices[t].controls.clone());
                            if !crate::detail::fits(nl, p, t, j, &x, x_in.1.as_ref())
                                || !crate::detail::fits(nl, p, s, l, &y, y_in.1.as_ref())
                            {
                                continue;
                            }
                            crate::detail::swap(p, (s, l, &y_in), (t, j, &x_in));
                            let c = cost(p);
                            if c < best - 1e-9 {
                                best = c;
                                changed = true;
                            } else {
                                crate::detail::swap(p, (s, l, &x_in), (t, j, &y_in));
                            }
                        }
                    }
                }
            }
        }
        reordered += usize::from(changed);
    }
    reordered
}

fn next_permutation(v: &mut [usize]) -> bool {
    let Some(i) = (1..v.len()).rev().find(|&i| v[i - 1] < v[i]) else { return false };
    let j = (i..v.len()).rev().find(|&j| v[j] > v[i - 1]).expect("a larger element");
    v.swap(i - 1, j);
    v[i..].reverse();
    true
}
