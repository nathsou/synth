//! Routing of a placed iCE40 design with the generic PathFinder router.

use synth_core::FxHashMap;
use synth_netlist::{CellId, NetId, Netlist, Param};
use synth_route::{Graph, NetRequest, RouteOptions, RouteStats};

use crate::arch::{Arch, NodeClass};
use crate::chipdb::NodeId;
use crate::pack::LC;
use crate::place::Placement;

/// The routing of every net: edges of the architecture's routing graph.
#[derive(Clone, Debug, Default)]
pub struct Routing {
    pub net_edges: FxHashMap<NetId, Vec<u32>>,
    /// Source node of each routed net.
    pub net_source: FxHashMap<NetId, NodeId>,
    /// Logic cells whose LUT inputs arrive on other physical pins: for each
    /// logical input `I<k>`, the physical pin carrying it.
    pub lut_pins: FxHashMap<CellId, [u8; 4]>,
}

impl Routing {
    /// Rewrites the truth tables of logic cells for the physical pins their
    /// inputs were routed to.
    pub fn apply_lut_permutation(&self, nl: &mut Netlist) {
        for (&c, perm) in &self.lut_pins {
            let init = nl.param(c, "LUT_INIT").and_then(Param::as_u64).unwrap_or(0);
            let connected: Vec<bool> =
                ["I0", "I1", "I2", "I3"].iter().map(|p| nl.pin_net(c, p).is_some()).collect();
            let mut new = 0u64;
            for phys in 0..16usize {
                let logical: usize =
                    (0..4).filter(|&k| connected[k]).map(|k| ((phys >> perm[k]) & 1) << k).sum();
                new |= ((init >> logical) & 1) << phys;
            }
            nl.set_param(c, "LUT_INIT", Param::Int(new as i64));
        }
    }
}

/// Whether input `I<k>` of a logic cell may arrive on any free LUT input:
/// not the carry's operands (`I1`, `I2` with the carry enabled) nor `I3` fed
/// by the carry chain.
fn permutable(nl: &Netlist, c: CellId, k: usize) -> bool {
    let carry = nl.param(c, "CARRY_ENABLE").and_then(Param::as_u64).unwrap_or(0) != 0;
    match k {
        1 | 2 => !carry,
        3 => nl.pin_net(c, "CIN").is_none() || nl.pin_net(c, "I3") != nl.pin_net(c, "CIN"),
        _ => true,
    }
}

/// The routing node a net reaches a cell's input pin at: the logical LUT
/// input for permutable LUT inputs, otherwise the pin itself.
pub fn sink_node(
    arch: &Arch,
    nl: &Netlist,
    placement: &Placement,
    cell: CellId,
    pin: &str,
) -> Result<NodeId, String> {
    if nl.cell_kind(cell) == LC
        && let Some(k) = ["I0", "I1", "I2", "I3"].iter().position(|p| *p == pin)
        && permutable(nl, cell, k)
    {
        return pin_node(arch, nl, placement, cell, ["PERM0", "PERM1", "PERM2", "PERM3"][k]);
    }
    pin_node(arch, nl, placement, cell, pin)
}

fn node_cost(class: NodeClass) -> f32 {
    match class {
        NodeClass::Global => 0.5,
        NodeClass::Sp12H | NodeClass::Sp12V | NodeClass::IoSpan12 => 1.4,
        _ => 1.0,
    }
}

/// The routing node of a placed cell's pin.
pub fn pin_node(
    arch: &Arch,
    nl: &Netlist,
    placement: &Placement,
    cell: synth_netlist::CellId,
    pin: &str,
) -> Result<NodeId, String> {
    let bel = placement
        .cell_bel
        .get(&cell)
        .ok_or_else(|| format!("cell {} is not placed", nl.str(nl.cells[cell].name)))?;
    arch.bel(*bel)
        .pin(pin)
        .ok_or_else(|| format!("bel of {} has no pin {pin}", nl.str(nl.cells[cell].name)))
}

