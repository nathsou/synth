//! Native Artix-7 physical implementation.
use crate::{
    device::Device,
    pack::{self, Packed},
    place::{self, Placement},
    routing::RoutingGraph,
    xdc::Constraints,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Instant,
};
use synth_core::FxHashMap;
use synth_netlist::{CellId, Dir, NetId, Netlist};

#[derive(Clone, Debug)]
pub struct FlowOptions {
    pub part: String,
    pub seed: u64,
    pub effort: f64,
}
#[derive(Debug)]
pub struct FlowResult {
    pub features: BTreeSet<String>,
    pub bitstream: Vec<u8>,
    pub log: Vec<String>,
    pub placement: Placement,
    /// Estimated worst register-to-register periods.
    pub timing: Vec<crate::sta::Clock>,
}

pub fn implement(
    mut nl: Netlist,
    root: &Path,
    xdc: &Constraints,
    opts: &FlowOptions,
) -> Result<FlowResult, String> {
    let start = Instant::now();
    let mut log = Vec::new();
    let mut p = pack::pack(&mut nl)?;
    log.push(format!("packed {} slices, {} carry chains", p.slices.len(), p.chains.len()));
    let db = Device::load(root, &opts.part)?;
    log.push(format!("loaded {} nodes in {:.2}s", db.node_x.len(), start.elapsed().as_secs_f64()));
    let timer = crate::sta::Timer::new(root, &nl, &p)?;
    // Timing-driven placement: criticalities from the delays the placement
    // being annealed predicts.
    let mut criticality = |pl: &place::Placement| {
        let a = timer.analyze(&predictor(&db, &nl, &p, pl));
        a.slack.keys().map(|&(c, n)| ((c, n), a.criticality(c, n))).collect()
    };
    let graph = RoutingGraph::new(&db)?;
    let clocked = graph.clocked(&db)?;
    let placement =
        place::place(&db, &nl, &p, xdc, &clocked, opts.seed, opts.effort, Some(&mut criticality))?;
    log.push(format!("placed: HPWL {:.0} ({:.1}s)", placement.hpwl, start.elapsed().as_secs_f64()));
    // Detailed placement: lanes move between the placed slices, with
    // criticalities from the delays the new slice contents predict.
    let mut retime = |q: &Packed| {
        let a = timer.analyze(&predictor(&db, &nl, q, &placement));
        a.slack.keys().map(|&(c, n)| ((c, n), a.criticality(c, n))).collect()
    };
    let moves = crate::detail::refine(&db, &nl, &mut p, &placement, opts.seed, &mut retime)?;
    log.push(format!(
        "detailed placement: {moves} lane moves ({:.1}s)",
        start.elapsed().as_secs_f64()
    ));
    if let Ok(path) = std::env::var("SYNTH_XC7_PLACEMENT") {
        std::fs::write(&path, placement_xdc(&nl, &p, &placement)).map_err(|e| e.to_string())?;
    }
    if let Ok(path) = std::env::var("SYNTH_XC7_MACROS") {
        std::fs::write(&path, packing_macros(&nl, &p)).map_err(|e| e.to_string())?;
    }
    let (mut requests, names, request_nets, owners, blocked_edges) =
        requests(&db, &nl, &p, &placement, &graph)?;
    // Timing-driven routing: criticalities from delays predicted by the
    // placement, then from the routes as they converge.
    let predicted = predictor(&db, &nl, &p, &placement);
    let estimate = timer.analyze(&predicted);
    let criticality = |a: &crate::sta::Analysis, i: usize, sink: u32| -> f32 {
        let Some(net) = request_nets[i] else { return 0.0 };
        owners
            .get(&sink)
            .into_iter()
            .flatten()
            .filter(|&&(_, _, n)| n == net)
            .map(|&(c, _, n)| a.criticality(c, n))
            .fold(0.0, f32::max)
    };
    for (i, r) in requests.iter_mut().enumerate() {
        r.criticality = r.sinks.iter().map(|&k| criticality(&estimate, i, k)).collect();
    }
    for clock in &estimate.clocks {
        log.push(format!(
            "placed timing estimate: clock {}: {:.3} ns",
            nl.str(nl.nets[clock.net].name),
            clock.period / 1000.0
        ));
    }
    log.push(format!(
        "routing {} physical nets on {} edges ({:.1}s)",
        requests.len(),
        graph.dst.len(),
        start.elapsed().as_secs_f64()
    ));
    for line in &log {
        eprintln!("info: {line}");
    }
    let route_opts = synth_route::RouteOptions {
        // Wires span up to 18 tiles: a lookahead by wire class bounds the
        // remaining cost far better than a cost per tile.
        lookahead_radius: 30,
        bbox_margin: 5,
        arc_bounding_boxes: true,
        max_iterations: 100,
        stall_iterations: 30,
        // Criticalities follow the routes every other iteration.
        retime_every: 2,
        // Critical connections weigh delay almost alone against congestion:
        // the 0.9 default left them on slower detours (about 8% Fmax on the
        // Euler designs, by the calibrated estimate), and nets still converge.
        max_criticality: 0.99,
        // Once routing is legal, critical nets are routed again three times,
        // keeping the best timed routing: 1-6% Fmax for up to 1.6 times the
        // routing time (more rounds gained no more).
        timing_rounds: 3,
        ..Default::default()
    };
    let mut progress = |iteration: usize, routed: usize, overused: &[usize]| {
        eprintln!(
            "info: route iteration {iteration}: {routed} nets, {} congested nodes ({:.1}s)",
            overused.len(),
            start.elapsed().as_secs_f64()
        );
    };
    let mut retime = |routes: &[synth_route::NetRoute]| -> synth_route::Retimed {
        let wires =
            crate::sta::routed_wires(&db, &graph, &requests, &request_nets, routes, &owners);
        // Connections not routed yet keep their predicted delays.
        let a =
            timer.analyze(&|c, n| wires.get(&(c, n)).copied().unwrap_or_else(|| predicted(c, n)));
        let criticality = requests
            .iter()
            .enumerate()
            .map(|(i, r)| r.sinks.iter().map(|&k| criticality(&a, i, k)).collect())
            .collect();
        let score = a.clocks.iter().map(|c| c.period).fold(0.0, f64::max);
        eprintln!(
            "info: routed timing estimate: {:.3} ns ({:.1}s)",
            score / 1000.0,
            start.elapsed().as_secs_f64()
        );
        synth_route::Retimed { criticality, score }
    };
    let hooks = synth_route::RouteHooks {
        progress: Some(&mut progress),
        retime: Some(&mut retime),
        node_class: &graph.class,
        blocked_edges: &blocked_edges,
        ..Default::default()
    };
    let (routes, stats) = synth_route::route_with(graph.graph(), &requests, &route_opts, hooks)
        .map_err(|e| match &e {
            synth_route::RouteError::Congested { nodes, nets, .. } => {
                let nets: Vec<_> = nets.iter().map(|&i| names[i].as_str()).collect();
                let at: Vec<_> = nodes
                    .iter()
                    .map(|n| match owners.get(n) {
                        Some(pins) => format!("node {n}: {}", describe(&nl, pins)),
                        None => format!("node {n}: {}", node_name(&db, *n)),
                    })
                    .collect();
                format!("{e}; nets {}; {}", nets.join(", "), at.join("; "))
            }
            synth_route::RouteError::Unroutable { net, sink } => {
                let pins = owners.get(sink).map(|p| describe(&nl, p)).unwrap_or_default();
                format!("{e} ({}; {pins})", names[*net])
            }
        })?;
    crate::routing::verify(graph.graph(), &requests, &routes)?;
    let pins = lut_pins(&graph, &nl, &p, &placement, &routes);
    let mut features = cell_features(&db, &nl, &p, &placement, xdc, &pins)?;
    features.extend(graph.features(&db, &routes));
    let coverage = crate::timing::coverage(root, &db, &graph, &routes)?;
    log.push(coverage.report());
    for pip in &coverage.missing_pip_types {
        log.push(format!("uncharacterized routed pip: {pip}"));
    }
    // Verify frame consistency after all cell and routing features are combined.
    let frames = crate::fasm::Assembler::new(&db, root).assemble(&features)?;
    let bitstream = crate::bitstream::encode(&db.part, &frames, true)?;
    crate::bitstream::decode(&db.part, &bitstream)?;
    log.push(format!(
        "routed in {} iterations; {} features; total {:.2}s",
        stats.iterations,
        features.len(),
        start.elapsed().as_secs_f64()
    ));
    let wires = crate::sta::routed_wires(&db, &graph, &requests, &request_nets, &routes, &owners);
    let routed = |c, n| wires.get(&(c, n)).copied().unwrap_or(0.0);
    let timing = timer.analyze(&routed).clocks;
    if !coverage.missing_pip_types.is_empty() {
        log.push(format!(
            "timing estimate counts {} uncharacterized pip types as zero delay",
            coverage.missing_pip_types.len()
        ));
    }
    for clock in &timing {
        let name = nl.str(nl.nets[clock.net].name);
        let target = xdc
            .clocks
            .iter()
            .find(|(port, _)| {
                nl.live_cells().any(|c| {
                    nl.str(nl.cells[c].name) == port.as_str()
                        && nl.pin_net(c, "PAD") == Some(clock.net)
                })
            })
            .map(|(_, &ns)| {
                format!(
                    ", target {ns:.3} ns: {}",
                    if clock.period <= ns * 1000.0 { "met" } else { "MISSED" }
                )
            })
            .unwrap_or_default();
        log.push(format!(
            "timing estimate: clock {name}: {:.3} ns ({:.1} MHz){target}",
            clock.period / 1000.0,
            1e6 / clock.period.max(1.0)
        ));
        if let (Some(first), Some(last)) = (clock.path.first(), clock.path.last()) {
            log.push(format!("critical path: {first} -> {last} ({} pins)", clock.path.len()));
        }
    }
    // For comparison with nextpnr-xilinx, which does not time them.
    if nl.live_cells().any(|c| matches!(nl.cell_kind(c), "RAMB18E1" | "DSP48E1")) {
        for clock in timer.fabric().analyze(&routed).clocks {
            log.push(format!(
                "timing estimate without block RAM and DSP paths: clock {}: {:.3} ns ({:.1} MHz)",
                nl.str(nl.nets[clock.net].name),
                clock.period / 1000.0,
                1e6 / clock.period.max(1.0)
            ));
        }
    }
    log.push(
        "timing is not certified: the estimate uses nextpnr-xilinx's routing model of X-Ray data, \
         uncalibrated against vendor timing or hardware, without clock skew or I/O paths"
            .into(),
    );
    drop(predicted);
    Ok(FlowResult { features, bitstream, log, placement, timing })
}
/// The local GND_WIRE and VCC_WIRE nodes of the constant sinks that are
/// interconnect inputs.
fn local_constants(
    db: &Device,
    sinks: &BTreeMap<NetId, BTreeSet<u32>>,
    p: &Packed,
) -> FxHashMap<u32, (u32, u32)> {
    let wanted: BTreeSet<u32> = sinks
        .iter()
        .filter(|(n, _)| p.constants.contains_key(n))
        .flat_map(|(_, s)| s.iter().copied())
        .collect();
    // Interconnect mux outputs (site inputs) and their tiles' constants.
    let mut inputs = FxHashMap::default();
    for (ti, t) in db.tiles.iter().enumerate() {
        if !(t.kind == "INT_L" || t.kind == "INT_R") {
            continue;
        }
        let tmpl = &db.templates[t.template];
        let (Ok(gnd), Ok(vcc)) = (db.wire(ti, "GND_WIRE"), db.wire(ti, "VCC_WIRE")) else {
            continue;
        };
        for (w, name) in tmpl.wires.iter().enumerate() {
            if ["IMUX", "BYP", "FAN", "CTRL"].iter().any(|x| name.starts_with(x)) {
                inputs.insert(t.nodes[w], (gnd, vcc));
            }
        }
    }
    let mut out: FxHashMap<u32, (u32, u32)> =
        wanted.iter().filter_map(|n| inputs.get(n).map(|&c| (*n, c))).collect();
    // Block RAM and DSP pins are one pip beyond an interconnect input.
    for t in db.tiles.iter().filter(|t| t.kind.starts_with("BRAM_") || t.kind.starts_with("DSP_")) {
        for pip in &db.templates[t.template].pips {
            let (src, dst) = (t.nodes[pip.src as usize], t.nodes[pip.dst as usize]);
            if wanted.contains(&dst)
                && !out.contains_key(&dst)
                && let Some(&c) = inputs.get(&src)
            {
                out.insert(dst, c);
            }
        }
    }
    out
}

