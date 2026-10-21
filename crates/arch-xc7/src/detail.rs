//! Detailed placement: once slices are placed, lanes (a LUT site with its
//! two flip-flops) move between nearby slices, greedily, while the placer's
//! cost improves. Packing decides slice contents before anything is
//! placed; a LUT that joined a slice only for its free site can then sit
//! far from its readers, and slices move only as a whole. nextpnr-xilinx
//! places LUTs and flip-flops individually for the same reason.
//!
//! A lane moves with its LUT6, its O5 LUT and both flip-flops, so the
//! connections inside it stay inside it. Lanes under a MUXF7 or MUXF8,
//! slices with LUT RAM or shift registers, and carry lanes stay put; a
//! carry slice's lanes above its last used carry stage take a LUT and the
//! flip-flop it feeds. Flip-flops join a slice only with its control set.
use crate::{
    device::Device,
    pack::{Controls, Packed},
    place::Placement,
};
use std::collections::BTreeSet;
use synth_core::FxHashMap;
use synth_core::rng::Rng;
use synth_netlist::{CellId, NetId, Netlist};

/// Criticalities of each reader of each net for the current slice
/// contents, from a timing analysis of the placement.
pub type Retime<'a> = &'a mut dyn FnMut(&Packed) -> FxHashMap<(CellId, NetId), f32>;

/// Tiles a lane may move by, in x and y.
const WINDOW: i32 = 3;
/// Passes over every lane, with criticalities updated every few.
const PASSES: usize = 8;
const RETIME_PASSES: usize = 2;
/// Target slices tried per lane and pass.
const TRIES: usize = 4;

/// The cells of a lane: LUT6, O5 LUT and the two flip-flops.
#[derive(Clone, Copy, Default)]
struct Lane {
    lut: Option<CellId>,
    lut5: Option<CellId>,
    ff: Option<CellId>,
    ff5: Option<CellId>,
}
impl Lane {
    fn get(p: &Packed, s: usize, l: usize) -> Lane {
        let x = &p.slices[s];
        Lane { lut: x.luts[l], lut5: x.lut5[l], ff: x.ffs[l], ff5: x.ffs[l + 4] }
    }
    fn cells(&self) -> impl Iterator<Item = CellId> {
        [self.lut, self.lut5, self.ff, self.ff5].into_iter().flatten()
    }
    fn has_ffs(&self) -> bool {
        self.ff.is_some() || self.ff5.is_some()
    }
}

/// The lanes whose contents may move, per slice.
fn movable(nl: &Netlist, p: &Packed) -> Vec<[bool; 4]> {
    p.slices
        .iter()
        .map(|s| {
            if s.ram.is_some() || s.memory.is_some() || s.f8.is_some() || s.needs_m(nl) {
                return [false; 4];
            }
            // A carry's stages up to its last used output stay with it.
            let live = s.carry.map_or(0, |c| {
                (0..4)
                    .filter(|&l| {
                        [format!("O[{l}]"), format!("CO[{l}]")].iter().any(|pin| {
                            nl.pin_net(c, pin).is_some_and(|n| !nl.nets[n].users.is_empty())
                        })
                    })
                    .map(|l| l + 1)
                    .max()
                    .unwrap_or(4)
            });
            let carry_nets: BTreeSet<NetId> = s
                .carry
                .iter()
                .flat_map(|&c| nl.cells[c].pins.iter().filter_map(|pin| pin.net))
                .collect();
            std::array::from_fn(|l| {
                s.f7[l / 2].is_none()
                    && l >= live
                    && [s.luts[l], s.lut5[l], s.ffs[l], s.ffs[l + 4]].into_iter().flatten().all(
                        |c| {
                            !nl.cells[c]
                                .pins
                                .iter()
                                .filter_map(|pin| pin.net)
                                .any(|n| carry_nets.contains(&n))
                        },
                    )
            })
        })
        .collect()
}

/// Whether `lane`, whose flip-flops have controls `ctrl`, may replace lane
/// `l` of slice `s`.
fn fits(
    nl: &Netlist,
    p: &Packed,
    s: usize,
    l: usize,
    lane: &Lane,
    ctrl: Option<&Controls>,
) -> bool {
    let x = &p.slices[s];
    if x.carry.is_some() {
        // Beside a carry: no second bank, no O5 LUT and no X bypass, so
        // only a flip-flop fed by the lane's own LUT.
        let own = lane.lut.and_then(|c| nl.pin_net(c, "O"));
        if lane.lut5.is_some()
            || lane.ff5.is_some()
            || lane.ff.is_some_and(|f| nl.pin_net(f, "D").is_none() || nl.pin_net(f, "D") != own)
        {
            return false;
        }
    }
    let others = (0..8).filter(|&k| k != l && k != l + 4).any(|k| x.ffs[k].is_some());
    !lane.has_ffs() || !others || x.controls.as_ref() == ctrl
}

