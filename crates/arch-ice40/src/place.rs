//! Placement for iCE40: fixed IOs from pin constraints, global network
//! assignment, then simulated annealing of logic cells, carry chains and
//! block RAMs.
//!
//! Logic tiles hold eight logic cells that share their clock, clock enable,
//! set/reset and clock polarity, and have 32 local tracks to feed inputs;
//! every move keeps those rules satisfied. Carry chains are rigid vertical
//! macros starting at slot 0 of a tile.

use synth_core::{FxHashMap, OrderedMap, Rng};
use synth_netlist::{CellId, Dir, NetId, Netlist, Param, PinRef};

use crate::arch::{Arch, BelId, BelKind};
use crate::device::TileKind;
use crate::pack::{GB, IO, LC, Packed, RAM};

/// Placement options.
#[derive(Clone, Debug)]
pub struct PlaceOptions {
    pub seed: u64,
    /// Moves per temperature step, as a multiple of `cells^(4/3)`.
    pub effort: f64,
    /// Maximum number of logic cell inputs fed from local tracks per tile.
    pub max_locals: usize,
}

impl Default for PlaceOptions {
    fn default() -> Self {
        PlaceOptions { seed: 1, effort: 1.0, max_locals: 32 }
    }
}

/// Final location of every placed cell.
#[derive(Clone, Debug, Default)]
pub struct Placement {
    pub cell_bel: FxHashMap<CellId, BelId>,
    /// Global network index of each global buffer cell (`SB_GB`) or pad
    /// driving a global (`SB_IO` with a `GLOBAL_BUFFER_OUTPUT` pin).
    pub cell_global: FxHashMap<CellId, u8>,
}

/// Clock, clock enable, set/reset and clock polarity shared by a tile's flops.
type ControlSet = (Option<NetId>, Option<NetId>, Option<NetId>, bool);

#[derive(Clone, Copy, Debug, Default)]
struct LcInfo {
    dff: bool,
    neg_clk: bool,
    clk: Option<NetId>,
    cen: Option<NetId>,
    sr: Option<NetId>,
    locals: u8,
}

#[derive(Clone, Debug)]
enum Unit {
    Lc(CellId),
    /// Carry chain, least significant cell first; anchored at slot 0.
    Chain(Vec<CellId>),
    Ram(CellId),
}

/// Pin constraints: top-level port name → package pin.
pub type PinConstraints = OrderedMap<String, String>;

fn port_name(nl: &Netlist, c: CellId) -> Option<String> {
    nl.cells[c]
        .attrs
        .iter()
        .find(|(k, _)| nl.str(**k) == "PORT")
        .and_then(|(_, v)| v.as_str().map(String::from))
}

/// Places a packed design.
pub fn place(
    arch: &Arch,
    nl: &mut Netlist,
    packed: &Packed,
    pins: &PinConstraints,
    opts: &PlaceOptions,
) -> Result<(Placement, Vec<String>), String> {
    let mut log = Vec::new();
    let mut placement = Placement::default();
    place_ios(arch, nl, pins, &mut placement, &mut log)?;
    assign_globals(arch, nl, &mut placement, &mut log)?;
    place_with_fixed(arch, nl, packed, &mut placement, opts, &mut log)?;
    Ok((placement, log))
}