/// The tile coordinates of a placed cell and the site it is in.
/// Wire delays predicted from a placement: by distance between tiles,
/// nothing within a site and a local hop within a tile.
fn predictor<'a>(
    db: &'a Device,
    nl: &'a Netlist,
    p: &'a Packed,
    pl: &'a Placement,
) -> impl Fn(CellId, NetId) -> f64 + 'a {
    move |c, n| {
        let Some(d) = nl.nets[n].driver else { return 0.0 };
        match (location(db, p, pl, d.cell), location(db, p, pl, c)) {
            (Some((a, sa)), Some((b, sb))) if a == b && sa == sb => 0.0,
            (Some(((ax, ay), _)), Some(((bx, by), _))) => {
                crate::sta::predicted_delay(i32::from(bx - ax), i32::from(by - ay))
            }
            _ => 0.0,
        }
    }
}

fn location(
    db: &Device,
    p: &Packed,
    pl: &Placement,
    c: CellId,
) -> Option<((i16, i16), (usize, usize))> {
    let site = if let Some(&(s, _)) = p.slots.get(&c) {
        &pl.slices[s]
    } else {
        pl.rams.get(&c).or_else(|| pl.dsps.get(&c)).or_else(|| pl.ports.get(&c))?
    };
    let t = &db.tiles[site.tile];
    Some(((t.x, t.y), (site.tile, site.index)))
}

