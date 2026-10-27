//! Routing: clocks on the global network, then every other connection by
//! negotiated congestion (`synth-route`).
//!
//! A clock (a net reaching flip-flop clock pins) enters the global network
//! through a clock buffer (`DCCA`): the buffer nearest its source (by
//! delay, through a dedicated clock pin's path or the fabric's entries to
//! the clock network) becomes one more sink of the source's net, and from
//! the buffer's output a search confined to one of the 16 globals' wires
//! reaches each clock pin: quadrant global, spine, tap, branch, the tile's
//! clock multiplexers. Those wires are then reserved, as nextpnr's global
//! router does it. A LUT's inputs are routed to its logical pins, which
//! each of the LUT's physical inputs reaches; the table is permuted to
//! the inputs the routes take.

use std::collections::{BinaryHeap, VecDeque};

use synth_core::{FxHashMap, FxHashSet};
use synth_netlist::{CellId, NetId, Netlist, OUTPUT_PORT};

use crate::arch::Arch;
use crate::cells::{CCU2C, FF, LUT4};
use crate::graph::NO_TILE;
use crate::pack::{ff_from_lut, pin};
use crate::place::{Placement, Site};

/// Where a port's pad is: `(x, y, pad letter)`.
pub type PadSites = FxHashMap<CellId, (usize, usize, char)>;

/// A clock buffer's position and the global its output takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dcc {
    /// The net the buffer reads.
    pub net: NetId,
    /// Its input and output nodes (`G_CLKI_LDCC0`, `G_CLKO_LDCC0`).
    pub input: u32,
    pub output: u32,
    pub global: usize,
    /// The edges from the output to the clock pins.
    pub edges: Vec<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct Routing {
    /// The edges each net uses.
    pub nets: FxHashMap<NetId, Vec<u32>>,
    pub dccs: Vec<Dcc>,
    /// The physical input (0-3: `A`-`D`) each logical input of a LUT
    /// arrives on.
    pub lut_pins: FxHashMap<CellId, [u8; 4]>,
}

const LUT_PINS: [&str; 4] = ["A", "B", "C", "D"];

/// The LUT or carry slice in the cell of each site (`(x, y, z)`).
fn luts_by_site(nl: &Netlist, p: &Placement) -> FxHashMap<(usize, usize, usize), CellId> {
    let mut m = FxHashMap::default();
    for (&c, s) in &p.sites {
        match nl.cell_kind(c) {
            LUT4 => {
                m.insert((s.x, s.y, s.z), c);
            }
            CCU2C => {
                m.insert((s.x, s.y, s.z), c);
                m.insert((s.x, s.y, s.z + 1), c);
            }
            _ => {}
        }
    }
    m
}

/// A pin's routing node, if it is routed through general routing: the
/// slice-side pin of a LUT input (any physical input may reach it), else
/// the pin's own wire. Dedicated connections (a flip-flop's data from its
/// own LUT, carries) have none.
fn pin_node(
    arch: &Arch,
    nl: &Netlist,
    s: &Site,
    c: CellId,
    pin: &str,
    luts: &FxHashMap<(usize, usize, usize), CellId>,
) -> Result<Option<u32>, String> {
    let (x, y, z) = (s.x, s.y, s.z);
    let wire = match (nl.cell_kind(c), pin) {
        (LUT4, "A" | "B" | "C" | "D") => format!("{pin}{z}_SLICE"),
        (LUT4, "Z") => format!("F{z}_SLICE"),
        (CCU2C, "A0" | "B0" | "A1" | "B1") => {
            let h = (pin.as_bytes()[1] - b'0') as usize;
            format!("{}{}", &pin[..1], z + h)
        }
        (CCU2C, "S0") => format!("F{z}_SLICE"),
        (CCU2C, "S1") => format!("F{}_SLICE", z + 1),
        (CCU2C, _) => return Ok(None),
        (FF, "Q") => format!("Q{z}_SLICE"),
        (FF, "DI") => {
            if ff_from_lut(nl, c, luts.get(&(x, y, z)).copied()) {
                return Ok(None);
            }
            format!("M{z}_SLICE")
        }
        (FF, "CE") => format!("CE{}_SLICE", z / 2),
        (FF, "LSR") => format!("LSR{}_SLICE", z / 2),
        (FF, "CLK") => format!("CLK{}_SLICE", z / 2),
        (kind, pin) => return Err(format!("cannot route pin {pin} of a {kind}")),
    };
    arch.node(x, y, &wire).map(Some)
}

