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

/// Whether physical LUT input `in_<j>` of the logic cell in slot `z` is fed
/// by class A local tracks (even `local_g0`/`local_g2`, odd
/// `local_g1`/`local_g3`), which also feed the flip-flop controls; the other
/// inputs are fed by the 16 class B tracks.
fn class_a(z: u8, j: usize) -> bool {
    j % 2 == z as usize % 2
}

/// Local track classes for nets with several inputs in a logic tile.
///
/// The LUT input permutation lets a net reach a cell on inputs of either
/// class. A net feeding several cells of a tile takes the same class in all
/// of them (one track), and in a full tile that can leave the other inputs
/// of those cells more nets than tracks of the other class. No single net
/// can then resolve the conflict, as moving the shared net to the other
/// class in one cell costs a track and frees none. So in tiles with more
/// than 16 input nets, each net with several inputs (or a fixed input or
/// control pin besides) gets the class that leaves the most room for the
/// others, which the router places freely; failing that, every input keeps
/// the class placement counted it in. Returns the permutation edges this
/// rules out.
fn track_classes(arch: &Arch, nl: &Netlist, placement: &Placement) -> Result<Vec<u32>, String> {
    const IN: [&str; 4] = ["I0", "I1", "I2", "I3"];
    const PERM: [&str; 4] = ["PERM0", "PERM1", "PERM2", "PERM3"];
    let g = &arch.graph;
    let mut tiles: FxHashMap<(u8, u8), Vec<(u8, CellId)>> = FxHashMap::default();
    for (&c, &bel) in &placement.cell_bel {
        if nl.cell_kind(c) == LC {
            let b = arch.bel(bel);
            tiles.entry((b.x, b.y)).or_default().push((b.z, c));
        }
    }
    let mut keys: Vec<(u8, u8)> = tiles.keys().copied().collect();
    keys.sort_unstable();
    let mut excluded = Vec::new();
    for key in keys {
        let cells = &tiles[&key];
        // The tile's nets, those with a fixed pin of each class (bit
        // masks), the inputs free to take either class, and the inputs of
        // each class each cell has room for.
        let mut nets: Vec<NetId> = Vec::new();
        let mut index = |n: NetId| match nets.iter().position(|&m| m == n) {
            Some(i) => i,
            None => {
                nets.push(n);
                nets.len() - 1
            }
        };
        let (mut fixed_a, mut fixed_b) = (0u64, 0u64);
        let mut free: Vec<(usize, usize, usize)> = Vec::new();
        let mut room = vec![[2u8; 2]; cells.len()];
        for (ci, &(z, c)) in cells.iter().enumerate() {
            if nl.param(c, "DFF_ENABLE").and_then(Param::as_u64).unwrap_or(0) != 0 {
                for p in ["CLK", "CEN", "SR"] {
                    if let Some(n) = nl.pin_net(c, p).filter(|&n| !crate::place::on_global(nl, n)) {
                        fixed_a |= 1 << index(n);
                    }
                }
            }
            for (k, p) in IN.iter().enumerate() {
                let net = nl.pin_net(c, p);
                // A carry output reaches the next cell on `in_3` only.
                let carried = k == 3
                    && net.and_then(|n| nl.nets[n].driver).is_some_and(|d| {
                        nl.str(nl.cells[d.cell].pins[d.pin as usize].name) == "COUT"
                    });
                if permutable(nl, c, k) && !carried {
                    if let Some(n) = net {
                        free.push((ci, k, index(n)));
                    }
                    continue;
                }
                // Carry operands (or a blocked pin), or a carry on `in_3`,
                // which needs no track.
                let a = class_a(z, k);
                room[ci][usize::from(!a)] -= 1;
                if let Some(n) = net.filter(|_| k != 3) {
                    *(if a { &mut fixed_a } else { &mut fixed_b }) |= 1 << index(n);
                }
            }
        }
        if nets.len() <= 16 {
            continue;
        }
        let mut sinks = vec![0usize; nets.len()];
        for &(_, _, n) in &free {
            sinks[n] += 1;
        }
        let fixed = fixed_a | fixed_b;
        let shared: Vec<usize> = (0..nets.len())
            .filter(|&n| sinks[n] >= 2 || (sinks[n] == 1 && fixed >> n & 1 != 0))
            .collect();
        let slot: Vec<Option<usize>> =
            free.iter().map(|&(_, _, n)| shared.iter().position(|&s| s == n)).collect();
        let mut unique = vec![0i32; cells.len()];
        for (&(ci, _, _), s) in free.iter().zip(&slot) {
            unique[ci] += i32::from(s.is_none());
        }
        // Tracks of the busier class and in all with the shared nets in the
        // classes of `mask` (bit set: class A), the others spread at best.
        let load = |mask: u32| -> Option<(i32, i32)> {
            let (mut a, mut b) = (fixed_a, fixed_b);
            for (i, &n) in shared.iter().enumerate() {
                *(if mask >> i & 1 != 0 { &mut a } else { &mut b }) |= 1 << n;
            }
            let mut left: Vec<[i32; 2]> =
                room.iter().map(|r| [i32::from(r[0]), i32::from(r[1])]).collect();
            for (&(ci, _, _), s) in free.iter().zip(&slot) {
                if let Some(i) = *s {
                    left[ci][usize::from(mask >> i & 1 == 0)] -= 1;
                }
            }
            // Range of the number of other nets on class A tracks.
            let (mut lo, mut hi, mut total) = (0, 0, 0);
            for (&[ra, rb], &u) in left.iter().zip(&unique) {
                if ra < 0 || rb < 0 || u > ra + rb {
                    return None;
                }
                lo += (u - rb).max(0);
                hi += u.min(ra);
                total += u;
            }
            let (na, nb) = (a.count_ones() as i32, b.count_ones() as i32);
            let even = nb + total - na;
            let busier = [even.div_euclid(2), (even + 1).div_euclid(2)]
                .map(|t| (na + t.clamp(lo, hi)).max(nb + total - t.clamp(lo, hi)));
            Some((busier[0].min(busier[1]), na + nb + total))
        };
        let combos = if shared.len() <= 12 { 1u32 << shared.len() } else { 0 };
        let best = (0..combos).filter_map(|m| Some((load(m)?, m))).min_by_key(|&(l, _)| l);
        let fits = best.filter(|&((busier, _), _)| busier <= 16).map(|(_, m)| m);
        // Room left by the shared nets in each cell: the other nets must
        // take the class with room when the other has none.
        let mut left = room.clone();
        for (&(ci, _, _), s) in free.iter().zip(&slot) {
            if let (Some(mask), Some(i)) = (fits, *s) {
                left[ci][usize::from(mask >> i & 1 == 0)] -= 1;
            }
        }
        for (&(ci, k, _), s) in free.iter().zip(&slot) {
            let (z, c) = cells[ci];
            let class = match (fits, s) {
                (Some(mask), Some(i)) => mask >> i & 1 != 0,
                (Some(_), None) if left[ci][1] == 0 => true,
                (Some(_), None) if left[ci][0] == 0 => false,
                (Some(_), None) => continue,
                (None, _) => class_a(z, k),
            };
            let bel = arch.bel(placement.cell_bel[&c]);
            let virt = bel.pin(PERM[k]).ok_or("logic cell without permutation nodes")?;
            for j in (0..4).filter(|&j| class_a(z, j) != class) {
                let phys = bel.pin(IN[j]).ok_or("logic cell without inputs")?;
                excluded.extend(
                    g.edges(phys.raw()).filter(|&e| g.edge_dst[e] == virt.raw()).map(|e| e as u32),
                );
            }
        }
    }
    Ok(excluded)
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
    timing_rounds: usize,
    log: &mut Vec<String>,
) -> Result<Routing, String> {
    route_incremental(arch, nl, placement, clocks, timing_rounds, log, None)
}