/// Cell pins and their nets, for diagnostics.
fn describe(nl: &Netlist, pins: &[(CellId, String, NetId)]) -> String {
    let names: Vec<_> = pins
        .iter()
        .map(|(c, pin, n)| {
            format!("{}.{pin} ({})", nl.str(nl.cells[*c].name), nl.str(nl.nets[*n].name))
        })
        .collect();
    names.join(", ")
}

/// Some tile wire of routing node `node`, for diagnostics.
fn node_name(db: &Device, node: u32) -> String {
    for t in &db.tiles {
        if let Some(w) = t.nodes.iter().position(|&n| n == node) {
            return format!("{}.{}", t.name, db.templates[t.template].wires[w]);
        }
    }
    "unknown wire".into()
}
fn lut_source(db: &Device, p: &Packed, pl: &Placement, c: CellId) -> Result<u32, String> {
    let (s, l) = p.slots[&c];
    pl.slices[s].node(db, &char::from(b'A' + l as u8).to_string())
}
fn source(db: &Device, nl: &Netlist, p: &Packed, pl: &Placement, n: NetId) -> Result<u32, String> {
    if let Some(c) = p.constant_luts.get(&n) {
        return lut_source(db, p, pl, *c);
    }
    let d = nl.nets[n].driver.ok_or_else(|| format!("undriven net {}", nl.str(nl.nets[n].name)))?;
    if let Some(site) = pl.ports.get(&d.cell) {
        return site.node(db, "I");
    }
    if let Some(site) = pl.rams.get(&d.cell).or_else(|| pl.dsps.get(&d.cell)) {
        let pin = nl.str(nl.cells[d.cell].pins[d.pin as usize].name).replace(['[', ']'], "");
        return site.node(db, &pin);
    }
    let &(s, l) = p.slots.get(&d.cell).ok_or("driver has no placement")?;
    let site = &pl.slices[s];
    let lane = char::from(b'A' + l as u8);
    let pin = nl.str(nl.cells[d.cell].pins[d.pin as usize].name);
    match nl.cell_kind(d.cell) {
        k if pack::is_lut(k) && p.slices[s].lut5[l] == Some(d.cell) => {
            site.node(db, &format!("{lane}MUX"))
        }
        k if pack::is_lut(k) || pack::is_srl(k) => site.node(db, &lane.to_string()),
        // The second bank leaves through the lane's output multiplexer.
        k if pack::is_ff(k) => {
            let lane = char::from(b'A' + (l % 4) as u8);
            site.node(db, &if l < 4 { format!("{lane}Q") } else { format!("{lane}MUX") })
        }
        k if pack::is_ram(k) => {
            let (l, o5) = pack::ram_output(nl, d.cell, pin).ok_or("unknown LUT RAM output")?;
            let lane = char::from(b'A' + l as u8);
            site.node(db, &if o5 { format!("{lane}MUX") } else { lane.to_string() })
        }
        "MUXF7" => site.node(db, if l == 0 { "AMUX" } else { "CMUX" }),
        "MUXF8" => site.node(db, "BMUX"),
        "CARRY4" => {
            if pin == "CO[3]"
                && nl.nets[n].users.iter().any(|u| {
                    nl.cell_kind(u.cell) == "CARRY4"
                        && nl.str(nl.cells[u.cell].pins[u.pin as usize].name) == "CI"
                })
            {
                site.node(db, "COUT")
            } else if pin.starts_with("O[") || pin.starts_with("CO[") {
                let lane = pin.as_bytes()[if pin.starts_with("CO") { 3 } else { 2 }];
                site.node(db, &format!("{}MUX", char::from(b'A' + lane - b'0')))
            } else {
                Err(format!("unlegalized carry output {pin}"))
            }
        }
        k => Err(format!("unsupported driver {k}")),
    }
}
/// The cell input pins (and their nets) at each routed sink node.
pub(crate) type Owners = BTreeMap<u32, Vec<(CellId, String, NetId)>>;
/// Routing requests with their names and nets (none for a clock's route to
/// its buffer), and the cell pins at the sinks.
/// Edges the router may not use: LUT input A6 of lanes whose O5 LUT needs
/// it high.
type Requests = (Vec<synth_route::NetRequest>, Vec<String>, Vec<Option<NetId>>, Owners, Vec<u32>);