fn place_ios(
    arch: &Arch,
    nl: &Netlist,
    pins: &PinConstraints,
    placement: &mut Placement,
    log: &mut Vec<String>,
) -> Result<(), String> {
    let mut used: FxHashMap<BelId, CellId> = FxHashMap::default();
    let ios: Vec<CellId> = nl.live_cells().filter(|&c| nl.cell_kind(c) == IO).collect();
    let mut unconstrained = Vec::new();
    for &c in &ios {
        let port = port_name(nl, c);
        let pin = port.as_ref().and_then(|p| pins.get(p));
        let Some(pin) = pin else {
            unconstrained.push(c);
            continue;
        };
        let bel = arch.pin_bel(pin).ok_or_else(|| {
            format!(
                "pin {pin} (port {}) does not exist in package {}",
                port.clone().unwrap_or_default(),
                arch.package.name
            )
        })?;
        if let Some(&other) = used.get(&bel) {
            return Err(format!(
                "pin {pin} assigned to both {} and {}",
                nl.str(nl.cells[other].name),
                nl.str(nl.cells[c].name)
            ));
        }
        used.insert(bel, c);
        placement.cell_bel.insert(c, bel);
    }
    // Unconstrained IOs go on any free bonded pin (deterministic order).
    let free: Vec<BelId> = arch
        .package
        .pins
        .iter()
        .filter_map(|p| arch.bel_at(p.x, p.y, p.z, BelKind::Io))
        .filter(|b| !used.contains_key(b))
        .collect();
    let mut free = free.into_iter();
    for c in unconstrained {
        let bel = free.next().ok_or("not enough IO pins")?;
        used.insert(bel, c);
        placement.cell_bel.insert(c, bel);
        log.push(format!(
            "warning: port {} is not constrained; placed on pin {}",
            port_name(nl, c).unwrap_or_else(|| nl.str(nl.cells[c].name).to_string()),
            arch.bel_pin_name(bel).unwrap_or("?")
        ));
    }
    Ok(())
}

/// Assigns global networks: a buffer fed directly by a global-capable pad
/// uses the pad's dedicated connection; the others take a free network,
/// preferring even networks for set/reset users and odd ones for clock
/// enables (those reach the logic tiles' control pins directly).
fn assign_globals(
    arch: &Arch,
    nl: &mut Netlist,
    placement: &mut Placement,
    log: &mut Vec<String>,
) -> Result<(), String> {
    let gbs: Vec<CellId> = nl.live_cells().filter(|&c| nl.cell_kind(c) == GB).collect();
    let mut taken = [false; 8];
    let mut rest = Vec::new();
    for gb in gbs {
        let input = nl.pin_net(gb, "USER_SIGNAL_TO_GLOBAL_BUFFER");
        let drv = input.and_then(|n| nl.nets[n].driver);
        let pad_g = drv
            .filter(|d| {
                nl.cell_kind(d.cell) == IO
                    && nl.str(nl.cells[d.cell].pins[d.pin as usize].name) == "D_IN_0"
            })
            .and_then(|d| {
                let bel = arch.bel(placement.cell_bel[&d.cell]);
                arch.db
                    .globals
                    .iter()
                    .position(|g| g.pad == (bel.x, bel.y, bel.z))
                    .map(|g| (d.cell, g))
            });
        match pad_g {
            Some((io, g)) if !taken[g] => {
                taken[g] = true;
                let out = nl.pin_net(gb, "GLOBAL_BUFFER_OUTPUT");
                nl.remove_cell(gb);
                nl.add_pin(io, "GLOBAL_BUFFER_OUTPUT", Dir::Output, out).map_err(|e| e.0)?;
                // The pad no longer needs to drive the fabric if nothing else reads it.
                if let Some(n) = input
                    && nl.nets[n].users.is_empty()
                {
                    let p = PinRef {
                        cell: io,
                        pin: nl.cells[io].pin_index(nl.names.get("D_IN_0").unwrap()).unwrap()
                            as u32,
                    };
                    nl.disconnect(p);
                }
                placement.cell_global.insert(io, g as u8);
                log.push(format!(
                    "global {g} driven directly by pad {}",
                    arch.bel_pin_name(placement.cell_bel[&io]).unwrap_or("?")
                ));
            }
            _ => rest.push(gb),
        }
    }
    for gb in rest {
        let out = nl.pin_net(gb, "GLOBAL_BUFFER_OUTPUT");
        let (mut sr, mut cen) = (false, false);
        if let Some(n) = out {
            for p in &nl.nets[n].users {
                match nl.str(nl.cells[p.cell].pins[p.pin as usize].name) {
                    "SR" => sr = true,
                    "CEN" => cen = true,
                    _ => {}
                }
            }
        }
        let preferred = |g: usize| {
            (sr && g.is_multiple_of(2)) || (cen && !g.is_multiple_of(2)) || (!sr && !cen)
        };
        let g = (0..8)
            .find(|&g| !taken[g] && preferred(g))
            .or_else(|| (0..8).find(|&g| !taken[g]))
            .ok_or("more than 8 global networks needed")?;
        taken[g] = true;
        let bel = arch.bels_of(BelKind::Gb).find(|&b| arch.bel(b).z == 2 + g as u8).unwrap();
        placement.cell_bel.insert(gb, bel);
        placement.cell_global.insert(gb, g as u8);
    }
    Ok(())
}