pub(crate) fn route_incremental(
    arch: &Arch,
    nl: &Netlist,
    placement: &Placement,
    clocks: &crate::timing::ClockConstraints,
    timing_rounds: usize,
    log: &mut Vec<String>,
    checkpoint: Option<&crate::checkpoint::Checkpoint>,
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
    // The request and sink node of each timing connection.
    let mut conn_target: Vec<Option<(usize, u32)>> = vec![None; timing.conns.len()];
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
            let conn = timing.conn_to(u.cell, u.pin as usize);
            if let Some(k) = conn {
                conn_target[k] = Some((requests.len(), node.raw()));
            }
            let c = conn.map_or(0.0, |k| crit[k]) as f32;
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
    let mut opts =
        RouteOptions { astar_cost_per_tile: 0.15, timing_rounds, ..RouteOptions::default() };
    // A repair: new routes avoid the kept ones from the start rather than
    // negotiate with them, and a repair that stops making progress gives
    // way to fresh place and route sooner.
    if checkpoint.is_some() {
        opts.present_factor = 5.0;
        opts.stall_iterations = 30;
    }
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
    // Criticalities from routed delays (as the final analysis computes them;
    // estimates for what is not routed yet), scored by the worst excess
    // over the clock targets.
    let mut retime = |routes: &[synth_route::NetRoute]| -> synth_route::Retimed {
        let at: Vec<FxHashMap<u32, f64>> = routes
            .iter()
            .zip(&requests)
            .map(|(r, q)| crate::timing::routed_node_delays(arch, Some(q.source), &r.edges))
            .collect();
        let delays: Vec<f64> = (0..timing.conns.len())
            .map(|k| match conn_target[k] {
                Some((i, node)) => at[i].get(&node).copied().unwrap_or(est[k]),
                None => est[k],
            })
            .collect();
        // With the LUT input permutation the routes chose.
        let mut lut_pins: FxHashMap<CellId, [u8; 4]> = FxHashMap::default();
        for &e in routes.iter().flat_map(|r| &r.edges) {
            if let Some((phys, logical)) = Arch::permutation(g.edge_pip[e as usize])
                && let Some(&cell) = logical_sink.get(&g.edge_dst[e as usize])
            {
                lut_pins.entry(cell).or_insert([0, 1, 2, 3])[logical] = phys as u8;
            }
        }
        let a =
            crate::timing::TimingGraph::with_lut_pins(arch, nl, clocks, &lut_pins).analyse(&delays);
        let mut criticality: Vec<Vec<f32>> =
            requests.iter().map(|r| vec![0.0; r.sinks.len()]).collect();
        for (k, t) in conn_target.iter().enumerate() {
            if let Some((i, node)) = *t
                && let Some(j) = requests[i].sinks.iter().position(|&s| s == node)
            {
                criticality[i][j] = criticality[i][j].max(a.criticality[k] as f32);
            }
        }
        let score = a
            .period
            .iter()
            .zip(&timing.clocks)
            .map(|(p, c)| p - c.target_ps.unwrap_or(0.0))
            .fold(f64::NEG_INFINITY, f64::max);
        if trace {
            eprintln!(
                "debug: routed timing score {:.0} ps ({:.1}s)",
                score,
                t0.elapsed().as_secs_f64()
            );
        }
        synth_route::Retimed { criticality, score }
    };
    let blocked_edges = track_classes(arch, nl, placement)?;
    let hooks = synth_route::RouteHooks {
        blocked: &blocked,
        blocked_edges: &blocked_edges,
        progress: Some(&mut progress),
        retime: if timing_rounds > 0 { Some(&mut retime) } else { None },
    };
    let previous = checkpoint.map(|c| c.routes()).unwrap_or_default();
    let initial: Vec<_> = requests
        .iter()
        .filter(|_| checkpoint.is_some())
        .map(|r| synth_route::NetRoute {
            edges: previous.get(&r.source).cloned().unwrap_or_default(),
        })
        .collect();
    let (routes, stats): (_, RouteStats) =
        synth_route::route_with_initial(graph, &requests, &opts, hooks, &initial).map_err(|e| match e {
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
    if checkpoint.is_some() {
        log.push(format!(
            "incremental routing: reused {}/{} net trees before repair, {} in part",
            stats.reused_nets,
            requests.len(),
            stats.partly_reused_nets
        ));
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::Part;

    #[test]
    fn local_track_classes() {
        let arch = Arch::new(Part::Hx8k, "cb132").unwrap();
        let g = &arch.graph;
        let node = |name: &str| arch.db.node_by_name(6, 10, name).unwrap().raw();
        let feeds = |track: u32, pin: u32| g.edges(track).any(|e| g.edge_dst[e] == pin);
        // Track `local_g<G>_<N>` is of class A when G and N have the same
        // parity; each LUT input sees (almost) all 16 tracks of its class.
        for z in 0..8u8 {
            for j in 0..4 {
                let pin = node(&format!("lutff_{z}/in_{j}"));
                let mut seen = 0;
                for t in 0..32 {
                    if feeds(node(&format!("local_g{}_{}", t / 8, t % 8)), pin) {
                        assert_eq!((t / 8) % 2 == t % 2, class_a(z, j), "lutff_{z}/in_{j}");
                        seen += 1;
                    }
                }
                assert!(seen >= 15, "lutff_{z}/in_{j}: {seen} tracks");
            }
        }
        for ctrl in ["lutff_global/clk", "lutff_global/cen", "lutff_global/s_r"] {
            for t in 0..32 {
                if feeds(node(&format!("local_g{}_{}", t / 8, t % 8)), node(ctrl)) {
                    assert_eq!((t / 8) % 2, t % 2, "{ctrl}");
                }
            }
        }
    }
}