/// Whether a lane's LUT inputs may be permuted by the router: plain LUTs
/// without constant inputs (constants route from the interconnect tile's
/// own wires to a known pin).
fn permutable(nl: &Netlist, slice: &pack::Slice, l: usize) -> bool {
    slice.ram.is_none()
        && slice.luts[l].iter().chain(&slice.lut5[l]).all(|&c| pack::is_lut(nl.cell_kind(c)))
        && slice.lane_inputs(nl, l).iter().all(|&n| pack::value(nl, n).is_none())
}

/// The nets on each permuted lane's LUT inputs A1-A6, from the routes.
type LutPins = FxHashMap<(usize, usize), [Option<NetId>; 6]>;

fn lut_pins(
    graph: &RoutingGraph,
    nl: &Netlist,
    p: &Packed,
    pl: &Placement,
    routes: &[synth_route::NetRoute],
) -> LutPins {
    let slices: FxHashMap<(u32, u32), usize> =
        pl.slices.iter().enumerate().map(|(s, x)| ((x.tile as u32, x.index as u32), s)).collect();
    let mut pins = LutPins::default();
    for r in routes {
        for &e in &r.edges {
            let (t, pin) = graph.pip[e as usize];
            if t != crate::routing::VIRTUAL {
                continue;
            }
            let v = (graph.dst[e as usize] - graph.first_virtual) as usize;
            let (tile, site, l) = graph.virtual_lanes[v / 6];
            let Some(&s) = slices.get(&(tile, site)) else { continue };
            let l = usize::from(l);
            let net = p.slices[s].lane_inputs(nl, l)[v % 6];
            pins.entry((s, l)).or_default()[pin as usize] = Some(net);
        }
    }
    pins
}