/// A pad's node: the pad's output to the fabric for an input port, its
/// input from the fabric for an output.
fn pad_node(
    arch: &Arch,
    nl: &Netlist,
    c: CellId,
    (x, y, l): (usize, usize, char),
) -> Result<u32, String> {
    let wire = if nl.cell_kind(c) == OUTPUT_PORT {
        format!("PADDO{l}_PIO")
    } else {
        format!("JPADDI{l}_PIO")
    };
    arch.node(x, y, &wire)
}

/// Routes the placed design.
pub fn route(
    arch: &Arch,
    nl: &Netlist,
    placement: &Placement,
    pads: &PadSites,
    log: &mut Vec<String>,
) -> Result<Routing, String> {
    let g = &arch.graph;
    let luts = luts_by_site(nl, placement);
    // Each net's source and sinks.
    let mut source: FxHashMap<NetId, u32> = FxHashMap::default();
    let mut sinks: FxHashMap<NetId, Vec<(u32, CellId, String)>> = FxHashMap::default();
    for n in nl.live_nets() {
        let net = &nl.nets[n];
        for p in net.driver.iter().chain(&net.users) {
            let name = pin(nl, p.cell, p.pin).to_string();
            let node = if let Some(&site) = pads.get(&p.cell) {
                Some(pad_node(arch, nl, p.cell, site)?)
            } else if let Some(s) = placement.sites.get(&p.cell) {
                pin_node(arch, nl, s, p.cell, &name, &luts)?
            } else {
                return Err(format!("{} is not placed", nl.str(nl.cells[p.cell].name)));
            };
            let Some(node) = node else { continue };
            if Some(*p) == net.driver {
                source.insert(n, node);
            } else {
                sinks.entry(n).or_default().push((node, p.cell, name));
            }
        }
    }
    let mut routing = Routing::default();
    // Nodes of the global routes, and the buffers taken.
    let mut used: FxHashSet<u32> = FxHashSet::default();
    let mut buffers: FxHashSet<u32> = FxHashSet::default();
    // Clocks, most clock pins first, on the 16 globals.
    let mut clocks: Vec<(NetId, usize)> = sinks
        .iter()
        .map(|(&n, s)| {
            (n, s.iter().filter(|(_, c, p)| nl.cell_kind(*c) == FF && p == "CLK").count())
        })
        .filter(|&(n, k)| k > 0 && source.contains_key(&n))
        .collect();
    clocks.sort_by_key(|&(n, k)| (std::cmp::Reverse(k), n));
    clocks.truncate(16);
    for (global, &(net, _)) in clocks.iter().enumerate() {
        let src = source[&net];
        let input = nearest_dcc(arch, src, &used, &buffers).ok_or_else(|| {
            format!("no clock buffer reachable from clock {}", nl.str(nl.nets[net].name))
        })?;
        let output = dcc_output(arch, input)?;
        buffers.insert(input);
        used.insert(output);
        let (clock_pins, rest): (Vec<_>, Vec<_>) =
            sinks[&net].iter().cloned().partition(|(_, c, p)| nl.cell_kind(*c) == FF && p == "CLK");
        let edges = route_global(arch, output, global, clock_pins.iter().map(|s| s.0), &mut used)
            .map_err(|e| format!("clock {}: {e}", nl.str(nl.nets[net].name)))?;
        // The rest of the net, and the buffer's input, by general routing.
        let mut rest = rest;
        rest.push((input, net_cell(nl, net), "CLKI".into()));
        sinks.insert(net, rest);
        routing.dccs.push(Dcc { net, input, output, global, edges });
    }
    log.push(format!("{} clocks on the global network", routing.dccs.len()));
    // Everything else.
    let mut ids: Vec<NetId> = sinks.keys().copied().filter(|n| source.contains_key(n)).collect();
    ids.sort_unstable();
    let requests: Vec<synth_route::NetRequest> = ids
        .iter()
        .map(|n| {
            let mut s: Vec<u32> = sinks[n].iter().map(|t| t.0).collect();
            s.sort_unstable();
            s.dedup();
            synth_route::NetRequest { source: source[n], sinks: s, criticality: Vec::new() }
        })
        .collect();
    let blocked: Vec<u32> = used.iter().copied().collect();
    let opts = synth_route::RouteOptions {
        lookahead_radius: arch.dev.info.max_col.max(arch.dev.info.max_row) as i16 + 1,
        ..synth_route::RouteOptions::default()
    };
    let hooks = synth_route::RouteHooks {
        blocked: &blocked,
        node_class: &g.node_class,
        ..Default::default()
    };
    let (routes, stats) = synth_route::route_with(g.route_graph(), &requests, &opts, hooks)
        .map_err(|e| e.to_string())?;
    log.push(format!(
        "routing: {} nets, {} wires, {} iterations",
        requests.len(),
        routes.iter().map(|r| r.edges.len()).sum::<usize>(),
        stats.iterations
    ));
    for (n, r) in ids.iter().zip(routes) {
        routing.nets.insert(*n, r.edges);
    }
    routing.lut_pins = lut_pins(arch, nl, placement, &routing);
    Ok(routing)
}