/// Weight of a net's bounding box by fanout (VPR's crossing-count correction).
fn net_weight(pins: usize) -> f64 {
    const Q: [f64; 11] =
        [1.0, 1.0, 1.0, 1.0828, 1.1536, 1.2206, 1.2823, 1.3385, 1.3991, 1.4493, 1.4974];
    if pins <= 10 { Q[pins] } else { 1.4974 + 0.0272 * (pins as f64 - 10.0) }
}

struct Annealer<'a> {
    arch: &'a Arch,
    rng: Rng,
    opts: PlaceOptions,
    width: usize,
    height: usize,
    logic_cols: Vec<usize>,
    units: Vec<Unit>,
    /// Unit of each movable cell.
    cell_unit: FxHashMap<CellId, usize>,
    lc_info: FxHashMap<CellId, LcInfo>,
    /// Position of every cell that takes part in wirelength (x, y, z).
    pos: FxHashMap<CellId, (i16, i16, u8)>,
    /// Logic cell slot occupancy, indexed by `slot(x, y, z)`.
    lc_occ: Vec<Option<CellId>>,
    ram_bels: Vec<BelId>,
    ram_occ: Vec<Option<CellId>>,
    cell_ram_slot: FxHashMap<CellId, usize>,
    /// Nets that matter for wirelength, with their distinct cells.
    nets: Vec<Vec<CellId>>,
    net_cost: Vec<f64>,
    cell_nets: FxHashMap<CellId, Vec<u32>>,
    fixed: FxHashMap<CellId, (i16, i16)>,
}

impl<'a> Annealer<'a> {
    fn new(
        arch: &'a Arch,
        nl: &Netlist,
        packed: &Packed,
        opts: &PlaceOptions,
    ) -> Result<Self, String> {
        let die = arch.db.die;
        let (width, height) = (die.width(), die.height());
        let logic_cols: Vec<usize> =
            (1..=width).filter(|&x| die.tile_kind(x, 1) == TileKind::Logic).collect();
        let ram_bels: Vec<BelId> = arch.bels_of(BelKind::Ram).collect();
        let mut sa = Annealer {
            arch,
            rng: Rng::new(opts.seed),
            opts: opts.clone(),
            width,
            height,
            logic_cols,
            units: Vec::new(),
            cell_unit: FxHashMap::default(),
            lc_info: FxHashMap::default(),
            pos: FxHashMap::default(),
            lc_occ: vec![None; (width + 2) * (height + 2) * 8],
            ram_occ: vec![None; ram_bels.len()],
            ram_bels,
            cell_ram_slot: FxHashMap::default(),
            nets: Vec::new(),
            net_cost: Vec::new(),
            cell_nets: FxHashMap::default(),
            fixed: FxHashMap::default(),
        };

        let is_global = |n: NetId| {
            nl.nets[n].driver.is_some_and(|d| {
                let pin = nl.str(nl.cells[d.cell].pins[d.pin as usize].name);
                nl.cell_kind(d.cell) == GB || pin == "GLOBAL_BUFFER_OUTPUT"
            })
        };
        for c in nl.live_cells() {
            if nl.cell_kind(c) != LC {
                continue;
            }
            let p = |name: &str| nl.param(c, name).and_then(Param::as_u64).unwrap_or(0) != 0;
            let mut info =
                LcInfo { dff: p("DFF_ENABLE"), neg_clk: p("NEG_CLK"), ..Default::default() };
            if info.dff {
                info.clk = nl.pin_net(c, "CLK");
                info.cen = nl.pin_net(c, "CEN");
                info.sr = nl.pin_net(c, "SR");
            }
            let cin = nl.pin_net(c, "CIN");
            let mut locals = 0u8;
            for (k, pin) in ["I0", "I1", "I2", "I3"].iter().enumerate() {
                if let Some(n) = nl.pin_net(c, pin)
                    && !(k == 3 && Some(n) == cin)
                {
                    locals += 1;
                }
            }
            info.locals = locals;
            sa.lc_info.insert(c, info);
        }
        // Units: chains first, then single logic cells and RAMs.
        let mut in_chain = std::collections::HashSet::new();
        for chain in &packed.chains {
            let chain: Vec<CellId> = chain.iter().copied().filter(|c| nl.cells[*c].alive).collect();
            if chain.len() > 8 * height {
                return Err(format!(
                    "carry chain of {} cells is longer than a column ({})",
                    chain.len(),
                    8 * height
                ));
            }
            for &c in &chain {
                in_chain.insert(c);
                sa.cell_unit.insert(c, sa.units.len());
            }
            sa.units.push(Unit::Chain(chain));
        }
        for c in nl.live_cells() {
            match nl.cell_kind(c) {
                LC if !in_chain.contains(&c) => {
                    sa.cell_unit.insert(c, sa.units.len());
                    sa.units.push(Unit::Lc(c));
                }
                RAM => {
                    sa.cell_unit.insert(c, sa.units.len());
                    sa.units.push(Unit::Ram(c));
                }
                _ => {}
            }
        }
        // Nets for the wirelength cost, skipping global networks.
        for n in nl.live_nets() {
            let net = &nl.nets[n];
            if is_global(n) {
                continue;
            }
            let mut cells: Vec<CellId> =
                net.driver.iter().chain(&net.users).map(|p| p.cell).collect();
            cells.sort();
            cells.dedup();
            if cells.len() < 2 {
                continue;
            }
            let id = sa.nets.len() as u32;
            for &c in &cells {
                sa.cell_nets.entry(c).or_default().push(id);
            }
            sa.nets.push(cells);
        }
        sa.net_cost = vec![0.0; sa.nets.len()];
        Ok(sa)
    }