fn requests(
    db: &Device,
    nl: &Netlist,
    p: &Packed,
    pl: &Placement,
    graph: &RoutingGraph,
) -> Result<Requests, String> {
    // A lane with an O5 LUT holds A6 high: no input may take it.
    let mut blocked = Vec::new();
    for (s, slice) in p.slices.iter().enumerate() {
        for l in 0..4 {
            if slice.lut5[l].is_some() && permutable(nl, slice, l) {
                let site = &pl.slices[s];
                let a6 = site.node(db, &format!("{}6", char::from(b'A' + l as u8)))? as usize;
                let first = graph.lane_sinks[&(site.tile as u32, site.index as u32, l as u8)];
                blocked.extend(
                    (graph.offsets[a6]..graph.offsets[a6 + 1])
                        .filter(|&e| (first..first + 6).contains(&graph.dst[e as usize])),
                );
            }
        }
    }
    let mut sinks: BTreeMap<NetId, BTreeSet<u32>> = BTreeMap::new();
    let mut owners: Owners = BTreeMap::new();
    let mut add = |n: NetId, node: u32, c: CellId, pin: &str| {
        sinks.entry(n).or_default().insert(node);
        owners.entry(node).or_default().push((c, pin.to_string(), n));
    };
    for c in nl.live_cells() {
        if let Some(site) = pl.ports.get(&c) {
            if nl.cell_kind(c) == "$output" {
                let n = nl.pin_net(c, "PAD").ok_or("unconnected output")?;
                add(n, site.node(db, "O")?, c, "PAD");
                let (&zero, _) = p
                    .constant_luts
                    .iter()
                    .find(|(n, _)| !p.constants[n])
                    .ok_or("missing physical ground source")?;
                add(zero, site.node(db, "T")?, c, "T");
            }
            continue;
        }
        if let Some(site) = pl.rams.get(&c) {
            for (pin, n) in crate::physical_bram::inputs(nl, c, p)? {
                add(n, site.node(db, &pin)?, c, &pin);
            }
            continue;
        }
        if let Some(site) = pl.dsps.get(&c) {
            for (pin, n) in crate::physical_dsp::inputs(db, nl, c, site)? {
                add(n, site.node(db, &pin)?, c, &pin);
            }
            continue;
        }
        let (s, l) = p.slots[&c];
        let site = &pl.slices[s];
        let lane = char::from(b'A' + (l % 4) as u8);
        for pin in &nl.cells[c].pins {
            if pin.dir != Dir::Input {
                continue;
            }
            let Some(n) = pin.net else { continue };
            let name = nl.str(pin.name);
            // An unrouted LUT input reads 1 (the interconnect default).
            let tied_high = pack::value(nl, n) == Some(true);
            let target = match nl.cell_kind(c) {
                k if pack::is_srl(k) => match name {
                    "D" if k == "SRL16E" => Some(format!("{lane}X")),
                    "D" => Some(format!("{lane}I")),
                    "CE" => Some("WE".into()),
                    "CLK" => Some("CLK".into()),
                    // Address bit i is LUT input i + 2; input 1 stays high.
                    _ => {
                        let i: usize = name
                            .trim_start_matches('A')
                            .trim_matches(['[', ']'])
                            .parse()
                            .map_err(|_| format!("unknown shift register pin {name}"))?;
                        (!tied_high).then(|| format!("{lane}{}", i + 2))
                    }
                },
                k if pack::is_ram(k) => match name {
                    "WE" => Some("WE".into()),
                    "WCLK" => Some("CLK".into()),
                    _ => {
                        let (port, rest) = name
                            .strip_prefix("ADDR")
                            .map(|r| (true, r))
                            .or_else(|| name.strip_prefix("DI").map(|r| (false, r)))
                            .ok_or_else(|| format!("unknown LUT RAM pin {name}"))?;
                        let x = char::from(rest.as_bytes()[0]);
                        let bit: usize = rest[1..].trim_matches(['[', ']']).parse().unwrap_or(0);
                        if port {
                            (!tied_high).then(|| format!("{x}{}", bit + 1))
                        } else if bit == 1 {
                            // RAM32M: bit 1 is written through DI2, the X input.
                            Some(format!("{x}X"))
                        } else {
                            Some(format!("{x}I"))
                        }
                    }
                },
                "MUXF7" | "MUXF8" => match name {
                    "S" => Some(
                        match (nl.cell_kind(c), l) {
                            ("MUXF8", _) => "BX",
                            (_, 0) => "AX",
                            _ => "CX",
                        }
                        .into(),
                    ),
                    // Data inputs are the slice's own LUT and MUXF7 outputs.
                    _ => None,
                },
                k if pack::is_lut(k) => {
                    let init = pack::param(nl, c, "INIT", 0);
                    let k: usize = k[3..].parse().map_err(|_| "invalid LUT")?;
                    let mask = if k == 6 { u64::MAX } else { (1u64 << (1 << k)) - 1 };
                    if init & mask == 0 || init & mask == mask {
                        continue;
                    }
                    let i = p.slices[s]
                        .lane_inputs(nl, l)
                        .iter()
                        .position(|&x| x == n)
                        .ok_or("LUT input missing from its site")?;
                    // The router picks the input of a permutable lane: the
                    // net's own sink behind all six.
                    if permutable(nl, &p.slices[s], l) {
                        let first =
                            graph.lane_sinks[&(site.tile as u32, site.index as u32, l as u8)];
                        add(n, first + i as u32, c, name);
                        continue;
                    }
                    Some(format!("{lane}{}", i + 1))
                }
                k if pack::is_ff(k) => match name {
                    "D" => (pack::ff_mux(nl, &p.slices[s], l, c) == "BYPASS")
                        .then(|| format!("{lane}X")),
                    "C" => Some("CLK".into()),
                    "CE" => p.slices[s].controls.as_ref().unwrap().ce.map(|_| "CE".into()),
                    "R" | "S" | "CLR" | "PRE" => {
                        p.slices[s].controls.as_ref().unwrap().sr.map(|_| "SR".into())
                    }
                    _ => return Err(format!("unknown FF pin {name}")),
                },
                "CARRY4" => {
                    if name.starts_with("S[") {
                        None
                    } else if name.starts_with("DI[") {
                        let lane = usize::from(name.as_bytes()[3] - b'0');
                        // From the lane's O5 inside the slice, or its X input.
                        (!p.slices[s].di_from_o5(nl, lane))
                            .then(|| format!("{}X", char::from(b'A' + lane as u8)))
                    } else if name == "CI" && pack::value(nl, n).is_none() {
                        Some("CIN".into())
                    } else if matches!(name, "CI" | "CYINIT") {
                        None
                    } else {
                        return Err(format!("unknown carry input {name}"));
                    }
                }
                k => return Err(format!("unsupported input on {k}")),
            };
            if let Some(target) = target {
                add(n, site.node(db, &target)?, c, name);
            }
        }
    }
    let mut requests = Vec::new();
    let mut names = Vec::new();
    let mut nets = Vec::new();
    // Constants come from each interconnect tile's own GND_WIRE (through its
    // GFAN wires) or VCC_WIRE, not from one LUT across the design.
    let local = local_constants(db, &sinks, p);
    for (n, sinks) in sinks {
        if sinks.is_empty() {
            continue;
        }
        let sinks = if let Some(v) = p.constants.get(&n) {
            let mut by_source: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
            let mut rest = BTreeSet::new();
            for s in sinks {
                match local.get(&s) {
                    Some(&(gnd, vcc)) => {
                        by_source.entry(if *v { vcc } else { gnd }).or_default().push(s)
                    }
                    None => {
                        rest.insert(s);
                    }
                }
            }
            for (source, sinks) in by_source {
                requests.push(synth_route::NetRequest { source, sinks, criticality: vec![] });
                names.push(format!("{}:local", nl.str(nl.nets[n].name)));
                nets.push(None);
            }
            if rest.is_empty() {
                continue;
            }
            rest
        } else {
            sinks
        };
        let src = source(db, nl, p, pl, n)?;
        if let Some(buf) = pl.clocks.get(&n) {
            requests.push(synth_route::NetRequest {
                source: src,
                sinks: vec![buf.node(db, "I0")?],
                criticality: vec![0.0],
            });
            names.push(format!("{}:BUFG-input", nl.str(nl.nets[n].name)));
            nets.push(None);
            requests.push(synth_route::NetRequest {
                source: buf.node(db, "O")?,
                sinks: sinks.into_iter().collect(),
                criticality: vec![],
            });
        } else {
            requests.push(synth_route::NetRequest {
                source: src,
                sinks: sinks.into_iter().filter(|&s| s != src).collect(),
                criticality: vec![],
            });
        }
        names.push(nl.str(nl.nets[n].name).into());
        nets.push(Some(n));
    }
    Ok((requests, names, nets, owners, blocked))
}
fn enable(f: &mut BTreeSet<String>, prefix: &str, s: &str) {
    f.insert(format!("{prefix}.{s}"));
}
/// Whether net `n` is read outside the slice's dedicated paths (a flip-flop
/// or MUXF8 of the same slice reading it directly).
fn external(nl: &Netlist, p: &Packed, n: NetId) -> bool {
    let driver = nl.nets[n].driver.and_then(|d| p.slots.get(&d.cell)).map(|&(s, _)| s);
    nl.nets[n].users.iter().any(|u| {
        let Some(&(s, l)) = p.slots.get(&u.cell) else { return true };
        let slice = &p.slices[s];
        let pin = nl.str(nl.cells[u.cell].pins[u.pin as usize].name);
        let internal = Some(s) == driver
            && (slice.f8 == Some(u.cell)
                || slice.carry == Some(u.cell)
                    && pin
                        .strip_prefix("DI[")
                        .and_then(|x| x.strip_suffix(']'))
                        .and_then(|x| x.parse().ok())
                        .is_some_and(|k| slice.di_from_o5(nl, k))
                // Only a flip-flop's data input reads inside the slice: its
                // clock enable and reset come through the interconnect.
                || pack::is_ff(nl.cell_kind(u.cell))
                    && pin == "D"
                    && pack::ff_mux(nl, slice, l, u.cell) != "BYPASS");
        !internal
    })
}