pub fn refine(
    db: &Device,
    nl: &Netlist,
    p: &mut Packed,
    pl: &Placement,
    seed: u64,
    retime: Retime,
) -> Result<usize, String> {
    let movable = movable(nl, p);
    let tile_of = |tile: usize| (i32::from(db.tiles[tile].x), i32::from(db.tiles[tile].y));
    let slice_xy: Vec<(i32, i32)> = pl.slices.iter().map(|s| tile_of(s.tile)).collect();
    let mut fixed: FxHashMap<CellId, (i32, i32)> = FxHashMap::default();
    for (&c, site) in pl.rams.iter().chain(&pl.dsps).chain(&pl.ports) {
        fixed.insert(c, tile_of(site.tile));
    }
    let clocks: BTreeSet<NetId> = p
        .slices
        .iter()
        .flat_map(|s| {
            s.controls
                .as_ref()
                .map(|c| c.clock)
                .into_iter()
                .chain(s.memory.as_ref().map(|m| m.clock))
        })
        .collect();
    // Nets reaching a movable cell: their cells, driver first if any.
    let mut moving: BTreeSet<CellId> = BTreeSet::new();
    for (s, lanes) in movable.iter().enumerate() {
        for (l, _) in lanes.iter().enumerate().filter(|(_, m)| **m) {
            moving.extend(Lane::get(p, s, l).cells());
        }
    }
    let mut nets: Vec<(NetId, Option<CellId>, Vec<CellId>)> = Vec::new();
    let mut of_cell: FxHashMap<CellId, Vec<usize>> = FxHashMap::default();
    for n in nl.live_nets() {
        let net = &nl.nets[n];
        if clocks.contains(&n) || net.constant.is_some() || net.users.len() > 100 {
            continue;
        }
        let mut users: Vec<CellId> = net.users.iter().map(|u| u.cell).collect();
        users.sort_unstable();
        users.dedup();
        let driver = net.driver.map(|d| d.cell);
        users.retain(|&u| Some(u) != driver);
        if !driver.iter().chain(&users).any(|c| moving.contains(c)) {
            continue;
        }
        let i = nets.len();
        for &c in driver.iter().chain(&users) {
            if moving.contains(&c) {
                of_cell.entry(c).or_default().push(i);
            }
        }
        nets.push((n, driver, users));
    }
    for list in of_cell.values_mut() {
        list.dedup();
    }
    let place = |p: &Packed, c: CellId| -> Option<(usize, (i32, i32))> {
        match p.slots.get(&c) {
            Some(&(s, _)) => Some((s, slice_xy[s])),
            None => fixed.get(&c).map(|&xy| (usize::MAX, xy)),
        }
    };
    // Criticality weights of each net's readers, and the two costs'
    // normalizations, refreshed every few passes.
    let mut weights: Vec<Vec<f64>> = vec![Vec::new(); nets.len()];
    let (mut wl_scale, mut td_scale) = (1.0, 0.0);
    let hpwl = |p: &Packed, i: usize| -> f64 {
        let (_, driver, users) = &nets[i];
        let mut b = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for &c in driver.iter().chain(users) {
            if let Some((_, (x, y))) = place(p, c) {
                b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
            }
        }
        if b.0 > b.2 { 0.0 } else { f64::from(b.2 - b.0 + b.3 - b.1) }
    };
    let delay = |p: &Packed, weights: &[Vec<f64>], i: usize| -> f64 {
        let (_, driver, users) = &nets[i];
        let Some((ds, (dx, dy))) = driver.and_then(|d| place(p, d)) else { return 0.0 };
        users
            .iter()
            .zip(&weights[i])
            .filter(|(_, w)| **w > 0.0)
            .filter_map(|(&u, &w)| {
                let (s, (x, y)) = place(p, u)?;
                let d = if s == ds && s != usize::MAX {
                    0.0
                } else if (x, y) == (dx, dy) {
                    150.0
                } else {
                    crate::sta::predicted_delay(x - dx, y - dy)
                };
                Some(w * d)
            })
            .sum()
    };
    let mut by_tile: std::collections::BTreeMap<(i32, i32), Vec<usize>> = Default::default();
    for (s, m) in movable.iter().enumerate() {
        if m.iter().any(|&x| x) {
            by_tile.entry(slice_xy[s]).or_default().push(s);
        }
    }
    let near = |(x, y): (i32, i32), w: i32| -> Vec<usize> {
        by_tile
            .range((x - w, i32::MIN)..=(x + w, i32::MAX))
            .filter(|((_, ty), _)| (ty - y).abs() <= w)
            .flat_map(|(_, v)| v.iter().copied())
            .collect()
    };
    let neighbours: Vec<Vec<usize>> = (0..p.slices.len())
        .map(|s| near(slice_xy[s], WINDOW).into_iter().filter(|&t| t != s).collect())
        .collect();
    let mut rng = Rng::new(seed ^ 0x6465_7461_696c);
    let mut moves = 0;
    for pass in 0..PASSES {
        if pass % RETIME_PASSES == 0 {
            let crit = retime(p);
            for (i, (n, _, users)) in nets.iter().enumerate() {
                weights[i] = users
                    .iter()
                    .map(|&u| f64::from(crit.get(&(u, *n)).copied().unwrap_or(0.0)).powi(8))
                    .collect();
            }
            let wl: f64 = (0..nets.len()).map(|i| hpwl(p, i)).sum();
            let td: f64 = (0..nets.len()).map(|i| delay(p, &weights, i)).sum();
            wl_scale = (1.0 - crate::place::TIMING_TRADEOFF) / wl.max(1.0);
            td_scale = if td > 0.0 { crate::place::TIMING_TRADEOFF / td } else { 0.0 };
        }
        let cost = |p: &Packed, list: &[usize]| -> f64 {
            list.iter().map(|&i| wl_scale * hpwl(p, i) + td_scale * delay(p, &weights, i)).sum()
        };
        let mut order: Vec<(usize, usize)> = movable
            .iter()
            .enumerate()
            .flat_map(|(s, m)| (0..4).filter(move |&l| m[l]).map(move |l| (s, l)))
            .collect();
        rng.shuffle(&mut order);
        for (s, l) in order {
            let a = Lane::get(p, s, l);
            if a.cells().next().is_none() || neighbours[s].is_empty() {
                continue;
            }
            // The criticality-weighted centre of the lane's connections.
            let toward: Vec<usize> = {
                let cells: Vec<CellId> = a.cells().collect();
                let (mut sx, mut sy, mut sw) = (0.0, 0.0, 0.0);
                let mut add = |c: CellId, w: f64| {
                    if let Some((_, (x, y))) = place(p, c) {
                        let w = 1.0 + 10.0 * w;
                        (sx, sy, sw) = (sx + w * f64::from(x), sy + w * f64::from(y), sw + w);
                    }
                };
                let mut list: Vec<usize> = cells
                    .iter()
                    .flat_map(|c| of_cell.get(c).into_iter().flatten().copied())
                    .collect();
                list.sort_unstable();
                list.dedup();
                for i in list {
                    let (_, driver, users) = &nets[i];
                    let inside_driver = driver.is_some_and(|d| cells.contains(&d));
                    for (k, &u) in users.iter().enumerate() {
                        let w = weights[i].get(k).copied().unwrap_or(0.0);
                        if inside_driver && !cells.contains(&u) {
                            add(u, w);
                        } else if cells.contains(&u)
                            && let Some(d) = driver.filter(|d| !cells.contains(d))
                        {
                            add(d, w);
                        }
                    }
                }
                if sw > 0.0 {
                    let c = ((sx / sw).round() as i32, (sy / sw).round() as i32);
                    near(c, 1).into_iter().filter(|&t| t != s).collect()
                } else {
                    Vec::new()
                }
            };
            for k in 0..TRIES {
                let t = if k % 2 == 0 && !toward.is_empty() {
                    toward[rng.below(toward.len())]
                } else {
                    neighbours[s][rng.below(neighbours[s].len())]
                };
                let j = rng.below(4);
                if !movable[t][j] {
                    continue;
                }
                let b = Lane::get(p, t, j);
                let a_in = (a, p.slices[s].controls.clone());
                let b_in = (b, p.slices[t].controls.clone());
                if !fits(nl, p, t, j, &a, a_in.1.as_ref())
                    || !fits(nl, p, s, l, &b, b_in.1.as_ref())
                {
                    continue;
                }
                let mut list: Vec<usize> = a
                    .cells()
                    .chain(b.cells())
                    .flat_map(|c| of_cell.get(&c).into_iter().flatten().copied())
                    .collect();
                list.sort_unstable();
                list.dedup();
                let before = cost(p, &list);
                swap(p, (s, l, &b_in), (t, j, &a_in));
                if cost(p, &list) < before - 1e-12 {
                    moves += 1;
                    break;
                }
                swap(p, (s, l, &a_in), (t, j, &b_in));
            }
        }
    }
    Ok(moves)
}

/// Puts `into_s` at lane `l` of slice `s` and `into_t` at lane `j` of
/// slice `t`, each with the controls of the slice it came from.
fn swap(
    p: &mut Packed,
    (s, l, into_s): (usize, usize, &(Lane, Option<Controls>)),
    (t, j, into_t): (usize, usize, &(Lane, Option<Controls>)),
) {
    for (s, l, (x, ctrl)) in [(s, l, into_s), (t, j, into_t)] {
        let slice = &mut p.slices[s];
        (slice.luts[l], slice.lut5[l], slice.ffs[l], slice.ffs[l + 4]) =
            (x.lut, x.lut5, x.ff, x.ff5);
        // A slice keeps its controls while its other flip-flops stay.
        if (0..8).filter(|&k| k != l && k != l + 4).all(|k| slice.ffs[k].is_none()) {
            slice.controls = if x.has_ffs() { ctrl.clone() } else { None };
        }
        for c in [x.lut, x.lut5, x.ff].into_iter().flatten() {
            p.slots.insert(c, (s, l));
        }
        if let Some(c) = x.ff5 {
            p.slots.insert(c, (s, l + 4));
        }
    }
}