    fn set_fixed(&mut self, placement: &Placement) {
        for (&c, &bel) in &placement.cell_bel {
            let b = self.arch.bel(bel);
            self.fixed.insert(c, (b.x as i16, b.y as i16));
        }
    }

    #[inline]
    fn slot(&self, x: usize, y: usize, z: usize) -> usize {
        (x * (self.height + 2) + y) * 8 + z
    }

    fn tile_ok(&self, x: usize, y: usize) -> bool {
        let mut locals = 0usize;
        let mut ctrl: Option<ControlSet> = None;
        for z in 0..8 {
            let Some(c) = self.lc_occ[self.slot(x, y, z)] else { continue };
            let i = &self.lc_info[&c];
            locals += i.locals as usize;
            if i.dff {
                let key = (i.clk, i.cen, i.sr, i.neg_clk);
                match ctrl {
                    None => {
                        ctrl = Some(key);
                        locals += [i.clk, i.cen, i.sr].iter().filter(|n| n.is_some()).count();
                    }
                    Some(k) if k != key => return false,
                    _ => {}
                }
            }
        }
        locals <= self.opts.max_locals
    }

    fn cell_xy(&self, c: CellId) -> (i16, i16) {
        if let Some(&(x, y, _)) = self.pos.get(&c) {
            (x, y)
        } else {
            self.fixed.get(&c).copied().unwrap_or((0, 0))
        }
    }