/// The physical INIT and mode features of lane `l`'s LUT, if it is used.
fn lut_config(
    nl: &Netlist,
    s: &pack::Slice,
    l: usize,
    routed: Option<&[Option<NetId>; 6]>,
) -> Result<Option<(u64, Vec<String>)>, String> {
    let lane = char::from(b'A' + l as u8);
    // Data input 1 comes from the lane's I input (D has no other choice).
    let di1 = (l < 3).then(|| format!("DI1MUX.{lane}I"));
    if let Some(r) = s.ram {
        let init = pack::param(nl, r, &format!("INIT_{lane}"), 0);
        if nl.cell_kind(r) == "RAM64M" {
            return Ok(Some((init, ["RAM".to_string()].into_iter().chain(di1).collect())));
        }
        // RAM32M interleaves its two bits; the LUT keeps bit 0 in the
        // lower half (O5) and bit 1 in the upper half (O6, with A6 high).
        let mut phys = 0u64;
        for k in 0..32 {
            phys |= (init >> (2 * k) & 1) << k | (init >> (2 * k + 1) & 1) << (32 + k);
        }
        let modes = ["RAM".to_string(), "SMALL".to_string()].into_iter().chain(di1).collect();
        return Ok(Some((phys, modes)));
    }
    let Some(c) = s.luts[l].or(s.lut5[l]) else { return Ok(None) };
    let kind = nl.cell_kind(c);
    let init = pack::param(nl, c, "INIT", 0);
    if pack::is_srl(kind) {
        // The address drives inputs 2-6 with input 1 high: bit i of the
        // shift register appears at LUT bits 2i and 2i + 1.
        let len = if kind == "SRL16E" { 16 } else { 32 };
        let phys = (0..64).fold(0u64, |t, j| t | (init >> ((j >> 1) % len) & 1) << j);
        let modes = if kind == "SRL16E" {
            vec!["SRL".into(), "SMALL".into()]
        } else {
            ["SRL".to_string()].into_iter().chain(di1).collect()
        };
        return Ok(Some((phys, modes)));
    }
    // Physical input j + 1 carries the lane's j-th input net; unrouted
    // inputs read 1. With an O5 LUT, A6 is high: O6 reads the upper half.
    // A routed permutation names the input of each net instead.
    let pins: Vec<Option<NetId>> = match routed {
        Some(r) => r.to_vec(),
        None => s.lane_inputs(nl, l).into_iter().map(Some).collect(),
    };
    let value = |c: CellId, j: usize| -> Result<u64, String> {
        let mut idx = 0;
        for i in 0..pack::lut_size(nl.cell_kind(c)) {
            let n = nl.pin_net(c, &format!("I{i}")).ok_or("unconnected LUT input")?;
            let pos =
                pins.iter().position(|&x| x == Some(n)).ok_or("LUT input missing from its site")?;
            idx |= (j >> pos & 1) << i;
        }
        Ok(pack::param(nl, c, "INIT", 0) >> idx & 1)
    };
    let mut phys = 0u64;
    for j in 0..64 {
        let cell = match (s.luts[l], s.lut5[l]) {
            (Some(o6), Some(_)) if j >= 32 => o6,
            (_, Some(o5)) if j < 32 => o5,
            (Some(o6), _) => o6,
            (None, Some(o5)) => o5,
            (None, None) => unreachable!(),
        };
        phys |= value(cell, j)? << j;
    }
    Ok(Some((phys, Vec::new())))
}

