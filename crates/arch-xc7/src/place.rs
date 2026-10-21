//! Deterministic placement by annealing HPWL (VPR's adaptive schedule and
//! move window): slice macros with legal carry chains, and block RAMs and
//! DSPs on sites of their kind.
use crate::{
    device::{Device, xy},
    pack::Packed,
    xdc::Constraints,
};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use synth_core::FxHashMap;
use synth_core::rng::Rng;
use synth_netlist::{CellId, NetId, Netlist};

#[derive(Clone, Debug)]
pub struct Site {
    pub tile: usize,
    pub index: usize,
    pub name: String,
    pub x: u32,
    pub y: u32,
}
impl Site {
    pub fn node(&self, db: &Device, pin: &str) -> Result<u32, String> {
        let t = &db.tiles[self.tile];
        let s = &db.templates[t.template].sites[self.index];
        if let Some(w) = s.pins.get(pin) {
            return Ok(t.nodes[*w as usize]);
        }
        if s.kind == "FIFO18E1" {
            return db.wire(self.tile, &format!("BRAM_FIFO18_{pin}"));
        }
        Err(format!("{} has no site pin {pin}", self.name))
    }
    pub fn prefix(&self, db: &Device) -> String {
        let t = &db.tiles[self.tile];
        let s = &db.templates[t.template].sites[self.index];
        format!("{}.{}_X{}", t.name, s.kind, xy(&s.name).unwrap().0)
    }
}
#[derive(Clone, Debug)]
pub struct Placement {
    pub slices: Vec<Site>,
    pub ports: BTreeMap<CellId, Site>,
    pub clocks: BTreeMap<synth_netlist::NetId, Site>,
    pub rams: BTreeMap<CellId, Site>,
    pub dsps: BTreeMap<CellId, Site>,
    pub hpwl: f64,
}
fn sites(db: &Device, kind: impl Fn(&str) -> bool) -> Result<Vec<Site>, String> {
    let mut out = Vec::new();
    for (tile, t) in db.tiles.iter().enumerate() {
        for (name, index) in db.sites(tile)? {
            if !kind(&db.templates[t.template].sites[index].kind) || t.prohibited.contains(&name) {
                continue;
            }
            let (x, y) = xy(&name)?;
            out.push(Site { tile, index, name, x, y });
        }
    }
    Ok(out)
}
/// Criticalities of each cell's reading of a net in a placement, for
/// timing-driven placement.
pub type Criticality<'a> = &'a mut dyn FnMut(&Placement) -> FxHashMap<(CellId, NetId), f32>;

/// The weight of timing against wirelength in timing-driven placement:
/// VPR's 0.5 lengthened wires enough on congested designs (Euler 48) to
/// lose more in routing than placement gained; 0.25 helped or held on each
/// design measured. The criticality exponent's range, raised as moves
/// become local, is VPR's.
pub(crate) const TIMING_TRADEOFF: f64 = 0.25;
const CRITICALITY_EXPONENT: (f64, f64) = (1.0, 8.0);
/// Temperatures between timing analyses of the placement.
const RETIME_TEMPERATURES: usize = 4;

/// The smallest move window in tiles: neighboring slice columns are two or
/// more tile columns apart, across their interconnect.
const MIN_WINDOW: i32 = 3;