    fn compute_net_cost(&self, n: usize) -> f64 {
        let cells = &self.nets[n];
        let (mut x0, mut y0, mut x1, mut y1) = (i16::MAX, i16::MAX, i16::MIN, i16::MIN);
        for &c in cells {
            let (x, y) = self.cell_xy(c);
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
        net_weight(cells.len()) * ((x1 - x0) as f64 + (y1 - y0) as f64)
    }

    fn initial_placement(&mut self) -> Result<(), String> {
        // Chains: fill columns bottom-up.
        let mut col_fill: Vec<usize> = vec![1; self.width + 2]; // next free tile row per column
        let mut ci = 0usize;
        let units = self.units.clone();
        for unit in &units {
            let Unit::Chain(cells) = unit else { continue };
            let tiles = cells.len().div_ceil(8);
            let mut placed = false;
            for k in 0..self.logic_cols.len() {
                let x = self.logic_cols[(ci + k) % self.logic_cols.len()];
                if col_fill[x] + tiles - 1 <= self.height {
                    let y0 = col_fill[x];
                    for (i, &c) in cells.iter().enumerate() {
                        let (y, z) = (y0 + i / 8, i % 8);
                        let s = self.slot(x, y, z);
                        self.lc_occ[s] = Some(c);
                        self.pos.insert(c, (x as i16, y as i16, z as u8));
                    }
                    col_fill[x] += tiles;
                    ci += 1;
                    placed = true;
                    break;
                }
            }
            if !placed {
                return Err("not enough logic columns for carry chains".into());
            }
        }
        // Single cells: scan slots in a random order of tiles.
        let mut tiles: Vec<(usize, usize)> =
            self.logic_cols.iter().flat_map(|&x| (1..=self.height).map(move |y| (x, y))).collect();
        self.rng.shuffle(&mut tiles);
        let mut ti = 0usize;
        for unit in &units {
            match unit {
                Unit::Lc(c) => {
                    let mut placed = false;
                    for attempt in 0..tiles.len() {
                        let (x, y) = tiles[(ti + attempt) % tiles.len()];
                        for z in 0..8 {
                            let s = self.slot(x, y, z);
                            if self.lc_occ[s].is_some() {
                                continue;
                            }
                            self.lc_occ[s] = Some(*c);
                            if self.tile_ok(x, y) {
                                self.pos.insert(*c, (x as i16, y as i16, z as u8));
                                placed = true;
                                ti = (ti + attempt) % tiles.len();
                            } else {
                                self.lc_occ[s] = None;
                            }
                            break;
                        }
                        if placed {
                            break;
                        }
                    }
                    if !placed {
                        return Err(
                            "design does not fit: no legal location for a logic cell".into()
                        );
                    }
                }
                Unit::Ram(c) => {
                    let slot = self
                        .ram_occ
                        .iter()
                        .position(|o| o.is_none())
                        .ok_or("design needs more block RAMs than available")?;
                    self.ram_occ[slot] = Some(*c);
                    self.cell_ram_slot.insert(*c, slot);
                    let b = self.arch.bel(self.ram_bels[slot]);
                    self.pos.insert(*c, (b.x as i16, b.y as i16, 0));
                }
                Unit::Chain(_) => {}
            }
        }
        for n in 0..self.nets.len() {
            self.net_cost[n] = self.compute_net_cost(n);
        }
        Ok(())
    }

    fn total_cost(&self) -> f64 {
        self.net_cost.iter().sum()
    }

    /// Recomputes costs of the nets touching `cells`; returns the delta and
    /// the new costs (applied only on accept).
    fn delta(&self, cells: &[CellId], scratch: &mut Vec<(u32, f64)>) -> f64 {
        scratch.clear();
        let mut d = 0.0;
        for &c in cells {
            if let Some(nets) = self.cell_nets.get(&c) {
                for &n in nets {
                    if scratch.iter().any(|(m, _)| *m == n) {
                        continue;
                    }
                    let new = self.compute_net_cost(n as usize);
                    d += new - self.net_cost[n as usize];
                    scratch.push((n, new));
                }
            }
        }
        d
    }

    /// Tries one move; returns whether it was accepted.
    fn try_move(&mut self, temp: f64, rlim: f64, scratch: &mut Vec<(u32, f64)>) -> Option<bool> {
        let u = self.rng.below(self.units.len());
        let r = rlim.max(1.0) as i64;
        match self.units[u].clone() {
            Unit::Lc(c) => {
                let (x, y, z) = self.pos[&c];
                let nx = self
                    .rng
                    .range_inclusive((x as i64 - r).max(1), (x as i64 + r).min(self.width as i64))
                    as usize;
                let ny = self
                    .rng
                    .range_inclusive((y as i64 - r).max(1), (y as i64 + r).min(self.height as i64))
                    as usize;
                if self.arch.db.die.tile_kind(nx, ny) != TileKind::Logic {
                    return None;
                }
                let nz = self.rng.below(8);
                let (ox, oy, oz) = (x as usize, y as usize, z as usize);
                if (nx, ny, nz) == (ox, oy, oz) {
                    return None;
                }
                let s_old = self.slot(ox, oy, oz);
                let s_new = self.slot(nx, ny, nz);
                let other = self.lc_occ[s_new];
                if let Some(o) = other
                    && !matches!(self.units[self.cell_unit[&o]], Unit::Lc(_))
                {
                    return None;
                }
                // Apply.
                self.lc_occ[s_new] = Some(c);
                self.lc_occ[s_old] = other;
                self.pos.insert(c, (nx as i16, ny as i16, nz as u8));
                if let Some(o) = other {
                    self.pos.insert(o, (ox as i16, oy as i16, oz as u8));
                }
                let legal = self.tile_ok(nx, ny) && ((ox, oy) == (nx, ny) || self.tile_ok(ox, oy));
                let moved: Vec<CellId> = std::iter::once(c).chain(other).collect();
                let accept = legal && {
                    let d = self.delta(&moved, scratch);
                    d <= 0.0 || self.rng.unit() < (-d / temp).exp()
                };
                if accept {
                    for &(n, cost) in scratch.iter() {
                        self.net_cost[n as usize] = cost;
                    }
                } else {
                    self.lc_occ[s_old] = Some(c);
                    self.lc_occ[s_new] = other;
                    self.pos.insert(c, (x, y, z));
                    if let Some(o) = other {
                        self.pos.insert(o, (nx as i16, ny as i16, nz as u8));
                    }
                }
                Some(accept)
            }
            Unit::Chain(cells) => {
                let (x, y, _) = self.pos[&cells[0]];
                let tiles = cells.len().div_ceil(8);
                let col = self.rng.below(self.logic_cols.len());
                let nx = self.logic_cols[col];
                if (nx as i64 - x as i64).abs() > r {
                    return None;
                }
                let max_y0 = self.height + 1 - tiles;
                let ny = self
                    .rng
                    .range_inclusive((y as i64 - r).max(1), (y as i64 + r).min(max_y0 as i64))
                    as usize;
                if (nx, ny) == (x as usize, y as usize) {
                    return None;
                }
                // Target slots must be free or belong to this chain.
                for i in 0..cells.len() {
                    let s = self.slot(nx, ny + i / 8, i % 8);
                    if let Some(o) = self.lc_occ[s]
                        && !cells.contains(&o)
                    {
                        return None;
                    }
                }
                let old: Vec<(i16, i16, u8)> = cells.iter().map(|c| self.pos[c]).collect();
                for (i, &c) in cells.iter().enumerate() {
                    let (ox, oy, oz) = old[i];
                    let s = self.slot(ox as usize, oy as usize, oz as usize);
                    if self.lc_occ[s] == Some(c) {
                        self.lc_occ[s] = None;
                    }
                }
                for (i, &c) in cells.iter().enumerate() {
                    let (cy, cz) = (ny + i / 8, i % 8);
                    let s = self.slot(nx, cy, cz);
                    self.lc_occ[s] = Some(c);
                    self.pos.insert(c, (nx as i16, cy as i16, cz as u8));
                }
                let mut legal = true;
                for t in 0..tiles {
                    legal &= self.tile_ok(nx, ny + t);
                }
                let accept = legal && {
                    let d = self.delta(&cells, scratch);
                    d <= 0.0 || self.rng.unit() < (-d / temp).exp()
                };
                if accept {
                    for &(n, cost) in scratch.iter() {
                        self.net_cost[n as usize] = cost;
                    }
                } else {
                    for (i, &c) in cells.iter().enumerate() {
                        let s = self.slot(nx, ny + i / 8, i % 8);
                        self.lc_occ[s] = None;
                        let _ = c;
                    }
                    for (i, &c) in cells.iter().enumerate() {
                        let (ox, oy, oz) = old[i];
                        let s = self.slot(ox as usize, oy as usize, oz as usize);
                        self.lc_occ[s] = Some(c);
                        self.pos.insert(c, old[i]);
                    }
                }
                Some(accept)
            }
            Unit::Ram(c) => {
                if self.ram_bels.len() < 2 {
                    return None;
                }
                let s_old = self.cell_ram_slot[&c];
                let s_new = self.rng.below(self.ram_bels.len());
                if s_new == s_old {
                    return None;
                }
                let other = self.ram_occ[s_new];
                let place = |sa: &mut Self, cell: CellId, slot: usize| {
                    sa.ram_occ[slot] = Some(cell);
                    sa.cell_ram_slot.insert(cell, slot);
                    let b = sa.arch.bel(sa.ram_bels[slot]);
                    sa.pos.insert(cell, (b.x as i16, b.y as i16, 0));
                };
                place(self, c, s_new);
                self.ram_occ[s_old] = None;
                if let Some(o) = other {
                    place(self, o, s_old);
                }
                let moved: Vec<CellId> = std::iter::once(c).chain(other).collect();
                let d = self.delta(&moved, scratch);
                let accept = d <= 0.0 || self.rng.unit() < (-d / temp).exp();
                if accept {
                    for &(n, cost) in scratch.iter() {
                        self.net_cost[n as usize] = cost;
                    }
                } else {
                    place(self, c, s_old);
                    self.ram_occ[s_new] = None;
                    if let Some(o) = other {
                        place(self, o, s_new);
                    }
                }
                Some(accept)
            }
        }
    }

    fn anneal(&mut self, log: &mut Vec<String>) {
        if self.units.is_empty() || self.nets.is_empty() {
            return;
        }
        let n = self.units.len() as f64;
        let moves_per_temp = ((self.opts.effort * n.powf(4.0 / 3.0)) as usize).max(100);
        let mut scratch = Vec::new();
        // Initial temperature: standard deviation of random move deltas.
        let mut rlim = self.width.max(self.height) as f64;
        let start = self.total_cost();
        let mut deltas = Vec::new();
        for _ in 0..self.units.len().min(500) {
            let before = self.total_cost();
            if self.try_move(f64::INFINITY, rlim, &mut scratch) == Some(true) {
                deltas.push(self.total_cost() - before);
            }
        }
        let mean = deltas.iter().sum::<f64>() / deltas.len().max(1) as f64;
        let var =
            deltas.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / deltas.len().max(1) as f64;
        let mut temp = 20.0 * var.sqrt().max(1.0);
        let nets = self.nets.len() as f64;
        let mut iters = 0;
        loop {
            let (mut tried, mut accepted) = (0usize, 0usize);
            for _ in 0..moves_per_temp {
                if let Some(a) = self.try_move(temp, rlim, &mut scratch) {
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
            rlim = (rlim * (1.0 - 0.44 + rate)).clamp(1.0, self.width.max(self.height) as f64);
            iters += 1;
            if temp < 0.005 * self.total_cost() / nets || iters > 2000 {
                break;
            }
        }
        // Final greedy pass at zero temperature.
        for _ in 0..moves_per_temp * 2 {
            self.try_move(1e-9, 3.0, &mut scratch);
        }
        // Re-sum to avoid accumulated rounding.
        for i in 0..self.nets.len() {
            self.net_cost[i] = self.compute_net_cost(i);
        }
        log.push(format!(
            "placement: {} units, {} nets, wirelength {:.0} -> {:.0} in {} temperature steps",
            self.units.len(),
            self.nets.len(),
            start,
            self.total_cost(),
            iters
        ));
    }

    fn finish(&self, placement: &mut Placement) {
        for (&c, &(x, y, z)) in &self.pos {
            let kind = if matches!(self.units[self.cell_unit[&c]], Unit::Ram(_)) {
                BelKind::Ram
            } else {
                BelKind::Lc
            };
            let bel = self.arch.bel_at(x as u8, y as u8, z, kind).expect("placed on a bel");
            placement.cell_bel.insert(c, bel);
        }
    }
}

/// Runs placement after IOs and globals are fixed. Exposed separately so
/// callers can place IOs and globals themselves.
pub fn place_with_fixed(
    arch: &Arch,
    nl: &Netlist,
    packed: &Packed,
    placement: &mut Placement,
    opts: &PlaceOptions,
    log: &mut Vec<String>,
) -> Result<(), String> {
    let mut sa = Annealer::new(arch, nl, packed, opts)?;
    sa.set_fixed(placement);
    sa.initial_placement()?;
    sa.anneal(log);
    sa.finish(placement);
    Ok(())
}
