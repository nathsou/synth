//! Deterministic placement by annealing HPWL: slice macros with legal
//! carry chains, and block RAMs and DSPs on sites of their kind.
use crate::{
    device::{Device, xy},
    pack::Packed,
    xdc::Constraints,
};
use std::collections::{BTreeMap, BTreeSet};
use synth_core::rng::Rng;
use synth_netlist::{CellId, Netlist};

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
pub fn place(
    db: &Device,
    nl: &Netlist,
    p: &Packed,
    xdc: &Constraints,
    seed: u64,
    effort: f64,
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
    // Nets as the slices, hard blocks and fixed pins they connect. Clocks
    // use global routing, not the fabric's.
    struct PlacedNet {
        slices: Vec<usize>,
        hard: Vec<usize>,
        fixed: Vec<(f64, f64)>,
    }
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
            Some(PlacedNet {
                slices: clusters.into_iter().collect(),
                hard: blocks.into_iter().collect(),
                fixed,
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
    let cost = |i: usize, loc: &[usize], hard_loc: &[usize]| {
        let net = &nets[i];
        let mut bounds = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        let slices = net.slices.iter().map(|&s| &db.tiles[all[loc[s]].tile]);
        let blocks = net.hard.iter().map(|&h| &db.tiles[kinds[hard[h].1][hard_loc[h]].tile]);
        for (x, y) in slices
            .chain(blocks)
            .map(|t| (f64::from(t.x), f64::from(t.y)))
            .chain(net.fixed.iter().copied())
        {
            bounds.0 = bounds.0.min(x);
            bounds.1 = bounds.1.min(y);
            bounds.2 = bounds.2.max(x);
            bounds.3 = bounds.3.max(y);
        }
        bounds.2 - bounds.0 + bounds.3 - bounds.1
    };
    let objects = macros.len() + hard.len();
    if objects != 0 {
        for round in 0..40 {
            let temperature = 20.0 * (0.85f64).powi(round);
            for _ in 0..((objects * 10) as f64 * effort) as usize {
                let m = rng.below(objects);
                if m >= macros.len() {
                    // A hard block moves to another site of its kind,
                    // swapping with the block there.
                    let h = m - macros.len();
                    let k = hard[h].1;
                    let target = rng.below(kinds[k].len());
                    if target == hard_loc[h] {
                        continue;
                    }
                    let other = hard_occupied[k][target];
                    let mut changed = hard_affected[h].clone();
                    if let Some(o) = other {
                        changed.extend(hard_affected[o].iter().copied());
                    }
                    let old: f64 = changed.iter().map(|&i| cost(i, &loc, &hard_loc)).sum();
                    let from = hard_loc[h];
                    hard_loc[h] = target;
                    if let Some(o) = other {
                        hard_loc[o] = from;
                    }
                    let new: f64 = changed.iter().map(|&i| cost(i, &loc, &hard_loc)).sum();
                    if new <= old || rng.unit() < ((old - new) / temperature).exp() {
                        hard_occupied[k][target] = Some(h);
                        hard_occupied[k][from] = other;
                    } else {
                        hard_loc[h] = from;
                        if let Some(o) = other {
                            hard_loc[o] = target;
                        }
                    }
                    continue;
                }
                let candidates = &choices[&(macros[m].len(), macro_m[m])];
                let target = &candidates[rng.below(candidates.len())];
                if *target == macro_sites[m] {
                    continue;
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
                    continue;
                };
                let mut moved = vec![m];
                if let Some(o) = swap {
                    moved.push(o);
                }
                let changed: BTreeSet<_> = moved
                    .iter()
                    .flat_map(|&m| macros[m].iter().flat_map(|&s| affected[s].iter().copied()))
                    .collect();
                let old: f64 = changed.iter().map(|&i| cost(i, &loc, &hard_loc)).sum();
                let saved: Vec<_> = moved.iter().map(|&m| macro_sites[m].clone()).collect();
                for (&s, &t) in macros[m].iter().zip(target) {
                    loc[s] = t;
                }
                if let Some(o) = swap {
                    loc[macros[o][0]] = saved[0][0];
                }
                let new: f64 = changed.iter().map(|&i| cost(i, &loc, &hard_loc)).sum();
                if new <= old || rng.unit() < ((old - new) / temperature).exp() {
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
                            loc[cluster] = s;
                        }
                    }
                }
            }
        }
    }
    let hpwl = (0..nets.len()).map(|i| cost(i, &loc, &hard_loc)).sum();
    let mut rams = BTreeMap::new();
    let mut dsps = BTreeMap::new();
    for (h, &(c, k)) in hard.iter().enumerate() {
        let site = kinds[k][hard_loc[h]].clone();
        if k == 0 {
            rams.insert(c, site)
        } else {
            dsps.insert(c, site)
        };
    }
    let mut clocks = BTreeMap::new();
    let buffers = sites(db, |k| k == "BUFGCTRL")?;
    let mut used = BTreeSet::new();
    for clock in clock_nets {
        let drv = nl.nets[clock].driver.ok_or("clock has no driver")?;
        let origin = ports
            .get(&drv.cell)
            .map(|s| s.tile)
            .or_else(|| p.slots.get(&drv.cell).map(|&(s, _)| all[loc[s]].tile))
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
    Ok(Placement {
        slices: loc.into_iter().map(|s| all[s].clone()).collect(),
        ports,
        clocks,
        rams,
        dsps,
        hpwl,
    })
}
