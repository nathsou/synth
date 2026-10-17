//! Routing of a placed iCE40 design with the generic PathFinder router.

use synth_core::FxHashMap;
use synth_netlist::{NetId, Netlist};
use synth_route::{Graph, NetRequest, RouteOptions, RouteStats};

use crate::arch::{Arch, NodeClass};
use crate::chipdb::NodeId;
use crate::place::Placement;

/// The routing of every net: edges of the architecture's routing graph.
#[derive(Clone, Debug, Default)]
pub struct Routing {
    pub net_edges: FxHashMap<NetId, Vec<u32>>,
    /// Source node of each routed net.
    pub net_source: FxHashMap<NetId, NodeId>,
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
    let mut requests = Vec::new();
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
        for &u in &net.users {
            let node = pin_node(arch, nl, placement, u.cell, pin_name(u))?;
            if node != source && !sinks.contains(&node.raw()) {
                sinks.push(node.raw());
            }
        }
        sources.insert(n, source);
        if sinks.is_empty() {
            continue;
        }
        requests.push(NetRequest { source: source.raw(), sinks, criticality: vec![] });
        ids.push(n);
    }
    let opts = RouteOptions { astar_cost_per_tile: 0.15, ..RouteOptions::default() };
    let (routes, stats): (_, RouteStats) =
        synth_route::route(graph, &requests, &opts).map_err(|e| match e {
            synth_route::RouteError::Unroutable { net, sink } => format!(
                "net {} cannot reach {}",
                nl.str(nl.nets[ids[net]].name),
                arch.db.node_name(NodeId::from_raw(sink))
            ),
            synth_route::RouteError::Congested { overused_nodes, nodes, nets } => {
                let names: Vec<&str> =
                    nets.iter().take(8).map(|&i| nl.str(nl.nets[ids[i]].name)).collect();
                let wires: Vec<String> =
                    nodes.iter().take(4).map(|&k| arch.db.node_name(NodeId::from_raw(k))).collect();
                // Sinks of the conflicting nets in the tile of the first wire.
                let tile = wires.first().map(|w| w.rsplit_once('/').map_or("", |x| x.0).to_string());
                let mut detail = String::new();
                if let Some(tile) = tile {
                    for &i in nets.iter().take(4) {
                        let sinks: Vec<String> = requests[i]
                            .sinks
                            .iter()
                            .map(|&k| arch.db.node_name(NodeId::from_raw(k)))
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
    Ok(routing)
}
