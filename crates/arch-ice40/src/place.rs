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
use crate::pack::{GB, GlobalClass, IO, LC, Packed, RAM, global_class};

/// Placement options.
#[derive(Clone, Debug)]
pub struct PlaceOptions {
    /// Weight of timing against wirelength in the placement cost (0..1);
    /// 0 places for wirelength only.
    pub timing_weight: f64,
    pub seed: u64,
    /// Moves per temperature step, as a multiple of `cells^(4/3)`.
    pub effort: f64,
    /// Maximum number of logic cell inputs fed from local tracks per tile.
    pub max_locals: usize,
    pub clocks: crate::timing::ClockConstraints,
}

impl Default for PlaceOptions {
    fn default() -> Self {
        PlaceOptions {
            timing_weight: crate::flow::TIMING_WEIGHT,
            seed: 1,
            effort: 1.0,
            max_locals: 32,
            clocks: Default::default(),
        }
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

/// No net, in [`LcInfo`].
const NO_NET: u32 = u32::MAX;

/// What tile legality needs of a logic cell, nets as raw ids.
#[derive(Clone, Copy, Debug)]
struct LcInfo {
    /// The control set of the cell's flip-flop (clock, clock enable,
    /// set/reset and clock polarity, which a tile's flip-flops share),
    /// numbered from 1 in the order of those nets; 0 without a flip-flop.
    ctrl: u32,
    /// Nets on inputs `I0`/`I2` and on `I1`/`I3` (unless it is the carry
    /// input). In even logic cells the former use class A local tracks and
    /// the latter class B; odd cells are the other way round.
    even_pins: [u32; 2],
    odd_pins: [u32; 2],
    /// Control nets that need a class A local track (not on a global).
    fabric_ctrl: [u32; 3],
}

impl Default for LcInfo {
    fn default() -> Self {
        LcInfo { ctrl: 0, even_pins: [NO_NET; 2], odd_pins: [NO_NET; 2], fabric_ctrl: [NO_NET; 3] }
    }
}

#[derive(Clone, Debug)]
enum Unit {
    Lc(CellId),
    /// Carry chain, least significant cell first; anchored at slot 0.
    Chain(Vec<CellId>),
    Ram(CellId),
    /// A logic cell placed beforehand (a PLL's `LOCK` buffer): never moves.
    Pinned(CellId),
}

impl Unit {
    fn cells(&self) -> &[CellId] {
        match self {
            Self::Lc(c) | Self::Ram(c) | Self::Pinned(c) => std::slice::from_ref(c),
            Self::Chain(cells) => cells,
        }
    }
}

/// Pin constraints: top-level port name → package pin.
pub type PinConstraints = OrderedMap<String, String>;

pub(crate) fn port_name(nl: &Netlist, c: CellId) -> Option<String> {
    nl.cells[c]
        .attrs
        .iter()
        .find(|(k, _)| nl.str(**k) == "PORT")
        .and_then(|(_, v)| v.as_str().map(String::from))
}

/// Whether a net is on a global network (driven by a global buffer, a
/// global pad or a PLL's global output).
pub(crate) fn on_global(nl: &Netlist, n: NetId) -> bool {
    nl.nets[n].driver.is_some_and(|d| {
        let pin = nl.str(nl.cells[d.cell].pins[d.pin as usize].name);
        nl.cell_kind(d.cell) == GB || pin == "GLOBAL_BUFFER_OUTPUT" || pin == "PLLOUTGLOBAL"
    })
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
    place_plls(arch, nl, packed, pins, &mut placement, &mut log)?;
    place_ios(arch, nl, pins, &mut placement, &mut log)?;
    assign_globals(arch, nl, &mut placement, &mut log)?;
    place_with_fixed(arch, nl, packed, &mut placement, opts, &mut log)?;
    Ok((placement, log))
}

/// Places each PLL on a site whose output pad is not one of the design's
/// pins (the pad is the PLL's, and its global network too), and the logic
/// cell buffering its `LOCK` in the one tile `LOCK` reaches.
pub(crate) fn place_plls(
    arch: &Arch,
    nl: &Netlist,
    packed: &Packed,
    pins: &PinConstraints,
    placement: &mut Placement,
    log: &mut Vec<String>,
) -> Result<(), String> {
    let plls: Vec<CellId> =
        nl.live_cells().filter(|&c| nl.cell_kind(c) == crate::pll::CORE).collect();
    if plls.is_empty() {
        return Ok(());
    }
    let pads: Vec<(u8, u8, u8)> = pins
        .values()
        .filter_map(|p| arch.pin_bel(p))
        .map(|b| (arch.bel(b).x, arch.bel(b).y, arch.bel(b).z))
        .collect();
    let mut sites = arch.plls.iter().filter(|s| !pads.contains(&s.out_io));
    for c in plls {
        let name = nl.str(nl.cells[c].name);
        let site = sites.next().ok_or_else(|| {
            if arch.plls.is_empty() {
                format!("{name}: {} {} has no PLL", arch.part, arch.package.name)
            } else {
                format!("{name}: no PLL left whose output pad is free")
            }
        })?;
        placement.cell_bel.insert(c, site.bel);
        placement.cell_global.insert(c, site.global);
        let b = arch.bel(site.bel);
        log.push(format!("{name} at PLL ({}, {}), global {}", b.x, b.y, site.global));
        for &(_, lc) in packed.lock_buffers.iter().filter(|(p, _)| *p == c) {
            let (x, y) = site.lock_tile;
            let bel = arch.bel_at(x, y, 0, BelKind::Lc).ok_or("no logic cell next to the PLL")?;
            placement.cell_bel.insert(lc, bel);
        }
    }
    Ok(())
}

/// The IO blocks that PLLs placed so far use for their outputs.
fn pll_pads(arch: &Arch, placement: &Placement) -> Vec<(u8, u8, u8)> {
    arch.plls
        .iter()
        .filter(|s| placement.cell_bel.values().any(|&b| b == s.bel))
        .map(|s| s.out_io)
        .collect()
}

pub(crate) fn place_ios(
    arch: &Arch,
    nl: &Netlist,
    pins: &PinConstraints,
    placement: &mut Placement,
    log: &mut Vec<String>,
) -> Result<(), String> {
    let mut used: FxHashMap<BelId, CellId> = FxHashMap::default();
    let reserved = pll_pads(arch, placement);
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
        .filter(|p| !reserved.contains(&(p.x, p.y, p.z)))
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
pub(crate) fn assign_globals(
    arch: &Arch,
    nl: &mut Netlist,
    placement: &mut Placement,
    log: &mut Vec<String>,
) -> Result<(), String> {
    let gbs: Vec<CellId> = nl.live_cells().filter(|&c| nl.cell_kind(c) == GB).collect();
    // Networks already driven (by PLLs).
    let mut taken = [false; 8];
    for &g in placement.cell_global.values() {
        taken[g as usize] = true;
    }
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
    // Control nets first (they need a network of their class), then clocks.
    let class_of = |nl: &Netlist, gb: CellId| {
        let (mut reset, mut enable) = (false, false);
        if let Some(n) = nl.pin_net(gb, "GLOBAL_BUFFER_OUTPUT") {
            for p in &nl.nets[n].users {
                let pin = nl.str(nl.cells[p.cell].pins[p.pin as usize].name);
                match global_class(nl.cell_kind(p.cell), crate::cells::port_base(pin)) {
                    Some(GlobalClass::Reset) => reset = true,
                    Some(GlobalClass::Enable) => enable = true,
                    _ => {}
                }
            }
        }
        (reset, enable)
    };
    rest.sort_by_key(|&gb| {
        let (r, e) = class_of(nl, gb);
        !(r || e)
    });
    for gb in rest {
        let (reset, enable) = class_of(nl, gb);
        if reset && enable {
            log.push(format!(
                "warning: global {} drives both set/reset and clock-enable pins; some may not route",
                nl.str(nl.cells[gb].name)
            ));
        }
        let fits = |g: usize| (!reset || g.is_multiple_of(2)) && (!enable || !g.is_multiple_of(2));
        // Plain clocks take the networks nobody else can use first.
        let order: Vec<usize> =
            if !reset && !enable { vec![0, 2, 4, 6, 1, 3, 5, 7] } else { (0..8).collect() };
        let g = order.iter().copied().find(|&g| !taken[g] && fits(g));
        let Some(g) = g else {
            if reset || enable {
                // No network of the right class left: back to fabric routing.
                let input = nl.pin_net(gb, "USER_SIGNAL_TO_GLOBAL_BUFFER").unwrap();
                let out = nl.pin_net(gb, "GLOBAL_BUFFER_OUTPUT").unwrap();
                let users = nl.nets[out].users.clone();
                nl.remove_cell(gb);
                for p in users {
                    nl.disconnect(p);
                    nl.connect(p, input).map_err(|e| e.0)?;
                }
                log.push(format!(
                    "{} stays on fabric routing (no free global network of its class)",
                    nl.str(nl.nets[input].name)
                ));
                continue;
            }
            return Err("more than 8 global networks needed".into());
        };
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

/// A driver-to-sink connection, for the timing cost.
#[derive(Clone, Copy, Debug)]
struct Conn {
    drv: CellId,
    sink: CellId,
}

/// Lists of `u32` by index, in compressed sparse row form.
#[derive(Debug, Default)]
struct Adjacency {
    off: Vec<u32>,
    items: Vec<u32>,
}

impl Adjacency {
    fn new(lists: Vec<Vec<u32>>) -> Self {
        let mut off = Vec::with_capacity(lists.len() + 1);
        off.push(0);
        let mut items = Vec::new();
        for l in lists {
            items.extend(l);
            off.push(items.len() as u32);
        }
        Adjacency { off, items }
    }

    #[inline]
    fn of(&self, i: usize) -> &[u32] {
        match self.off.get(i + 1) {
            Some(&end) => &self.items[self.off[i] as usize..end as usize],
            None => &[],
        }
    }

    fn len(&self) -> usize {
        self.off.len().saturating_sub(1)
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// No unit, RAM slot or position: the marker of the dense per-cell tables.
const NONE: u32 = u32::MAX;

/// A net's bounding box, with the number of its cells on each side, so
/// that moves update it without visiting the other cells (as VPR does).
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
    /// The box of points `at` (none if there are none).
    fn of(at: impl Iterator<Item = (i16, i16)>) -> Option<BBox> {
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
        b
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

/// What the annealing rule makes of a move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    Accepted,
    Rejected,
    /// A move that changes no cost: always taken.
    Free,
}

impl Decision {
    /// Whether the move was accepted, for the moves the schedule counts.
    fn counted(self) -> Option<bool> {
        (self != Decision::Free).then_some(self == Decision::Accepted)
    }
}

/// Nets with at most this many cells get their bounding boxes by a scan
/// rather than updates.
const SCANNED_NET: usize = 8;

struct Annealer<'a> {
    arch: &'a Arch,
    nl: &'a Netlist,
    rng: Rng,
    opts: PlaceOptions,
    width: usize,
    height: usize,
    logic_cols: Vec<usize>,
    units: Vec<Unit>,
    /// The per-cell tables below are indexed by cell index.
    /// Unit of each movable cell ([`NONE`] if none).
    cell_unit: Vec<u32>,
    lc_info: Vec<LcInfo>,
    /// Position of every placed cell (x, y, z).
    pos: Vec<Option<(i16, i16, u8)>>,
    /// Where each cell counts in the costs: where it is placed, else where
    /// it is fixed, else the origin.
    xy: Vec<(i16, i16)>,
    /// Logic cell slot occupancy, indexed by `slot(x, y, z)`.
    lc_occ: Vec<Option<CellId>>,
    ram_bels: Vec<BelId>,
    ram_occ: Vec<Option<CellId>>,
    cell_ram_slot: Vec<u32>,
    /// Nets that matter for wirelength: their distinct cells (raw ids).
    nets: Adjacency,
    /// Bounding box of each net, and its cost.
    net_bb: Vec<BBox>,
    net_cost: Vec<f64>,
    cell_nets: Adjacency,
    fixed: Vec<Option<(i16, i16)>>,
    /// Logic cells placed beforehand, and where.
    pinned: Vec<(CellId, (i16, i16, u8))>,
    /// Units moves may pick (all when empty): those an incremental repair
    /// placed anew.
    movable: Vec<usize>,
    conns: Vec<Conn>,
    cell_conns: Adjacency,
    timing: crate::timing::TimingGraph,
    /// The placement connection of each timing graph connection (`u32::MAX`
    /// for connections that do not depend on placement).
    conn_of_timing: Vec<u32>,
    /// Timing weight (criticality^8) and current cost of each connection.
    conn_weight: Vec<f64>,
    conn_cost: Vec<f64>,
    /// Normalisation of the two cost terms (their totals at the last update).
    bb_norm: f64,
    t_norm: f64,
    /// New bounding boxes of the nets, and costs of the connections, that
    /// the move being tried changes.
    net_scratch: Vec<(u32, BBox)>,
    conn_scratch: Vec<(u32, f64)>,
    /// Cells the move being tried displaces, and where they were.
    moved: Vec<(CellId, (i16, i16))>,
    /// Marks of the nets, connections and cells a step has visited: those
    /// equal to `stamp`.
    net_seen: Vec<u32>,
    conn_seen: Vec<u32>,
    cell_seen: Vec<u32>,
    stamp: u32,
    /// Median move scratch: bounding box sides of the moved cells' nets.
    xs: Vec<i16>,
    ys: Vec<i16>,
    delays: &'a crate::estimate::DelayTable,
}

impl<'a> Annealer<'a> {
    fn new(
        arch: &'a Arch,
        nl: &'a Netlist,
        packed: &Packed,
        opts: &PlaceOptions,
    ) -> Result<Self, String> {
        let die = arch.db.die;
        let (width, height) = (die.width(), die.height());
        let logic_cols: Vec<usize> =
            (1..=width).filter(|&x| die.tile_kind(x, 1) == TileKind::Logic).collect();
        let ram_bels: Vec<BelId> = arch.bels_of(BelKind::Ram).collect();
        let timing = crate::timing::TimingGraph::new(arch, nl, &opts.clocks);
        let ncells = nl.cells.len();
        let mut sa = Annealer {
            arch,
            nl,
            conn_of_timing: vec![u32::MAX; timing.conns.len()],
            timing,
            conns: Vec::new(),
            cell_conns: Adjacency::default(),
            conn_weight: Vec::new(),
            conn_cost: Vec::new(),
            bb_norm: 1.0,
            t_norm: 1.0,
            net_scratch: Vec::new(),
            conn_scratch: Vec::new(),
            moved: Vec::new(),
            net_seen: Vec::new(),
            conn_seen: Vec::new(),
            cell_seen: vec![0; ncells],
            stamp: 0,
            xs: Vec::new(),
            ys: Vec::new(),
            delays: arch.delay_table(),
            rng: Rng::new(opts.seed),
            opts: opts.clone(),
            width,
            height,
            logic_cols,
            units: Vec::new(),
            cell_unit: vec![NONE; ncells],
            lc_info: vec![LcInfo::default(); ncells],
            pos: vec![None; ncells],
            xy: vec![(0, 0); ncells],
            lc_occ: vec![None; (width + 2) * (height + 2) * 8],
            ram_occ: vec![None; ram_bels.len()],
            ram_bels,
            cell_ram_slot: vec![NONE; ncells],
            nets: Adjacency::default(),
            net_bb: Vec::new(),
            net_cost: Vec::new(),
            cell_nets: Adjacency::default(),
            fixed: vec![None; ncells],
            pinned: Vec::new(),
            movable: Vec::new(),
        };

        let is_global = |n: NetId| on_global(nl, n);
        let raw = |n: Option<NetId>| n.map_or(NO_NET, |n| n.raw());
        let mut sets = Vec::new();
        for c in nl.live_cells() {
            if nl.cell_kind(c) != LC {
                continue;
            }
            let p = |name: &str| nl.param(c, name).and_then(Param::as_u64).unwrap_or(0) != 0;
            let mut info = LcInfo::default();
            if p("DFF_ENABLE") {
                let ctrl = [nl.pin_net(c, "CLK"), nl.pin_net(c, "CEN"), nl.pin_net(c, "SR")];
                let n = |n: Option<NetId>| n.map_or(0, |n| n.index() + 1);
                sets.push(((n(ctrl[0]), n(ctrl[1]), n(ctrl[2]), p("NEG_CLK")), c));
                info.fabric_ctrl = ctrl.map(|n| raw(n.filter(|&n| !is_global(n))));
            }
            let cin = nl.pin_net(c, "CIN");
            info.even_pins = [raw(nl.pin_net(c, "I0")), raw(nl.pin_net(c, "I2"))];
            info.odd_pins =
                [raw(nl.pin_net(c, "I1")), raw(nl.pin_net(c, "I3").filter(|&n| Some(n) != cin))];
            sa.lc_info[c.index()] = info;
        }
        sets.sort_unstable();
        let mut set = 0;
        for (i, &(key, c)) in sets.iter().enumerate() {
            if i == 0 || key != sets[i - 1].0 {
                set += 1;
            }
            sa.lc_info[c.index()].ctrl = set;
        }
        // Units: chains first, then single logic cells and RAMs.
        let mut in_chain = vec![false; ncells];
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
                in_chain[c.index()] = true;
                sa.cell_unit[c.index()] = sa.units.len() as u32;
            }
            sa.units.push(Unit::Chain(chain));
        }
        for c in nl.live_cells() {
            match nl.cell_kind(c) {
                LC if !in_chain[c.index()] => {
                    sa.cell_unit[c.index()] = sa.units.len() as u32;
                    sa.units.push(Unit::Lc(c));
                }
                RAM => {
                    sa.cell_unit[c.index()] = sa.units.len() as u32;
                    sa.units.push(Unit::Ram(c));
                }
                _ => {}
            }
        }
        // Nets for the wirelength cost, skipping global networks.
        let mut nets = Vec::new();
        let mut cell_nets: Vec<Vec<u32>> = vec![Vec::new(); ncells];
        for n in nl.live_nets() {
            let net = &nl.nets[n];
            if is_global(n) {
                continue;
            }
            let mut cells: Vec<u32> =
                net.driver.iter().chain(&net.users).map(|p| p.cell.raw()).collect();
            cells.sort();
            cells.dedup();
            if cells.len() < 2 {
                continue;
            }
            let id = nets.len() as u32;
            for &c in &cells {
                cell_nets[c as usize].push(id);
            }
            nets.push(cells);
        }
        sa.nets = Adjacency::new(nets);
        sa.cell_nets = Adjacency::new(cell_nets);
        sa.net_cost = vec![0.0; sa.nets.len()];
        sa.net_seen = vec![0; sa.nets.len()];
        // Connections for the timing cost (not on globals, not inside carry
        // chains, whose cells move together).
        let mut cell_conns: Vec<Vec<u32>> = vec![Vec::new(); ncells];
        for n in nl.live_nets() {
            let net = &nl.nets[n];
            let Some(d) = net.driver else { continue };
            if is_global(n) {
                continue;
            }
            let dpin = nl.str(nl.cells[d.cell].pins[d.pin as usize].name);
            for p in &net.users {
                let spin = nl.str(nl.cells[p.cell].pins[p.pin as usize].name);
                if p.cell == d.cell || (dpin == "COUT" && spin == "CIN") {
                    continue;
                }
                let id = sa.conns.len() as u32;
                if let Some(t) = sa.timing.conn_to(p.cell, p.pin as usize) {
                    sa.conn_of_timing[t] = id;
                }
                sa.conns.push(Conn { drv: d.cell, sink: p.cell });
                cell_conns[d.cell.index()].push(id);
                cell_conns[p.cell.index()].push(id);
            }
        }
        sa.cell_conns = Adjacency::new(cell_conns);
        sa.conn_weight = vec![0.0; sa.conns.len()];
        sa.conn_cost = vec![0.0; sa.conns.len()];
        sa.conn_seen = vec![0; sa.conns.len()];
        Ok(sa)
    }

    /// Places cell `c` at `p`.
    #[inline]
    fn set_pos(&mut self, c: CellId, p: (i16, i16, u8)) {
        self.pos[c.index()] = Some(p);
        self.xy[c.index()] = (p.0, p.1);
    }

    #[inline]
    fn pos_of(&self, c: CellId) -> (i16, i16, u8) {
        self.pos[c.index()].expect("placed cell")
    }

    #[inline]
    fn unit_of(&self, c: CellId) -> usize {
        self.cell_unit[c.index()] as usize
    }

    fn conn_delay(&self, k: usize) -> f64 {
        let c = self.conns[k];
        let (x0, y0) = self.cell_xy(c.drv);
        let (x1, y1) = self.cell_xy(c.sink);
        self.delays.at((x1 - x0) as i32, (y1 - y0) as i32)
    }

    /// Recomputes connection criticalities from the current placement and
    /// renormalises the cost terms.
    fn update_timing(&mut self) {
        if self.opts.timing_weight <= 0.0 || self.conns.is_empty() {
            return;
        }
        let nl = self.nl;
        // Placed connections by distance; carry chains are free, globals a
        // fixed delay.
        let delay: Vec<f64> = (0..self.timing.conns.len())
            .map(|t| match self.conn_of_timing[t] {
                u32::MAX => {
                    let c = self.timing.conns[t];
                    let n = nl.cells[c.sink].pins[c.pin].net;
                    if n.is_some_and(|n| {
                        nl.nets[n].driver.is_some_and(|d| nl.cell_kind(d.cell) == GB)
                    }) {
                        1500.0
                    } else {
                        0.0
                    }
                }
                k => self.conn_delay(k as usize),
            })
            .collect();
        let crit = self.timing.analyse(&delay).criticality;
        for (t, &k) in self.conn_of_timing.iter().enumerate() {
            if k != u32::MAX {
                let k = k as usize;
                self.conn_weight[k] = crit[t].powi(8);
                self.conn_cost[k] = self.conn_weight[k] * self.conn_delay(k);
            }
        }
        self.bb_norm = self.net_cost.iter().sum::<f64>().max(1.0);
        self.t_norm = self.conn_cost.iter().sum::<f64>().max(1.0);
    }

    /// Normalised cost (wirelength and timing terms weighted).
    fn norm_cost(&self) -> f64 {
        let l = self.opts.timing_weight;
        let t: f64 = self.conn_cost.iter().sum();
        (1.0 - l) * self.total_cost() / self.bb_norm + l * t / self.t_norm
    }

    fn set_fixed(&mut self, placement: &Placement) {
        for (&c, &bel) in &placement.cell_bel {
            let b = self.arch.bel(bel);
            self.fixed[c.index()] = Some((b.x as i16, b.y as i16));
            if self.pos[c.index()].is_none() {
                self.xy[c.index()] = (b.x as i16, b.y as i16);
            }
            // Logic cells placed beforehand stay where they are.
            if b.kind == BelKind::Lc && self.cell_unit[c.index()] != NONE {
                let u = self.unit_of(c);
                self.units[u] = Unit::Pinned(c);
                self.pinned.push((c, (b.x as i16, b.y as i16, b.z)));
            }
        }
    }

    #[inline]
    fn slot(&self, x: usize, y: usize, z: usize) -> usize {
        (x * (self.height + 2) + y) * 8 + z
    }

    /// Whether the logic cells placed in tile (x, y) can share it: one
    /// control set, and enough local tracks. A tile has 16 local tracks of
    /// each of two classes: control pins not on a global and `I0`/`I2` of
    /// even cells (`I1`/`I3` of odd cells) take class A, the other inputs
    /// class B (a net reaching several pins of a class needs one track).
    fn tile_ok(&self, x: usize, y: usize) -> bool {
        let mut ctrl = 0;
        let mut a = [NO_NET; 19];
        let mut b = [NO_NET; 16];
        let (mut na, mut nb) = (0, 0);
        let base = self.slot(x, y, 0);
        for z in 0..8 {
            let Some(c) = self.lc_occ[base + z] else { continue };
            let i = &self.lc_info[c.index()];
            if i.ctrl != 0 {
                if ctrl == 0 {
                    ctrl = i.ctrl;
                    for &n in &i.fabric_ctrl {
                        if n != NO_NET && !a[..na].contains(&n) {
                            a[na] = n;
                            na += 1;
                        }
                    }
                } else if ctrl != i.ctrl {
                    return false;
                }
            }
            let (ca, cb) =
                if z % 2 == 0 { (&i.even_pins, &i.odd_pins) } else { (&i.odd_pins, &i.even_pins) };
            for &n in ca {
                if n != NO_NET && !a[..na].contains(&n) {
                    a[na] = n;
                    na += 1;
                }
            }
            for &n in cb {
                if n != NO_NET && !b[..nb].contains(&n) {
                    b[nb] = n;
                    nb += 1;
                }
            }
        }
        let half = self.opts.max_locals / 2;
        na <= half && nb <= half
    }

    #[inline]
    fn cell_xy(&self, c: CellId) -> (i16, i16) {
        self.xy[c.index()]
    }

    /// The bounding box of net `n` where its cells are now.
    fn scan_bb(&self, n: usize) -> BBox {
        BBox::of(self.nets.of(n).iter().map(|&c| self.xy[c as usize])).expect("nets have cells")
    }

    fn bb_cost(&self, n: usize, bb: &BBox) -> f64 {
        net_weight(self.nets.of(n).len()) * bb.half_perimeter()
    }

    /// Bounding boxes and costs of every net from scratch.
    fn reset_costs(&mut self) {
        self.net_bb = (0..self.nets.len()).map(|n| self.scan_bb(n)).collect();
        self.net_cost = (0..self.nets.len()).map(|n| self.bb_cost(n, &self.net_bb[n])).collect();
    }

    /// The bounding box of net `n` after the move being tried: updated for
    /// each moved cell of a large net, or scanned.
    fn moved_bb(&self, n: usize) -> BBox {
        let cells = self.nets.of(n);
        if cells.len() <= SCANNED_NET {
            return self.scan_bb(n);
        }
        let mut bb = self.net_bb[n];
        for &(c, from) in &self.moved {
            if cells.binary_search(&c.raw()).is_ok() && !bb.shift(from, self.xy[c.index()]) {
                return self.scan_bb(n);
            }
        }
        bb
    }

    fn initial_placement(&mut self) -> Result<(), String> {
        for (c, (x, y, z)) in self.pinned.clone() {
            let s = self.slot(x as usize, y as usize, z as usize);
            self.lc_occ[s] = Some(c);
            self.set_pos(c, (x, y, z));
        }
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
                let free = |sa: &Self| {
                    (0..cells.len())
                        .all(|i| sa.lc_occ[sa.slot(x, col_fill[x] + i / 8, i % 8)].is_none())
                };
                if col_fill[x] + tiles - 1 <= self.height && free(self) {
                    let y0 = col_fill[x];
                    for (i, &c) in cells.iter().enumerate() {
                        let (y, z) = (y0 + i / 8, i % 8);
                        let s = self.slot(x, y, z);
                        self.lc_occ[s] = Some(c);
                        self.set_pos(c, (x as i16, y as i16, z as u8));
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
        // Single cells: flip-flops grouped by control set (a tile has one),
        // so each group fills tiles densely; cells without a flip-flop last,
        // in the slots left. Tiles in a random order, each cell going to
        // the current tile if it fits there.
        let mut tiles: Vec<(usize, usize)> =
            self.logic_cols.iter().flat_map(|&x| (1..=self.height).map(move |y| (x, y))).collect();
        self.rng.shuffle(&mut tiles);
        let key = |c: CellId, sa: &Self| {
            let ctrl = sa.lc_info[c.index()].ctrl;
            (ctrl == 0, ctrl)
        };
        let mut singles: Vec<CellId> = units
            .iter()
            .filter_map(|u| if let Unit::Lc(c) = u { Some(*c) } else { None })
            .collect();
        singles.sort_by_key(|&c| (key(c, self), c));
        let mut ti = 0usize;
        for c in singles {
            let mut placed = false;
            'tiles: for attempt in 0..tiles.len() {
                let (x, y) = tiles[(ti + attempt) % tiles.len()];
                for z in 0..8 {
                    let s = self.slot(x, y, z);
                    if self.lc_occ[s].is_some() {
                        continue;
                    }
                    self.lc_occ[s] = Some(c);
                    if self.tile_ok(x, y) {
                        self.set_pos(c, (x as i16, y as i16, z as u8));
                        placed = true;
                        ti = (ti + attempt) % tiles.len();
                        break 'tiles;
                    }
                    self.lc_occ[s] = None;
                }
            }
            if !placed {
                return Err("design does not fit: no legal location for a logic cell".into());
            }
        }
        for unit in &units {
            if let Unit::Ram(c) = unit {
                let slot = self
                    .ram_occ
                    .iter()
                    .position(|o| o.is_none())
                    .ok_or("design needs more block RAMs than available")?;
                self.ram_occ[slot] = Some(*c);
                self.cell_ram_slot[c.index()] = slot as u32;
                let b = self.arch.bel(self.ram_bels[slot]);
                self.set_pos(*c, (b.x as i16, b.y as i16, 0));
            }
        }
        self.reset_costs();
        Ok(())
    }

    /// Atomically occupy a legal site for a complete placement unit. Reused
    /// chains must retain their order, column and slot-zero anchor.
    fn seed_unit(&mut self, unit: &Unit, bels: &[BelId]) -> bool {
        if bels.len() != unit.cells().len()
            || bels.is_empty()
            || bels.iter().any(|b| b.index() >= self.arch.bels.len())
        {
            return false;
        }
        if let Unit::Ram(c) = unit {
            let Some(slot) = self.ram_bels.iter().position(|b| *b == bels[0]) else { return false };
            if self.ram_occ[slot].is_some() {
                return false;
            }
            let b = self.arch.bel(bels[0]);
            self.ram_occ[slot] = Some(*c);
            self.cell_ram_slot[c.index()] = slot as u32;
            self.set_pos(*c, (b.x as i16, b.y as i16, b.z));
            return true;
        }
        let first = self.arch.bel(bels[0]);
        let mut slots = Vec::new();
        let mut tiles = Vec::new();
        for (i, &bel) in bels.iter().enumerate() {
            let b = self.arch.bel(bel);
            if b.kind != BelKind::Lc
                || (matches!(unit, Unit::Chain(_))
                    && (b.x != first.x
                        || b.y as usize != first.y as usize + i / 8
                        || b.z as usize != i % 8))
            {
                return false;
            }
            let slot = self.slot(b.x as usize, b.y as usize, b.z as usize);
            if self.lc_occ[slot].is_some() || slots.contains(&slot) {
                return false;
            }
            slots.push(slot);
            tiles.push((b.x as usize, b.y as usize));
        }
        for (&slot, &c) in slots.iter().zip(unit.cells()) {
            self.lc_occ[slot] = Some(c);
        }
        if !tiles.iter().all(|&(x, y)| self.tile_ok(x, y)) {
            for slot in slots {
                self.lc_occ[slot] = None;
            }
            return false;
        }
        for (&c, &bel) in unit.cells().iter().zip(bels) {
            let b = self.arch.bel(bel);
            self.set_pos(c, (b.x as i16, b.y as i16, b.z));
        }
        true
    }

    fn repair_placement(&mut self, previous: &Placement) -> Result<usize, String> {
        for (c, (x, y, z)) in self.pinned.clone() {
            let s = self.slot(x as usize, y as usize, z as usize);
            self.lc_occ[s] = Some(c);
            self.set_pos(c, (x, y, z));
        }
        let units = self.units.clone();
        let mut reused = 0;
        for unit in units.iter().filter(|u| !matches!(u, Unit::Pinned(_))) {
            let bels: Option<Vec<_>> =
                unit.cells().iter().map(|c| previous.cell_bel.get(c).copied()).collect();
            if let Some(bels) = bels
                && self.seed_unit(unit, &bels)
            {
                reused += unit.cells().len();
            }
        }
        let mut placed_anew = Vec::new();
        for (u, unit) in units.iter().enumerate() {
            if unit.cells().iter().all(|c| self.pos[c.index()].is_some()) {
                continue;
            }
            placed_anew.push(u);
            // Prefer sites near already placed neighbors; retain unchanged
            // units during this cheap repair attempt. The flow retries fresh
            // placement if the remaining sites cannot accommodate the edit.
            let mut neighbors = Vec::new();
            for c in unit.cells() {
                for &n in self.cell_nets.of(c.index()) {
                    for &other in self.nets.of(n as usize) {
                        let o = other as usize;
                        if self.pos[o].is_some() || self.fixed[o].is_some() {
                            neighbors.push(self.xy[o]);
                        }
                    }
                }
            }
            let center = if neighbors.is_empty() {
                (self.width as i16 / 2, self.height as i16 / 2)
            } else {
                (
                    (neighbors.iter().map(|p| p.0 as i64).sum::<i64>() / neighbors.len() as i64)
                        as i16,
                    (neighbors.iter().map(|p| p.1 as i64).sum::<i64>() / neighbors.len() as i64)
                        as i16,
                )
            };
            let kind = if matches!(unit, Unit::Ram(_)) { BelKind::Ram } else { BelKind::Lc };
            let mut candidates: Vec<_> = self.arch.bels_of(kind).collect();
            candidates.sort_by_key(|&id| {
                let b = self.arch.bel(id);
                ((b.x as i16 - center.0).abs() + (b.y as i16 - center.1).abs(), id)
            });
            let mut placed = false;
            for bel in candidates {
                let b = self.arch.bel(bel);
                let bels = if matches!(unit, Unit::Chain(_)) {
                    if b.z != 0 {
                        continue;
                    }
                    (0..unit.cells().len())
                        .map(|i| {
                            let y = b.y as usize + i / 8;
                            if y > self.height {
                                None
                            } else {
                                self.arch.bel_at(b.x, y as u8, (i % 8) as u8, kind)
                            }
                        })
                        .collect::<Option<Vec<_>>>()
                } else {
                    Some(vec![bel])
                };
                if let Some(bels) = bels
                    && self.seed_unit(unit, &bels)
                {
                    placed = true;
                    break;
                }
            }
            if !placed {
                return Err("incremental placement has no legal site for an edited unit".into());
            }
        }
        // Only the units placed anew move: moving kept ones would void
        // their routes.
        self.movable = placed_anew;
        Ok(reused)
    }

    fn total_cost(&self) -> f64 {
        self.net_cost.iter().sum()
    }

    /// Recomputes costs of the nets (and connections) touching the cells of
    /// `self.moved`; returns the normalised cost delta. New costs are
    /// applied with [`Annealer::commit`] if the move is accepted.
    fn delta(&mut self) -> f64 {
        let stamp = self.next_stamp();
        let mut scratch = std::mem::take(&mut self.net_scratch);
        scratch.clear();
        let mut d = 0.0;
        for &(c, _) in &self.moved {
            for &n in self.cell_nets.of(c.index()) {
                if std::mem::replace(&mut self.net_seen[n as usize], stamp) == stamp {
                    continue;
                }
                let bb = self.moved_bb(n as usize);
                d += self.bb_cost(n as usize, &bb) - self.net_cost[n as usize];
                scratch.push((n, bb));
            }
        }
        self.net_scratch = scratch;
        let l = self.opts.timing_weight;
        let mut dt = 0.0;
        let mut cs = std::mem::take(&mut self.conn_scratch);
        cs.clear();
        if l > 0.0 {
            for &(c, _) in &self.moved {
                for &k in self.cell_conns.of(c.index()) {
                    if self.conn_weight[k as usize] == 0.0
                        || std::mem::replace(&mut self.conn_seen[k as usize], stamp) == stamp
                    {
                        continue;
                    }
                    let new = self.conn_weight[k as usize] * self.conn_delay(k as usize);
                    dt += new - self.conn_cost[k as usize];
                    cs.push((k, new));
                }
            }
        }
        self.conn_scratch = cs;
        (1.0 - l) * d / self.bb_norm + l * dt / self.t_norm
    }

    /// A fresh mark for [`Annealer::net_seen`] and the like.
    fn next_stamp(&mut self) -> u32 {
        self.stamp = self.stamp.wrapping_add(1);
        if self.stamp == 0 {
            self.net_seen.fill(0);
            self.conn_seen.fill(0);
            self.cell_seen.fill(0);
            self.stamp = 1;
        }
        self.stamp
    }

    fn commit(&mut self) {
        for i in 0..self.net_scratch.len() {
            let (n, bb) = self.net_scratch[i];
            self.net_cost[n as usize] = self.bb_cost(n as usize, &bb);
            self.net_bb[n as usize] = bb;
        }
        for &(k, cost) in &self.conn_scratch {
            self.conn_cost[k as usize] = cost;
        }
    }

    /// Half of the time: a random point in the median region of the
    /// bounding boxes of `cells`' nets without them (VPR's median move),
    /// where moving the cells most shortens their nets.
    fn median_target(&mut self, cells: &[CellId]) -> Option<(usize, usize)> {
        if self.rng.below(2) == 0 {
            return None;
        }
        let (mut xs, mut ys) = (std::mem::take(&mut self.xs), std::mem::take(&mut self.ys));
        xs.clear();
        ys.clear();
        let stamp = self.next_stamp();
        for c in cells {
            self.cell_seen[c.index()] = stamp;
        }
        for c in cells {
            for &n in self.cell_nets.of(c.index()) {
                let net = self.nets.of(n as usize);
                if net.len() > 64 {
                    continue;
                }
                // The net's box without a single cell is its box, unless
                // the cell alone is on a side of it.
                let bb = self.net_bb[n as usize];
                let alone = |&c: &CellId| {
                    let (x, y) = self.xy[c.index()];
                    (x == bb.x0 && bb.on_x0 == 1)
                        || (x == bb.x1 && bb.on_x1 == 1)
                        || (y == bb.y0 && bb.on_y0 == 1)
                        || (y == bb.y1 && bb.on_y1 == 1)
                };
                let bb = if let [c] = cells
                    && !alone(c)
                {
                    Some(bb)
                } else {
                    let others = net.iter().filter(|&&o| self.cell_seen[o as usize] != stamp);
                    BBox::of(others.map(|&o| self.xy[o as usize]))
                };
                if let Some(bb) = bb {
                    xs.extend([bb.x0, bb.x1]);
                    ys.extend([bb.y0, bb.y1]);
                }
            }
        }
        let target = (!xs.is_empty()).then(|| {
            xs.sort_unstable();
            ys.sort_unstable();
            let m = xs.len() / 2;
            let pick = |rng: &mut Rng, v: &[i16], hi: usize| {
                rng.range_inclusive(v[m - 1] as i64, v[m] as i64).clamp(1, hi as i64) as usize
            };
            (pick(&mut self.rng, &xs, self.width), pick(&mut self.rng, &ys, self.height))
        });
        (self.xs, self.ys) = (xs, ys);
        target
    }

    /// The annealing rule's decision on the move being tried.
    fn decide(&mut self, temp: f64) -> Decision {
        let d = self.delta();
        if d == 0.0 {
            Decision::Free
        } else if d < 0.0 || self.rng.unit() < (-d / temp).exp() {
            Decision::Accepted
        } else {
            Decision::Rejected
        }
    }

    /// Tries one move; returns whether it was accepted, or `None` for a
    /// move the schedule does not count: illegal, or changing no cost. The
    /// acceptance rate tells how hot the annealing is; illegal moves (most
    /// at first, to tiles of another control set) would hold it near half
    /// from the first temperature on, and moves within a tile, which cost
    /// nothing, would keep it there to the end.
    fn try_move(&mut self, temp: f64, rlim: f64) -> Option<bool> {
        let u = if self.movable.is_empty() {
            self.rng.below(self.units.len())
        } else {
            self.movable[self.rng.below(self.movable.len())]
        };
        let r = rlim.max(1.0) as i64;
        match self.units[u] {
            Unit::Pinned(_) => None,
            Unit::Lc(c) => {
                let (x, y, z) = self.pos_of(c);
                let (nx, ny) = match self.median_target(&[c]) {
                    Some(t) => t,
                    None => (
                        self.rng.range_inclusive(
                            (x as i64 - r).max(1),
                            (x as i64 + r).min(self.width as i64),
                        ) as usize,
                        self.rng.range_inclusive(
                            (y as i64 - r).max(1),
                            (y as i64 + r).min(self.height as i64),
                        ) as usize,
                    ),
                };
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
                    && !matches!(self.units[self.unit_of(o)], Unit::Lc(_))
                {
                    return None;
                }
                // Apply.
                self.lc_occ[s_new] = Some(c);
                self.lc_occ[s_old] = other;
                self.set_pos(c, (nx as i16, ny as i16, nz as u8));
                if let Some(o) = other {
                    self.set_pos(o, (ox as i16, oy as i16, oz as u8));
                }
                let legal = self.tile_ok(nx, ny) && ((ox, oy) == (nx, ny) || self.tile_ok(ox, oy));
                self.moved.clear();
                self.moved.push((c, (x, y)));
                self.moved.extend(other.map(|o| (o, (nx as i16, ny as i16))));
                let decision = legal.then(|| self.decide(temp));
                let accept = decision.is_some_and(|d| d != Decision::Rejected);
                if accept {
                    self.commit();
                } else {
                    self.lc_occ[s_old] = Some(c);
                    self.lc_occ[s_new] = other;
                    self.set_pos(c, (x, y, z));
                    if let Some(o) = other {
                        self.set_pos(o, (nx as i16, ny as i16, nz as u8));
                    }
                }
                decision.and_then(Decision::counted)
            }
            Unit::Chain(_) => {
                let Unit::Chain(cells) = std::mem::replace(&mut self.units[u], Unit::Chain(vec![]))
                else {
                    unreachable!()
                };
                let result = self.try_chain_move(u, &cells, r, temp);
                self.units[u] = Unit::Chain(cells);
                result
            }
            Unit::Ram(c) => {
                if self.ram_bels.len() < 2 {
                    return None;
                }
                let s_old = self.cell_ram_slot[c.index()] as usize;
                let s_new = self.rng.below(self.ram_bels.len());
                if s_new == s_old {
                    return None;
                }
                let other = self.ram_occ[s_new];
                let place = |sa: &mut Self, cell: CellId, slot: usize| {
                    sa.ram_occ[slot] = Some(cell);
                    sa.cell_ram_slot[cell.index()] = slot as u32;
                    let b = sa.arch.bel(sa.ram_bels[slot]);
                    sa.set_pos(cell, (b.x as i16, b.y as i16, 0));
                };
                let at = |sa: &Self, slot: usize| {
                    let b = sa.arch.bel(sa.ram_bels[slot]);
                    (b.x as i16, b.y as i16)
                };
                self.moved.clear();
                self.moved.push((c, at(self, s_old)));
                self.moved.extend(other.map(|o| (o, at(self, s_new))));
                place(self, c, s_new);
                self.ram_occ[s_old] = None;
                if let Some(o) = other {
                    place(self, o, s_old);
                }
                let decision = self.decide(temp);
                let accept = decision != Decision::Rejected;
                if accept {
                    self.commit();
                } else {
                    place(self, c, s_old);
                    self.ram_occ[s_new] = None;
                    if let Some(o) = other {
                        place(self, o, s_new);
                    }
                }
                decision.counted()
            }
        }
    }

    /// Tries moving the carry chain of unit `u` (its `cells`, taken out of
    /// `units` meanwhile), swapping it with a chain in the way.
    fn try_chain_move(&mut self, u: usize, cells: &[CellId], r: i64, temp: f64) -> Option<bool> {
        let (x, y, _) = self.pos_of(cells[0]);
        let tiles = cells.len().div_ceil(8);
        let max_y0 = self.height + 1 - tiles;
        let (nx, ny) = match self.median_target(cells) {
            // The nearest logic column; the chain centred on the target.
            Some((tx, ty)) => {
                let &col =
                    self.logic_cols.iter().min_by_key(|&&c| (c as i64 - tx as i64).abs()).unwrap();
                (col, (ty as i64 - tiles as i64 / 2).clamp(1, max_y0 as i64) as usize)
            }
            None => {
                let col = self.rng.below(self.logic_cols.len());
                let nx = self.logic_cols[col];
                if (nx as i64 - x as i64).abs() > r {
                    return None;
                }
                let ny = self
                    .rng
                    .range_inclusive((y as i64 - r).max(1), (y as i64 + r).min(max_y0 as i64))
                    as usize;
                (nx, ny)
            }
        };
        if (nx, ny) == (x as usize, y as usize) {
            return None;
        }
        // A chain in the way swaps places with this one.
        let mut others: Vec<usize> = (0..cells.len())
            .filter_map(|i| self.lc_occ[self.slot(nx, ny + i / 8, i % 8)])
            .map(|o| self.unit_of(o))
            .filter(|&v| v != u && matches!(self.units[v], Unit::Chain(_)))
            .collect();
        others.sort_unstable();
        others.dedup();
        match others[..] {
            [] => self.move_chains(&[(cells, nx, ny)], temp),
            [v] => {
                let Unit::Chain(other) = &self.units[v] else { unreachable!() };
                if y as usize + other.len().div_ceil(8) > self.height + 1 {
                    return None;
                }
                let other = other.clone();
                self.move_chains(&[(cells, nx, ny), (&other, x as usize, y as usize)], temp)
            }
            _ => None,
        }
    }

    /// Moves carry chains to new anchors (column, bottom row), as one move
    /// accepted or rejected by the annealing rule. Their target slots must
    /// be free, belong to a moving chain or hold single logic cells, which
    /// move to the slots the chains leave.
    fn move_chains(&mut self, moves: &[(&[CellId], usize, usize)], temp: f64) -> Option<bool> {
        let moving: Vec<CellId> = moves.iter().flat_map(|m| m.0.iter().copied()).collect();
        let mut target: Vec<usize> = Vec::with_capacity(moving.len());
        for (cells, nx, ny) in moves {
            target.extend((0..cells.len()).map(|i| self.slot(*nx, ny + i / 8, i % 8)));
        }
        let mut sorted = target.clone();
        sorted.sort_unstable();
        if sorted.windows(2).any(|w| w[0] == w[1]) {
            return None;
        }
        let mut displaced: Vec<(CellId, (i16, i16, u8))> = Vec::new();
        for &s in &target {
            if let Some(o) = self.lc_occ[s]
                && !moving.contains(&o)
            {
                if !matches!(self.units[self.unit_of(o)], Unit::Lc(_)) {
                    return None;
                }
                displaced.push((o, self.pos_of(o)));
            }
        }
        let old: Vec<(i16, i16, u8)> = moving.iter().map(|&c| self.pos_of(c)).collect();
        let slot_of =
            |sa: &Self, p: (i16, i16, u8)| sa.slot(p.0 as usize, p.1 as usize, p.2 as usize);
        let at = |s: usize, sa: &Self| {
            let z = s % 8;
            let y = (s / 8) % (sa.height + 2);
            let x = s / 8 / (sa.height + 2);
            (x as i16, y as i16, z as u8)
        };
        for &(_, p) in &displaced {
            let s = slot_of(self, p);
            self.lc_occ[s] = None;
        }
        for (i, &c) in moving.iter().enumerate() {
            let s = slot_of(self, old[i]);
            if self.lc_occ[s] == Some(c) {
                self.lc_occ[s] = None;
            }
        }
        for (i, &c) in moving.iter().enumerate() {
            self.lc_occ[target[i]] = Some(c);
            self.set_pos(c, at(target[i], self));
        }
        // Displaced cells fill the chains' old slots that are free now.
        let vacated: Vec<(i16, i16, u8)> =
            old.iter().copied().filter(|&p| self.lc_occ[slot_of(self, p)].is_none()).collect();
        for (k, &(o, _)) in displaced.iter().enumerate() {
            let p = vacated[k];
            let s = slot_of(self, p);
            self.lc_occ[s] = Some(o);
            self.set_pos(o, p);
        }
        let mut touched: Vec<(usize, usize)> = target
            .iter()
            .map(|&s| at(s, self))
            .chain(vacated.iter().take(displaced.len()).copied())
            .map(|p| (p.0 as usize, p.1 as usize))
            .collect();
        touched.sort_unstable();
        touched.dedup();
        let legal = touched.iter().all(|&(tx, ty)| self.tile_ok(tx, ty));
        self.moved.clear();
        self.moved.extend(moving.iter().zip(&old).map(|(&c, p)| (c, (p.0, p.1))));
        self.moved.extend(displaced.iter().map(|&(o, p)| (o, (p.0, p.1))));
        let decision = legal.then(|| self.decide(temp));
        let accept = decision.is_some_and(|d| d != Decision::Rejected);
        if accept {
            self.commit();
        } else {
            for p in vacated.iter().take(displaced.len()) {
                let s = slot_of(self, *p);
                self.lc_occ[s] = None;
            }
            for &s in &target {
                self.lc_occ[s] = None;
            }
            for (i, &c) in moving.iter().enumerate() {
                let s = slot_of(self, old[i]);
                self.lc_occ[s] = Some(c);
                self.set_pos(c, old[i]);
            }
            for &(o, p) in &displaced {
                let s = slot_of(self, p);
                self.lc_occ[s] = Some(o);
                self.set_pos(o, p);
            }
        }
        decision.and_then(Decision::counted)
    }

    fn anneal(&mut self, log: &mut Vec<String>) {
        self.anneal_from(log, None);
    }

    /// Anneals from scratch, or with `refine` (a starting temperature times
    /// the number of nets) refines the current placement: a cold start with
    /// short moves, which keeps its structure.
    fn anneal_from(&mut self, log: &mut Vec<String>, refine: Option<f64>) {
        if self.units.is_empty() || self.nets.is_empty() {
            return;
        }
        let n = if self.movable.is_empty() { self.units.len() } else { self.movable.len() } as f64;
        let moves_per_temp = ((self.opts.effort * n.powf(4.0 / 3.0)) as usize).max(100);
        let nets = self.nets.len() as f64;
        let start = self.total_cost();
        self.bb_norm = start.max(1.0);
        let (mut temp, mut rlim) = match refine {
            Some(t) => {
                self.update_timing();
                (t / nets, 3.0)
            }
            None => {
                // Initial temperature: standard deviation of random move
                // deltas.
                let rlim = self.width.max(self.height) as f64;
                let mut deltas = Vec::new();
                for _ in 0..self.units.len().min(500) {
                    let before = self.norm_cost();
                    if self.try_move(f64::INFINITY, rlim) == Some(true) {
                        deltas.push(self.norm_cost() - before);
                    }
                }
                let mean = deltas.iter().sum::<f64>() / deltas.len().max(1) as f64;
                let var = deltas.iter().map(|d| (d - mean).powi(2)).sum::<f64>()
                    / deltas.len().max(1) as f64;
                (20.0 * var.sqrt().max(1e-6), rlim)
            }
        };
        let mut iters = 0;
        // Progress: how far the temperature has fallen toward the end of
        // annealing, on a log scale.
        let (first, end) = (temp, 0.005 / nets);
        let mut reported = 0.0f64;
        loop {
            // Criticalities follow the placement; normalise the cost terms.
            self.update_timing();
            if self.opts.timing_weight <= 0.0 {
                self.bb_norm = self.total_cost().max(1.0);
            }
            let (mut tried, mut accepted) = (0usize, 0usize);
            for _ in 0..moves_per_temp {
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
            rlim = (rlim * (1.0 - 0.44 + rate)).clamp(1.0, self.width.max(self.height) as f64);
            iters += 1;
            if temp < end || iters > 2000 {
                break;
            }
            let cooled = (first / temp).ln() / (first / end).ln().max(1e-9);
            reported = reported.max(cooled.max(iters as f64 / 2000.0));
            synth_core::progress::advance(0.95 * reported, || {
                format!("annealing: step {iters}, {:.0}% of moves accepted", 100.0 * rate)
            });
        }
        // Final greedy pass at zero temperature.
        for _ in 0..moves_per_temp * 2 {
            self.try_move(1e-9, 3.0);
        }
        // Re-sum to avoid accumulated rounding.
        self.reset_costs();
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
        for (i, p) in self.pos.iter().enumerate() {
            let Some((x, y, z)) = *p else { continue };
            let c = CellId::new(i);
            let kind = if matches!(self.units[self.unit_of(c)], Unit::Ram(_)) {
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

/// Least share of cells an incremental placement must keep in place.
const MIN_REUSE: f64 = 0.6;

/// Starting temperature (times the number of nets) of the annealing that
/// refines the units an incremental placement placed anew.
const REFINE_TEMPERATURE: f64 = 0.5;

/// Share of cells placed anew above which the whole placement is refined,
/// from this starting temperature.
const WIDE_EDIT: f64 = 0.05;
const REFINE_TEMPERATURE_WIDE: f64 = 0.05;

/// Retains legal complete units and places edited units in remaining sites.
/// Does not anneal retained cells; failure asks the caller for a fresh run.
pub(crate) fn place_incremental(
    arch: &Arch,
    nl: &Netlist,
    packed: &Packed,
    placement: &mut Placement,
    opts: &PlaceOptions,
    previous: &Placement,
    log: &mut Vec<String>,
) -> Result<(), String> {
    let mut sa = Annealer::new(arch, nl, packed, opts)?;
    sa.set_fixed(placement);
    let reused = sa.repair_placement(previous)?;
    let total = sa.pos.iter().filter(|p| p.is_some()).count();
    log.push(format!("incremental placement: reused {reused}/{total} logic/RAM cells"));
    // Repairing a mostly new design costs more than placing it afresh, and
    // places it worse.
    if (reused as f64) < MIN_REUSE * total as f64 {
        return Err(format!(
            "only {:.0}% of the cells can stay in place",
            100.0 * reused as f64 / total.max(1) as f64
        ));
    }
    sa.reset_costs();
    // A small edit refines only the units placed anew; a wider one, whose
    // new units need room the kept ones hold, refines everything (colder).
    if !sa.movable.is_empty() {
        if (reused as f64) < (1.0 - WIDE_EDIT) * total as f64 {
            sa.movable.clear();
            sa.anneal_from(log, Some(REFINE_TEMPERATURE_WIDE));
        } else {
            sa.anneal_from(log, Some(REFINE_TEMPERATURE));
        }
    }
    sa.finish(placement);
    Ok(())
}