/// Routes all nets of a placed design.
pub fn route(
    arch: &Arch,
    nl: &Netlist,
    placement: &Placement,
    clocks: &crate::timing::ClockConstraints,
    log: &mut Vec<String>,
) -> Result<Routing, String> {
    let g = &arch.graph;
    let costs: Vec<f32> = g.node_class.iter().map(|&c| node_cost(c)).collect();
    let graph = Graph {
        offsets: &g.offsets,
        edge_dst: &g.edge_dst,
        edge_delay: &g.edge_delay,
        node_x: &g.node_x,
        node_y: &g.node_y,
        node_cost: &costs,
    };
    // Connection criticalities from estimated delays, so that critical
    // connections take fast routes.
    let timing = crate::timing::TimingGraph::new(arch, nl, clocks);
    let est: Vec<f64> = timing
        .conns
        .iter()
        .map(|c| {
            let dname = nl.str(nl.cells[c.driver].pins[c.driver_pin].name);
            let sname = nl.str(nl.cells[c.sink].pins[c.pin].name);
            if dname == "COUT" && sname == "CIN" {
                return 0.0;
            }
            match (
                pin_node(arch, nl, placement, c.driver, dname),
                pin_node(arch, nl, placement, c.sink, sname),
            ) {
                (Ok(a), Ok(b)) => {
                    let (a, b) = (a.raw() as usize, b.raw() as usize);
                    let dist = (g.node_x[a] as i32 - g.node_x[b] as i32).abs()
                        + (g.node_y[a] as i32 - g.node_y[b] as i32).abs();
                    crate::place::est_delay(dist)
                }
                _ => 0.0,
            }
        })
        .collect();
    let crit = timing.analyse(&est).criticality;
    // The carry reads the physical pins in1 and in2: no other signal may
    // arrive there through the LUT input permutation.
    let mut blocked = Vec::new();
    for c in nl.live_cells() {
        if nl.cell_kind(c) == LC && !permutable(nl, c, 1) {
            for p in ["I1", "I2"] {
                if nl.pin_net(c, p).is_none() {
                    blocked.push(pin_node(arch, nl, placement, c, p)?.raw());
                }
            }
        }
    }
    let mut requests = Vec::new();
    // Logic cell of each logical LUT input node that is a sink.
    let mut logical_sink: FxHashMap<u32, CellId> = FxHashMap::default();
    let mut ids = Vec::new();
    let mut sources = FxHashMap::default();
    for n in nl.live_nets() {
        let net = &nl.nets[n];
        let Some(drv) = net.driver else {
            if !net.users.is_empty() {
                log.push(format!("warning: net {} has no driver", nl.str(net.name)));
            }
            continue;
        };
        let pin_name =
            |p: synth_netlist::PinRef| nl.str(nl.cells[p.cell].pins[p.pin as usize].name);
        let source = pin_node(arch, nl, placement, drv.cell, pin_name(drv))?;
        let mut sinks: Vec<u32> = Vec::new();
        let mut criticality: Vec<f32> = Vec::new();
        for &u in &net.users {
            let node = sink_node(arch, nl, placement, u.cell, pin_name(u))?;
            if g.node_class[node.index()] == NodeClass::LcInput
                && node.index() >= arch.db.nodes.len()
            {
                logical_sink.insert(node.raw(), u.cell);
            }
            let c = timing.conn_to(u.cell, u.pin as usize).map_or(0.0, |k| crit[k]) as f32;
            match sinks.iter().position(|&s| s == node.raw()) {
                Some(k) => criticality[k] = criticality[k].max(c),
                None if node != source => {
                    sinks.push(node.raw());
                    criticality.push(c);
                }
                None => {}
            }
        }
        sources.insert(n, source);
        if sinks.is_empty() {
            continue;
        }
        requests.push(NetRequest { source: source.raw(), sinks, criticality });
        ids.push(n);
    }
    let opts = RouteOptions { astar_cost_per_tile: 0.15, ..RouteOptions::default() };
    let t0 = std::time::Instant::now();
    let trace = std::env::var("SYNTH_DEBUG").is_ok();
    let mut progress = |iter: usize, nets: usize, over: &[usize]| {
        if !trace {
            return;
        }
        let mut by_class: FxHashMap<String, usize> = FxHashMap::default();
        for &k in over {
            *by_class.entry(format!("{:?}", g.node_class[k])).or_default() += 1;
        }
        let mut v: Vec<_> = by_class.into_iter().collect();
        v.sort_by_key(|e| std::cmp::Reverse(e.1));
        eprintln!(
            "debug: route iteration {iter}: {nets} nets, {} overused {v:?} ({:.1}s)",
            over.len(),
            t0.elapsed().as_secs_f64()
        );
    };
    let hooks =
        synth_route::RouteHooks { blocked: &blocked, progress: Some(&mut progress), retime: None };
    let (routes, stats): (_, RouteStats) =
        synth_route::route_with(graph, &requests, &opts, hooks).map_err(|e| match e {
            synth_route::RouteError::Unroutable { net, sink } => format!(
                "net {} cannot reach {}",
                nl.str(nl.nets[ids[net]].name),
                arch.node_name(NodeId::from_raw(sink))
            ),
            synth_route::RouteError::Congested { overused_nodes, nodes, nets } => {
                let names: Vec<&str> =
                    nets.iter().take(8).map(|&i| nl.str(nl.nets[ids[i]].name)).collect();
                let wires: Vec<String> =
                    nodes.iter().take(4).map(|&k| arch.node_name(NodeId::from_raw(k))).collect();
                // Sinks of the conflicting nets in the tile of the first wire.
                let tile = wires.first().map(|w| w.rsplit_once('/').map_or("", |x| x.0).to_string());
                let mut detail = String::new();
                if let Some(tile) = tile {
                    for &i in nets.iter().take(4) {
                        let sinks: Vec<String> = requests[i]
                            .sinks
                            .iter()
                            .map(|&k| arch.node_name(NodeId::from_raw(k)))
                            .filter(|n| n.starts_with(&format!("{tile}/")))
                            .collect();
                        detail.push_str(&format!("; {} -> {}", nl.str(nl.nets[ids[i]].name), sinks.join(" ")));
                    }
                }
                format!(
                    "routing congestion not resolved: {overused_nodes} wires overused (e.g. {}) by nets {}{detail}",
                    wires.join(", "),
                    names.join(", ")
                )
            }
        })?;
    log.push(format!(
        "routing: {} nets, {} wires used, {} iterations",
        requests.len(),
        stats.wire_nodes,
        stats.iterations
    ));
    let mut routing = Routing { net_source: sources, ..Default::default() };
    for (i, r) in routes.into_iter().enumerate() {
        routing.net_edges.insert(ids[i], r.edges);
    }
    // The physical pin each logical LUT input was routed through.
    for edges in routing.net_edges.values() {
        for &e in edges {
            let Some((phys, logical)) = Arch::permutation(g.edge_pip[e as usize]) else { continue };
            if let Some(&cell) = logical_sink.get(&g.edge_dst[e as usize]) {
                routing.lut_pins.entry(cell).or_insert([0, 1, 2, 3])[logical] = phys as u8;
            }
        }
    }
    routing.lut_pins.retain(|_, p| *p != [0, 1, 2, 3]);
    Ok(routing)
}