fn cell_features(
    db: &Device,
    nl: &Netlist,
    p: &Packed,
    pl: &Placement,
    xdc: &Constraints,
    pins: &LutPins,
) -> Result<BTreeSet<String>, String> {
    let mut f = BTreeSet::new();
    for (i, s) in p.slices.iter().enumerate() {
        let prefix = pl.slices[i].prefix(db);
        for l in 0..4 {
            let lane = char::from(b'A' + l as u8);
            let Some((init, modes)) = lut_config(nl, s, l, pins.get(&(i, l)))? else { continue };
            for bit in 0..64 {
                if init >> bit & 1 != 0 {
                    enable(&mut f, &prefix, &format!("{lane}LUT.INIT[{bit}]"));
                }
            }
            for mode in modes {
                enable(&mut f, &prefix, &format!("{lane}LUT.{mode}"));
            }
            if let Some(n) = s.lut5[l].and_then(|c| nl.pin_net(c, "O"))
                && external(nl, p, n)
            {
                enable(&mut f, &prefix, &format!("{lane}OUTMUX.O5"));
            }
            // A RAM32M port's bit 0 leaves through the O5 output.
            if let Some(r) = s.ram.filter(|&r| nl.cell_kind(r) == "RAM32M")
                && nl.pin_net(r, &format!("DO{lane}[0]")).is_some_and(|n| external(nl, p, n))
            {
                enable(&mut f, &prefix, &format!("{lane}OUTMUX.O5"));
            }
        }
        for (half, &mux) in s.f7.iter().enumerate() {
            if let Some(n) = mux.and_then(|c| nl.pin_net(c, "O"))
                && external(nl, p, n)
            {
                enable(&mut f, &prefix, &format!("{}OUTMUX.F7", if half == 0 { 'A' } else { 'C' }));
            }
        }
        if let Some(n) = s.f8.and_then(|c| nl.pin_net(c, "O"))
            && external(nl, p, n)
        {
            enable(&mut f, &prefix, "BOUTMUX.F8");
        }
        if let Some(m) = &s.memory {
            enable(&mut f, &prefix, if m.inverted { "CLKINV" } else { "NOCLKINV" });
        }
        for (k, &ff) in s.ffs.iter().enumerate() {
            if let Some(c) = ff {
                let lane = char::from(b'A' + (k % 4) as u8);
                let kind = nl.cell_kind(c);
                let mux = pack::ff_mux(nl, s, k, c);
                // The second bank takes O5 (IN_A) or the X input (IN_B).
                let (ff, mux) = if k < 4 {
                    (
                        format!("{lane}FF"),
                        if mux == "BYPASS" { format!("{lane}X") } else { mux.into() },
                    )
                } else {
                    enable(&mut f, &prefix, &format!("{lane}OUTMUX.{lane}5Q"));
                    (format!("{lane}5FF"), if mux == "O5" { "IN_A".into() } else { "IN_B".into() })
                };
                enable(&mut f, &prefix, &format!("{ff}MUX.{mux}"));
                if pack::param(nl, c, "INIT", u64::from(matches!(kind, "FDSE" | "FDPE"))) == 0 {
                    enable(&mut f, &prefix, &format!("{ff}.ZINI"));
                }
                if matches!(kind, "FDRE" | "FDCE") {
                    enable(&mut f, &prefix, &format!("{ff}.ZRST"));
                }
            }
        }
        if let Some(ctrl) = &s.controls {
            enable(&mut f, &prefix, if ctrl.inverted { "CLKINV" } else { "NOCLKINV" });
            if ctrl.synchronous {
                enable(&mut f, &prefix, "FFSYNC");
            }
            if ctrl.ce.is_some() {
                enable(&mut f, &prefix, "CEUSEDMUX");
            }
            if ctrl.sr.is_some() {
                enable(&mut f, &prefix, "SRUSEDMUX");
            }
        }
        if let Some(c) = s.carry {
            let ci = nl.pin_net(c, "CI").ok_or("missing CI")?;
            let cy = nl.pin_net(c, "CYINIT").ok_or("missing CYINIT")?;
            let init = if pack::value(nl, ci).is_none() {
                if pack::value(nl, cy) != Some(false) {
                    return Err("dynamic CI requires CYINIT=0".into());
                }
                "CIN"
            } else if pack::value(nl, ci) == Some(true) || pack::value(nl, cy) == Some(true) {
                "C1"
            } else {
                "C0"
            };
            enable(&mut f, &prefix, &format!("PRECYINIT.{init}"));
            for l in 0..4 {
                if s.di_from_o5(nl, usize::from(l)) {
                    enable(&mut f, &prefix, &format!("CARRY4.{}CY0", char::from(b'A' + l)));
                }
                let used =
                    |pin: &str| nl.pin_net(c, pin).is_some_and(|n| !nl.nets[n].users.is_empty());
                if used(&format!("O[{l}]")) {
                    enable(&mut f, &prefix, &format!("{}OUTMUX.XOR", char::from(b'A' + l)));
                } else if used(&format!("CO[{l}]")) {
                    enable(&mut f, &prefix, &format!("{}OUTMUX.CY", char::from(b'A' + l)));
                }
            }
        }
    }
    for (&c, site) in &pl.ports {
        let constraint = &xdc.ports[nl.str(nl.cells[c].name)];
        let tile = &db.tiles[site.tile];
        let template = &db.templates[tile.template].sites[site.index];
        let io = template.pins.get("I").ok_or("IOB missing I pin")?;
        let wire = &db.templates[tile.template].wires[*io as usize];
        let suffix = wire.strip_prefix("IOB_IBUF").ok_or("unsupported IOB layout")?;
        let suffix = if tile.kind.ends_with("_SING")
            && tile.bits.values().any(|r| r.alias.as_ref().is_some_and(|a| a.1 == 0))
        {
            "1"
        } else {
            suffix
        };
        let prefix = format!("{}.IOB_Y{suffix}", tile.name);
        enable(
            &mut f,
            &prefix,
            &format!("PULLTYPE.{}", constraint.pull.as_deref().unwrap_or("NONE")),
        );
        if nl.cell_kind(c) == "$input" {
            enable(&mut f, &prefix, "LVCMOS25_LVCMOS33_LVTTL.IN");
            enable(
                &mut f,
                &prefix,
                "LVCMOS12_LVCMOS15_LVCMOS18_LVCMOS25_LVCMOS33_LVDS_25_LVTTL_SSTL135_SSTL15_TMDS_33.IN_ONLY",
            );
        } else {
            let drive = match constraint.drive.unwrap_or(12) {
                4 => "I4",
                8 => "I12_I8",
                12 | 16 => "I12_I16",
                _ => return Err("unsupported LVCMOS33 drive strength".into()),
            };
            enable(&mut f, &prefix, &format!("LVCMOS33_LVTTL.DRIVE.{drive}"));
            let slew = constraint.slew.as_deref().unwrap_or("SLOW");
            match slew {
                "SLOW" => enable(
                    &mut f,
                    &prefix,
                    "LVCMOS12_LVCMOS15_LVCMOS18_LVCMOS25_LVCMOS33_LVTTL_SSTL135_SSTL15.SLEW.SLOW",
                ),
                "FAST" => enable(
                    &mut f,
                    &prefix,
                    "LVCMOS12_LVCMOS15_LVCMOS18_LVCMOS25_LVCMOS33_LVTTL.SLEW.FAST",
                ),
                _ => return Err("SLEW must be FAST or SLOW".into()),
            }
        }
    }
    for (&c, site) in &pl.rams {
        f.extend(crate::physical_bram::features(db, nl, c, site)?);
    }
    for (&c, site) in &pl.dsps {
        f.extend(crate::physical_dsp::features(db, nl, c, site)?);
    }
    for site in pl.clocks.values() {
        let t = &db.tiles[site.tile];
        let index = crate::device::xy(&db.templates[t.template].sites[site.index].name)?.1;
        let prefix = format!("{}.BUFGCTRL.BUFGCTRL_X0Y{index}", t.name);
        for feature in ["IN_USE", "IS_IGNORE1_INVERTED", "ZINV_CE0", "ZINV_S0"] {
            enable(&mut f, &prefix, feature);
        }
    }
    Ok(f)
}