pub fn place(
    db: &Device,
    nl: &Netlist,
    p: &Packed,
    xdc: &Constraints,
    seed: u64,
    effort: f64,
    mut criticality: Option<Criticality<'_>>,
) -> Result<Placement, String> {
    if !effort.is_finite() || effort <= 0.0 || effort > 100.0 {
        return Err("placement effort must be in (0,100]".into());
    }
    let mut ports = BTreeMap::new();
    let mut used_pins = BTreeSet::new();
    let mut names = BTreeSet::new();
    for c in nl.live_cells().filter(|&c| matches!(nl.cell_kind(c), "$input" | "$output")) {
        let name = nl.str(nl.cells[c].name);
        names.insert(name.to_string());
        let constraint =
            xdc.ports.get(name).ok_or_else(|| format!("port {name} has no XDC constraints"))?;
        let pin =
            constraint.pin.as_ref().ok_or_else(|| format!("port {name} has no PACKAGE_PIN"))?;
        if !used_pins.insert(pin.clone()) {
            return Err(format!("package pin {pin} assigned more than once"));
        }
        let info = db
            .pins
            .get(pin)
            .ok_or_else(|| format!("package pin {pin} is not a user IO on {}", db.part.name))?;
        if constraint.standard.as_deref() != Some("LVCMOS33") {
            return Err(format!("port {name}: physical flow currently requires explicit LVCMOS33"));
        }
        let tile = db.tile_by_name[&info.tile];
        let index = db
            .sites(tile)?
            .into_iter()
            .find(|(s, _)| s == &info.site)
            .ok_or("package site missing from grid")?
            .1;
        if db.tiles[tile].prohibited.contains(&info.site) {
            return Err(format!("prohibited package site {}", info.site));
        }
        let (x, y) = xy(&info.site)?;
        ports.insert(c, Site { tile, index, name: info.site.clone(), x, y });
    }
    for n in xdc.ports.keys().chain(xdc.clocks.keys()) {
        if !names.contains(n) {
            return Err(format!("XDC references nonexistent port {n}"));
        }
    }
    let all = sites(db, |k| k == "SLICEL" || k == "SLICEM")?;
    let is_m: Vec<bool> = all
        .iter()
        .map(|s| db.templates[db.tiles[s.tile].template].sites[s.index].kind == "SLICEM")
        .collect();
    let by_xy: BTreeMap<_, _> = all.iter().enumerate().map(|(i, s)| ((s.x, s.y), i)).collect();
    let mut macros = p.chains.clone();
    let chained: BTreeSet<_> = macros.iter().flatten().copied().collect();
    for i in 0..p.slices.len() {
        if !chained.contains(&i) {
            macros.push(vec![i]);
        }
    }
    // A macro of slices needing LUT RAM or shift registers needs SLICEMs.
    let needs_m = |m: &[usize]| m.iter().any(|&s| p.slices[s].needs_m(nl));
    // SLICEM macros go first: their sites are the scarcer.
    macros.sort_by_key(|m| (std::cmp::Reverse(m.len()), !needs_m(m), m[0]));
    let macro_m: Vec<bool> = macros.iter().map(|m| needs_m(m)).collect();
    let mut choices: BTreeMap<(usize, bool), Vec<Vec<usize>>> = BTreeMap::new();
    for (m, &mm) in macros.iter().zip(&macro_m) {
        choices.entry((m.len(), mm)).or_insert_with(|| {
            all.iter()
                .filter_map(|s| {
                    (0..m.len())
                        .map(|i| by_xy.get(&(s.x, s.y + i as u32)).copied())
                        .collect::<Option<Vec<_>>>()
                        .filter(|v| !mm || v.iter().all(|&i| is_m[i]))
                })
                .collect()
        });
    }
    let mut occupied = vec![None; all.len()];
    let mut loc = vec![0; p.slices.len()];
    let mut macro_sites = vec![Vec::new(); macros.len()];
    let mut rng = Rng::new(seed);
    for (i, m) in macros.iter().enumerate() {
        let mut candidates = choices[&(m.len(), macro_m[i])].clone();
        rng.shuffle(&mut candidates);
        let v = candidates.into_iter().find(|v| v.iter().all(|&s| occupied[s].is_none())).ok_or(
            if macro_m[i] {
                "not enough SLICEM sites for LUT RAM and shift registers"
            } else {
                "not enough legal slice sites for carry macros"
            },
        )?;
        for (&cluster, &s) in m.iter().zip(&v) {
            loc[cluster] = s;
            occupied[s] = Some(i);
        }
        macro_sites[i] = v;
    }
    // Block RAMs and DSPs anneal with the slices, each on a site of its kind.
    let kinds =
        [sites(db, |k| k == "RAMB18E1" || k == "FIFO18E1")?, sites(db, |k| k == "DSP48E1")?];
    let mut hard: Vec<(CellId, usize)> = Vec::new();
    for c in nl.live_cells() {
        match nl.cell_kind(c) {
            "RAMB18E1" => {
                crate::physical_bram::validate(nl, c)?;
                hard.push((c, 0));
            }
            "DSP48E1" => {
                crate::physical_dsp::validate(nl, c)?;
                hard.push((c, 1));
            }
            _ => {}
        }
    }
    let hard_index: BTreeMap<CellId, usize> =
        hard.iter().enumerate().map(|(h, &(c, _))| (c, h)).collect();
    let mut hard_occupied: Vec<Vec<Option<usize>>> =
        kinds.iter().map(|k| vec![None; k.len()]).collect();
    let mut hard_loc = vec![0; hard.len()];
    for (h, &(c, k)) in hard.iter().enumerate() {
        let mut candidates: Vec<usize> = (0..kinds[k].len()).collect();
        rng.shuffle(&mut candidates);
        let site = candidates
            .into_iter()
            .find(|&i| hard_occupied[k][i].is_none())
            .ok_or_else(|| format!("not enough {} sites", nl.cell_kind(c)))?;
        hard_occupied[k][site] = Some(h);
        hard_loc[h] = site;
    }
    let mut clock_nets: BTreeSet<_> = p
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
    for &(c, k) in &hard {
        let pins: &[&str] = if k == 0 {
            &["CLKARDCLK", "CLKBWRCLK"]
        } else if crate::physical_dsp::validate(nl, c)?.clocked() {
            &["CLK"]
        } else {
            &[]
        };
        for pin in pins {
            if let Some(n) = nl.pin_net(c, pin)
                && nl.nets[n].constant.is_none()
            {
                clock_nets.insert(n);
            }
        }
    }
    // From here, locations change only through moves, which the timing
    // analysis between temperatures reads.
    let loc: Vec<Cell<usize>> = loc.into_iter().map(Cell::new).collect();
    let hard_loc: Vec<Cell<usize>> = hard_loc.into_iter().map(Cell::new).collect();
    // Nets as the slices, hard blocks and fixed pins they connect, and their
    // connections from driver to each reader with its criticality. Clocks
    // use global routing, not the fabric's.
    #[derive(Clone, Copy, PartialEq)]
    enum End {
        Slice(usize),
        Hard(usize),
        Fixed(i32, i32),
    }
    struct PlacedNet {
        slices: Vec<usize>,
        hard: Vec<usize>,
        fixed: Vec<(f64, f64)>,
        driver: Option<End>,
        /// Readers with their timing weights: criticality raised to the
        /// exponent.
        sinks: Vec<(End, (CellId, NetId), Cell<f64>)>,
    }
    let end = |c: CellId| {
        if let Some(&(s, _)) = p.slots.get(&c) {
            Some(End::Slice(s))
        } else if let Some(&h) = hard_index.get(&c) {
            Some(End::Hard(h))
        } else {
            let t = &db.tiles[ports.get(&c)?.tile];
            Some(End::Fixed(i32::from(t.x), i32::from(t.y)))
        }
    };
    let nets: Vec<PlacedNet> = nl
        .live_nets()
        .filter(|n| !clock_nets.contains(n) && nl.nets[*n].constant.is_none())
        .filter_map(|n| {
            let net = &nl.nets[n];
            let mut clusters = BTreeSet::new();
            let mut blocks = BTreeSet::new();
            let mut fixed = Vec::new();
            for pin in net.driver.iter().chain(&net.users) {
                if let Some(&(s, _)) = p.slots.get(&pin.cell) {
                    clusters.insert(s);
                } else if let Some(&h) = hard_index.get(&pin.cell) {
                    blocks.insert(h);
                } else if let Some(site) = ports.get(&pin.cell) {
                    let tile = &db.tiles[site.tile];
                    fixed.push((f64::from(tile.x), f64::from(tile.y)));
                }
            }
            if clusters.len() + blocks.len() + fixed.len() < 2 || net.users.len() > 100 {
                return None;
            }
            let driver = net.driver.and_then(|d| end(d.cell));
            let mut sinks: Vec<(End, (CellId, NetId), Cell<f64>)> = Vec::new();
            for u in &net.users {
                if let Some(e) = end(u.cell)
                    && Some(e) != driver
                    && !sinks.iter().any(|(_, (c, _), _)| *c == u.cell)
                {
                    sinks.push((e, (u.cell, n), Cell::new(0.0)));
                }
            }
            Some(PlacedNet {
                slices: clusters.into_iter().collect(),
                hard: blocks.into_iter().collect(),
                fixed,
                driver,
                sinks,
            })
        })
        .collect();
    let mut affected = vec![BTreeSet::new(); p.slices.len()];
    let mut hard_affected = vec![BTreeSet::new(); hard.len()];
    for (i, net) in nets.iter().enumerate() {
        for &s in &net.slices {
            affected[s].insert(i);
        }
        for &h in &net.hard {
            hard_affected[h].insert(i);
        }
    }
    let tile_at = |e: End| match e {
        End::Slice(s) => {
            let t = &db.tiles[all[loc[s].get()].tile];
            (i32::from(t.x), i32::from(t.y))
        }
        End::Hard(h) => {
            let t = &db.tiles[kinds[hard[h].1][hard_loc[h].get()].tile];
            (i32::from(t.x), i32::from(t.y))
        }
        End::Fixed(x, y) => (x, y),
    };
    let hpwl = |i: usize| {
        let net = &nets[i];
        let mut bounds = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        let slices = net.slices.iter().map(|&s| tile_at(End::Slice(s)));
        let blocks = net.hard.iter().map(|&h| tile_at(End::Hard(h)));
        for (x, y) in slices
            .chain(blocks)
            .map(|(x, y)| (f64::from(x), f64::from(y)))
            .chain(net.fixed.iter().copied())
        {
            bounds.0 = bounds.0.min(x);
            bounds.1 = bounds.1.min(y);
            bounds.2 = bounds.2.max(x);
            bounds.3 = bounds.3.max(y);
        }
        bounds.2 - bounds.0 + bounds.3 - bounds.1
    };
    // The timing cost of a net: each connection's predicted delay weighted
    // by its criticality raised to the current exponent (VPR).
    let timing = |i: usize| {
        let net = &nets[i];
        let Some(d) = net.driver else { return 0.0 };
        let (dx, dy) = tile_at(d);
        net.sinks
            .iter()
            .map(|(e, _, weight)| {
                let (x, y) = tile_at(*e);
                let delay = if (x, y) == (dx, dy) {
                    0.0
                } else {
                    crate::sta::predicted_delay(x - dx, y - dy)
                };
                weight.get() * delay
            })
            .sum::<f64>()
    };
    // Each net costs its wirelength and timing cost, normalized by their
    // totals at the start of each temperature.
    let (wl_weight, td_weight) = (Cell::new(1.0), Cell::new(0.0));
    let cost = |i: usize| {
        let td = td_weight.get();
        wl_weight.get() * hpwl(i) + if td == 0.0 { 0.0 } else { td * timing(i) }
    };
    // Tile coordinates of every slice and hard block site, and each macro
    // shape's candidate placements by the tile of their first slice, so
    // that moves can stay within a window around a macro.
    let tile_of = |s: &Site| (i32::from(db.tiles[s.tile].x), i32::from(db.tiles[s.tile].y));
    let slice_xy: Vec<(i32, i32)> = all.iter().map(tile_of).collect();
    let hard_xy: Vec<Vec<(i32, i32)>> =
        kinds.iter().map(|k| k.iter().map(tile_of).collect()).collect();
    type ByTile = BTreeMap<(i32, i32), Vec<usize>>;
    let mut nearby: BTreeMap<(usize, bool), ByTile> = BTreeMap::new();
    for (key, list) in &choices {
        let index = nearby.entry(*key).or_default();
        for (i, v) in list.iter().enumerate() {
            index.entry(slice_xy[v[0]]).or_default().push(i);
        }
    }
    let span =
        slice_xy.iter().chain(hard_xy.iter().flatten()).fold(1, |m, &(x, y)| m.max(x).max(y));
    let objects = macros.len() + hard.len();
    let mut step =
        |m: usize, window: i32, temperature: f64, rng: &mut Rng| -> Option<(f64, bool)> {
            let accept = |delta: f64, rng: &mut Rng| {
                delta <= 0.0
                    || temperature.is_infinite()
                    || rng.unit() < (-delta / temperature).exp()
            };
            if m >= macros.len() {
                // A hard block moves to another site of its kind in the window,
                // swapping with the block there.
                let h = m - macros.len();
                let k = hard[h].1;
                let (cx, cy) = hard_xy[k][hard_loc[h].get()];
                // Hard block columns are sparse: the window spans at least a few.
                let w = window.max(10);
                let near: Vec<usize> = (0..kinds[k].len())
                    .filter(|&t| {
                        let (x, y) = hard_xy[k][t];
                        t != hard_loc[h].get() && (x - cx).abs() <= w && (y - cy).abs() <= w
                    })
                    .collect();
                if near.is_empty() {
                    return None;
                }
                let target = near[rng.below(near.len())];
                let other = hard_occupied[k][target];
                let mut changed = hard_affected[h].clone();
                if let Some(o) = other {
                    changed.extend(hard_affected[o].iter().copied());
                }
                let old: f64 = changed.iter().map(|&i| cost(i)).sum();
                let from = hard_loc[h].get();
                hard_loc[h].set(target);
                if let Some(o) = other {
                    hard_loc[o].set(from);
                }
                let new: f64 = changed.iter().map(|&i| cost(i)).sum();
                let accepted = accept(new - old, rng);
                if accepted {
                    hard_occupied[k][target] = Some(h);
                    hard_occupied[k][from] = other;
                } else {
                    hard_loc[h].set(from);
                    if let Some(o) = other {
                        hard_loc[o].set(target);
                    }
                }
                return Some((new - old, accepted));
            }
            let key = (macros[m].len(), macro_m[m]);
            let (cx, cy) = slice_xy[macro_sites[m][0]];
            // Offsets land on tiles without a site for this shape
            // (interconnect columns, the device edge): try a few.
            let d = 2 * window as usize + 1;
            let at = (0..16).find_map(|_| {
                let (dx, dy) = (rng.below(d) as i32 - window, rng.below(d) as i32 - window);
                nearby[&key].get(&(cx + dx, cy + dy))
            })?;
            let target = &choices[&key][at[rng.below(at.len())]];
            if *target == macro_sites[m] {
                return None;
            }
            let owners: BTreeSet<_> =
                target.iter().filter_map(|&s| occupied[s]).filter(|&o| o != m).collect();
            let swap = if owners.is_empty() {
                None
            } else if owners.len() == 1
            && macros[m].len() == 1
            && macros[*owners.first().unwrap()].len() == 1
            // The displaced slice takes this one's site.
            && (!macro_m[*owners.first().unwrap()] || is_m[macro_sites[m][0]])
            {
                owners.first().copied()
            } else {
                return None;
            };
            let mut moved = vec![m];
            if let Some(o) = swap {
                moved.push(o);
            }
            let changed: BTreeSet<_> = moved
                .iter()
                .flat_map(|&m| macros[m].iter().flat_map(|&s| affected[s].iter().copied()))
                .collect();
            let old: f64 = changed.iter().map(|&i| cost(i)).sum();
            let saved: Vec<_> = moved.iter().map(|&m| macro_sites[m].clone()).collect();
            for (&s, &t) in macros[m].iter().zip(target) {
                loc[s].set(t);
            }
            if let Some(o) = swap {
                loc[macros[o][0]].set(saved[0][0]);
            }
            let new: f64 = changed.iter().map(|&i| cost(i)).sum();
            let accepted = accept(new - old, rng);
            if accepted {
                for v in &saved {
                    for &s in v {
                        occupied[s] = None;
                    }
                }
                macro_sites[m] = target.clone();
                if let Some(o) = swap {
                    macro_sites[o] = saved[0].clone();
                }
                for &m in &moved {
                    for &s in &macro_sites[m] {
                        occupied[s] = Some(m);
                    }
                }
            } else {
                for (&m, v) in moved.iter().zip(saved) {
                    for (&cluster, s) in macros[m].iter().zip(v) {
                        loc[cluster].set(s);
                    }
                }
            }
            Some((new - old, accepted))
        };
    let snapshot = || {
        let (mut rams, mut dsps) = (BTreeMap::new(), BTreeMap::new());
        for (h, &(c, k)) in hard.iter().enumerate() {
            let site = kinds[k][hard_loc[h].get()].clone();
            if k == 0 {
                rams.insert(c, site)
            } else {
                dsps.insert(c, site)
            };
        }
        Placement {
            slices: loc.iter().map(|s| all[s.get()].clone()).collect(),
            ports: ports.clone(),
            clocks: BTreeMap::new(),
            rams,
            dsps,
            hpwl: (0..nets.len()).map(hpwl).sum(),
        }
    };
    // Timing-driven placement: criticalities from the current placement,
    // the exponent raised as moves become local, and both costs weighted by
    // the inverse of their totals.
    let mut retime = |window: i32| {
        let Some(criticality) = criticality.as_mut() else { return };
        let local = 1.0 - f64::from(window - MIN_WINDOW) / f64::from((span - MIN_WINDOW).max(1));
        let (first, last) = CRITICALITY_EXPONENT;
        let exponent = first + (last - first) * local.clamp(0.0, 1.0);
        let crit = criticality(&snapshot());
        for net in &nets {
            for (_, key, weight) in &net.sinks {
                weight.set(f64::from(crit.get(key).copied().unwrap_or(0.0)).powf(exponent));
            }
        }
        let wl: f64 = (0..nets.len()).map(hpwl).sum();
        let td: f64 = (0..nets.len()).map(timing).sum();
        wl_weight.set((1.0 - TIMING_TRADEOFF) / wl.max(1.0));
        td_weight.set(if td > 0.0 { TIMING_TRADEOFF / td } else { 0.0 });
    };
    if objects != 0 && !nets.is_empty() {
        // VPR's schedule (Betz and Rose): the initial temperature is twenty
        // times the deviation of cost changes from random moves; each
        // temperature tries about N^(4/3) moves in a window that adapts
        // toward 44% acceptance; cooling is slower while acceptance is
        // moderate, and ends when the temperature is negligible beside the
        // cost per net.
        let mut window = span;
        retime(window);
        let deltas: Vec<f64> = (0..objects)
            .filter_map(|_| step(rng.below(objects), window, f64::INFINITY, &mut rng).map(|d| d.0))
            .collect();
        let mean = deltas.iter().sum::<f64>() / deltas.len().max(1) as f64;
        let deviation = (deltas.iter().map(|d| (d - mean).powi(2)).sum::<f64>()
            / deltas.len().max(1) as f64)
            .sqrt();
        let mut temperature = 20.0 * deviation.max(1e-3);
        let moves = ((objects as f64).powf(4.0 / 3.0) * effort).max(objects as f64) as usize;
        for round in 1.. {
            let (mut tried, mut accepted) = (0, 0);
            for _ in 0..moves {
                if let Some((_, ok)) = step(rng.below(objects), window, temperature, &mut rng) {
                    tried += 1;
                    if ok {
                        accepted += 1;
                    }
                }
            }
            let rate = accepted as f64 / tried.max(1) as f64;
            window =
                ((f64::from(window) * (1.0 - 0.44 + rate)).round() as i32).clamp(MIN_WINDOW, span);
            temperature *= if rate > 0.96 {
                0.5
            } else if rate > 0.8 {
                0.9
            } else if rate > 0.15 {
                0.95
            } else {
                0.8
            };
            if round % RETIME_TEMPERATURES == 0 {
                retime(window);
            }
            let total: f64 = (0..nets.len()).map(cost).sum();
            if temperature < 0.005 * total / nets.len() as f64 {
                break;
            }
        }
        // A final greedy pass.
        for _ in 0..moves {
            step(rng.below(objects), MIN_WINDOW, 0.0, &mut rng);
        }
    }
    let mut placement = snapshot();
    let mut clocks = BTreeMap::new();
    let buffers = sites(db, |k| k == "BUFGCTRL")?;
    let mut used = BTreeSet::new();
    for clock in clock_nets {
        let drv = nl.nets[clock].driver.ok_or("clock has no driver")?;
        let origin = ports
            .get(&drv.cell)
            .map(|s| s.tile)
            .or_else(|| p.slots.get(&drv.cell).map(|&(s, _)| all[loc[s].get()].tile))
            .ok_or("unsupported clock source")?;
        let source = &db.tiles[origin];
        let selected = buffers
            .iter()
            .enumerate()
            .filter(|(i, _)| !used.contains(i))
            .min_by_key(|(_, s)| {
                let t = &db.tiles[s.tile];
                (t.x - source.x).abs() + (t.y - source.y).abs()
            })
            .ok_or("not enough BUFGs")?;
        used.insert(selected.0);
        clocks.insert(clock, selected.1.clone());
    }
    placement.clocks = clocks;
    Ok(placement)
}