/// A cell on the net (to name a sink that is no cell's pin).
fn net_cell(nl: &Netlist, n: NetId) -> CellId {
    nl.nets[n].driver.map(|p| p.cell).unwrap_or(CellId::new(0))
}

/// The input of the free clock buffer nearest `src` by delay, avoiding the
/// nodes of global routes.
fn nearest_dcc(
    arch: &Arch,
    src: u32,
    used: &FxHashSet<u32>,
    taken: &FxHashSet<u32>,
) -> Option<u32> {
    let g = &arch.graph;
    let mut dist: FxHashMap<u32, f32> = FxHashMap::default();
    let mut heap = BinaryHeap::new();
    dist.insert(src, 0.0);
    heap.push((std::cmp::Reverse(0u64), src));
    let mut visited = 0;
    while let Some((std::cmp::Reverse(d), v)) = heap.pop() {
        let d = d as f32 / 16.0;
        if dist.get(&v).is_some_and(|&b| b < d) {
            continue;
        }
        let name = g.name(v);
        if name.starts_with("G_CLKI_") && name.contains("DCC") && !taken.contains(&v) {
            return Some(v);
        }
        visited += 1;
        if visited > 2_000_000 {
            return None;
        }
        for e in g.edges(v) {
            let w = g.edge_dst[e];
            if used.contains(&w) {
                continue;
            }
            let nd = d + g.edge_delay[e] + 1.0;
            if dist.get(&w).is_none_or(|&b| nd < b) {
                dist.insert(w, nd);
                heap.push((std::cmp::Reverse((nd * 16.0) as u64), w));
            }
        }
    }
    None
}

/// The output of the clock buffer whose input is `input`.
fn dcc_output(arch: &Arch, input: u32) -> Result<u32, String> {
    let g = &arch.graph;
    let name = g.name(input).replacen("G_CLKI_", "G_CLKO_", 1);
    g.node(g.node_x[input as usize] as usize, g.node_y[input as usize] as usize, &name)
        .ok_or_else(|| format!("no clock buffer output {name}"))
}

/// Whether a node belongs to global `n`'s distribution or a tile's clock
/// multiplexers.
fn on_global(name: &str, n: usize) -> bool {
    let branch = format!("{n:02}00");
    let global = |p: &str| name.strip_prefix(p) == Some(&branch);
    if ["G_HPRX", "G_VPTX", "L_HPBX", "R_HPBX", "G_HPBX"].iter().any(|p| global(p)) {
        return true;
    }
    if let Some(q) = name.strip_prefix("G_")
        && q.len() > 6
        && &q[2..6] == "PCLK"
    {
        return q[6..] == n.to_string();
    }
    matches!(name, "CLK0" | "CLK1")
        || name.starts_with("MUXCLK")
        || (name.starts_with("CLK") && name.ends_with("_SLICE"))
}