/// The placement as Vivado BEL and LOC constraints on the netlist's cells
/// (a development aid: Vivado can then route and time synth's placement of
/// its own netlist). Cells Vivado renames are skipped by the guard.
fn placement_xdc(nl: &Netlist, p: &Packed, pl: &Placement) -> String {
    let mut out = String::new();
    let mut add = |c: CellId, bel: Option<String>, site: &str| {
        let name = nl.str(nl.cells[c].name);
        // Cells Vivado renamed or made constant drivers are skipped.
        out.push_str(&format!("if {{[llength [get_cells -quiet {{{name}}}]]}} {{ catch {{\n"));
        if let Some(bel) = bel {
            out.push_str(&format!("  set_property BEL {bel} [get_cells {{{name}}}]\n"));
        }
        out.push_str(&format!("  set_property LOC {site} [get_cells {{{name}}}]\n}} }}\n"));
    };
    for (s, slice) in p.slices.iter().enumerate() {
        let site = &pl.slices[s].name;
        for l in 0..4 {
            let lane = char::from(b'A' + l as u8);
            if let Some(c) = slice.luts[l].filter(|c| !p.constant_luts.values().any(|k| k == c)) {
                add(c, Some(format!("{lane}6LUT")), site);
            }
            if let Some(c) = slice.lut5[l] {
                add(c, Some(format!("{lane}5LUT")), site);
            }
            if let Some(c) = slice.ffs[l] {
                add(c, Some(format!("{lane}FF")), site);
            }
            if let Some(c) = slice.ffs[l + 4] {
                add(c, Some(format!("{lane}5FF")), site);
            }
        }
        if let Some(c) = slice.carry {
            add(c, Some("CARRY4".into()), site);
        }
        if let Some(c) = slice.ram {
            add(c, None, site);
        }
        for (i, c) in slice.f7.iter().enumerate() {
            if let Some(c) = c {
                add(*c, Some(format!("F7{}MUX", if i == 0 { 'A' } else { 'B' })), site);
            }
        }
        if let Some(c) = slice.f8 {
            add(c, Some("F8MUX".into()), site);
        }
    }
    for (c, site) in pl.rams.iter().chain(&pl.dsps) {
        add(*c, None, &site.name);
    }
    out
}

/// synth's packing as Vivado relatively placed macros (a development aid):
/// each carry chain or single slice becomes a macro whose cells keep their
/// BELs and rows, so Vivado's placer places synth's slices.
fn packing_macros(nl: &Netlist, p: &Packed) -> String {
    let mut out = String::new();
    let chained: std::collections::BTreeSet<usize> = p.chains.iter().flatten().copied().collect();
    let groups = p
        .chains
        .iter()
        .cloned()
        .chain((0..p.slices.len()).filter(|s| !chained.contains(s)).map(|s| vec![s]));
    for (k, group) in groups.enumerate() {
        let mut cells = Vec::new();
        for (row, &s) in group.iter().enumerate() {
            let slice = &p.slices[s];
            if slice.ram.is_some() {
                cells.clear();
                break;
            }
            let mut add = |c: CellId, bel: &str| {
                if !p.constant_luts.values().any(|k| *k == c) {
                    cells.push((nl.str(nl.cells[c].name).to_string(), bel.to_string(), row));
                }
            };
            for l in 0..4 {
                let lane = char::from(b'A' + l as u8);
                if let Some(c) = slice.luts[l] {
                    add(c, &format!("{lane}6LUT"));
                }
                if let Some(c) = slice.lut5[l] {
                    add(c, &format!("{lane}5LUT"));
                }
                if let Some(c) = slice.ffs[l] {
                    add(c, &format!("{lane}FF"));
                }
                if let Some(c) = slice.ffs[l + 4] {
                    add(c, &format!("{lane}5FF"));
                }
            }
            if let Some(c) = slice.carry {
                add(c, "CARRY4");
            }
            for (i, c) in slice.f7.iter().enumerate() {
                if let Some(c) = c {
                    add(*c, &format!("F7{}MUX", if i == 0 { 'A' } else { 'B' }));
                }
            }
            if let Some(c) = slice.f8 {
                add(c, "F8MUX");
            }
        }
        if cells.is_empty() {
            continue;
        }
        let names: Vec<String> = cells.iter().map(|(n, _, _)| format!("{{{n}}}")).collect();
        out.push_str(&format!("set cells [get_cells -quiet [list {}]]\n", names.join(" ")));
        out.push_str(&format!("if {{[llength $cells] == {}}} {{\n", cells.len()));
        for (n, bel, _) in &cells {
            out.push_str(&format!("  set_property BEL {bel} [get_cells {{{n}}}]\n"));
        }
        out.push_str(&format!("  create_macro m{k}\n  update_macro m{k} [list"));
        for (n, _, row) in &cells {
            out.push_str(&format!(" {{{n}}} X0Y{row}"));
        }
        out.push_str("]\n}\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use synth_netlist::{Const, Param};

    #[test]
    fn a_flip_flop_enable_from_its_own_slice_leaves_through_the_outmux() {
        // d = a & b on a lane's O6 feeds a flip-flop whose enable e = !a is
        // a LUT packed into the same slice: the enable reaches CE through
        // the interconnect, so its LUT's output must leave the slice.
        let mut nl = Netlist::default();
        let input = |nl: &mut Netlist, name: &str| {
            let c = nl.add_cell(name, synth_netlist::INPUT_PORT);
            let n = nl.add_net(name);
            nl.add_pin(c, "PAD", Dir::Output, Some(n)).unwrap();
            n
        };
        let (clk, a, b) = (input(&mut nl, "clk"), input(&mut nl, "a"), input(&mut nl, "b"));
        let lut = |nl: &mut Netlist, ins: &[NetId], init: i64| {
            let c = nl.add_cell("lut", &format!("LUT{}", ins.len()));
            nl.set_param(c, "INIT", Param::Int(init));
            for (i, &n) in ins.iter().enumerate() {
                nl.add_pin(c, &format!("I{i}"), Dir::Input, Some(n)).unwrap();
            }
            let o = nl.add_net("o");
            nl.add_pin(c, "O", Dir::Output, Some(o)).unwrap();
            o
        };
        let d = lut(&mut nl, &[a, b], 0b1000);
        let e = lut(&mut nl, &[a], 0b01);
        let zero = nl.add_const_net(Const::Zero);
        let ff = nl.add_cell("ff", "FDRE");
        for (pin, n) in [("C", clk), ("D", d), ("CE", e), ("R", zero)] {
            nl.add_pin(ff, pin, Dir::Input, Some(n)).unwrap();
        }
        let q = nl.add_net("q");
        nl.add_pin(ff, "Q", Dir::Output, Some(q)).unwrap();
        let out = nl.add_cell("q", synth_netlist::OUTPUT_PORT);
        nl.add_pin(out, "PAD", Dir::Input, Some(q)).unwrap();
        let p = pack::pack(&mut nl).unwrap();
        let slice = |n: NetId| p.slots[&nl.nets[n].driver.unwrap().cell].0;
        assert_eq!(slice(e), p.slots[&ff].0, "the enable's LUT shares the flip-flop's slice");
        assert!(external(&nl, &p, e));
        assert!(!external(&nl, &p, d));
    }
}