/// Routes a clock from its buffer's output to `pins` over global `n`,
/// taking no node in `used` (and adding its own).
fn route_global(
    arch: &Arch,
    output: u32,
    n: usize,
    pins: impl Iterator<Item = u32>,
    used: &mut FxHashSet<u32>,
) -> Result<Vec<u32>, String> {
    let g = &arch.graph;
    let pins: FxHashSet<u32> = pins.collect();
    // Breadth first from the buffer: through the center multiplexers onto
    // the quadrant globals (any free global wires), then down global n.
    let mut parent: FxHashMap<u32, u32> = FxHashMap::default();
    let mut queue = VecDeque::from([output]);
    let mut seen: FxHashSet<u32> = FxHashSet::from_iter([output]);
    let mut found = 0;
    while let Some(v) = queue.pop_front() {
        if pins.contains(&v) {
            found += 1;
            if found == pins.len() {
                break;
            }
            continue;
        }
        let in_tree = on_global(g.name(v), n);
        for e in g.edges(v) {
            let w = g.edge_dst[e];
            if seen.contains(&w) || used.contains(&w) {
                continue;
            }
            let name = g.name(w);
            // Off global n's distribution only toward it: center wires.
            let ok = on_global(name, n)
                || (!in_tree && name.starts_with("G_") && !name.contains("HPBX"));
            if !ok {
                continue;
            }
            seen.insert(w);
            parent.insert(w, e as u32);
            queue.push_back(w);
        }
    }
    if found < pins.len() {
        return Err(format!(
            "{} of {} clock pins unreachable on global {n}",
            pins.len() - found,
            pins.len()
        ));
    }
    let mut edges = Vec::new();
    let mut in_route: FxHashSet<u32> = FxHashSet::default();
    for &p in &pins {
        let mut v = p;
        while v != output && in_route.insert(v) {
            let e = parent[&v];
            edges.push(e);
            v = edge_source(g, e);
        }
    }
    used.extend(in_route);
    edges.sort_unstable();
    Ok(edges)
}

/// The physical input each logical LUT input arrives on, from the routes'
/// permutation edges (physical `X<z>` into logical `Y<z>_SLICE`).
fn lut_pins(
    arch: &Arch,
    nl: &Netlist,
    placement: &Placement,
    routing: &Routing,
) -> FxHashMap<CellId, [u8; 4]> {
    let g = &arch.graph;
    let mut at: FxHashMap<(i16, i16, String), CellId> = FxHashMap::default();
    for (&c, s) in &placement.sites {
        if nl.cell_kind(c) == LUT4 {
            for p in LUT_PINS {
                at.insert((s.x as i16, s.y as i16, format!("{p}{}_SLICE", s.z)), c);
            }
        }
    }
    let mut out: FxHashMap<CellId, [u8; 4]> = FxHashMap::default();
    let mut src: FxHashMap<u32, u32> = FxHashMap::default();
    for edges in routing.nets.values() {
        for &e in edges {
            let d = g.edge_dst[e as usize];
            let name = g.name(d);
            if !name.ends_with("_SLICE") || g.edge_tile[e as usize] != NO_TILE {
                continue;
            }
            let key = (g.node_x[d as usize], g.node_y[d as usize], name.to_string());
            let Some(&cell) = at.get(&key) else { continue };
            let s = *src.entry(e).or_insert_with(|| edge_source(g, e));
            let logical = LUT_PINS.iter().position(|p| name.starts_with(p)).expect("LUT pin");
            let physical =
                LUT_PINS.iter().position(|p| g.name(s).starts_with(p)).expect("LUT input");
            out.entry(cell).or_insert([0, 1, 2, 3])[logical] = physical as u8;
        }
    }
    out
}

/// The node an edge leaves (by search of its index in the offsets).
pub fn edge_source(g: &crate::graph::Graph, e: u32) -> u32 {
    let i = g.offsets.partition_point(|&o| o <= e);
    (i - 1) as u32
}
