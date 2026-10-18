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
    let p = pack::pack(&mut nl)?;
    log.push(format!("packed {} slices, {} carry chains", p.slices.len(), p.chains.len()));
    let db = Device::load(root, &opts.part)?;
    log.push(format!("loaded {} nodes in {:.2}s", db.node_x.len(), start.elapsed().as_secs_f64()));
    let placement = place::place(&db, &nl, &p, xdc, opts.seed, opts.effort)?;
    log.push(format!("placed: HPWL {:.0}", placement.hpwl));
    let mut features = cell_features(&db, &nl, &p, &placement, xdc)?;
    let (requests, names, request_nets, owners) = requests(&db, &nl, &p, &placement)?;
    let graph = RoutingGraph::new(&db)?;
    log.push(format!("routing {} physical nets on {} edges", requests.len(), graph.dst.len()));
    for line in &log {
        eprintln!("info: {line}");
    }
    let route_opts = synth_route::RouteOptions {
        astar_cost_per_tile: 0.1,
        astar_delay_per_tile: 10.0,
        bbox_margin: 15,
        max_iterations: 100,
        stall_iterations: 30,
        ..Default::default()
    };
    let mut progress = |iteration: usize, routed: usize, overused: &[usize]| {
        eprintln!(
            "info: route iteration {iteration}: {routed} nets, {} congested nodes",
            overused.len()
        );
    };
    let hooks = synth_route::RouteHooks { progress: Some(&mut progress), ..Default::default() };
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
    let timing = crate::sta::analyze(
        root,
        &db,
        &nl,
        &p,
        &graph,
        &requests,
        &request_nets,
        &routes,
        &owners,
    )?;
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
    log.push(
        "timing is not certified: the estimate uses nextpnr-xilinx's routing model of X-Ray data, \
         uncalibrated against vendor timing or hardware, without clock skew or I/O paths"
            .into(),
    );
    Ok(FlowResult { features, bitstream, log, placement, timing })
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
type Requests = (Vec<synth_route::NetRequest>, Vec<String>, Vec<Option<NetId>>, Owners);

fn requests(db: &Device, nl: &Netlist, p: &Packed, pl: &Placement) -> Result<Requests, String> {
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
            for (pin, n) in crate::physical_dsp::inputs(db, nl, c, site) {
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
                        Some(format!("{}X", char::from(b'A' + name.as_bytes()[3] - b'0')))
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
    for (n, sinks) in sinks {
        if sinks.is_empty() {
            continue;
        }
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
    Ok((requests, names, nets, owners))
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
        let internal = Some(s) == driver
            && (slice.f8 == Some(u.cell)
                || pack::is_ff(nl.cell_kind(u.cell))
                    && pack::ff_mux(nl, slice, l, u.cell) != "BYPASS");
        !internal
    })
}

/// The physical INIT and mode features of lane `l`'s LUT, if it is used.
fn lut_config(
    nl: &Netlist,
    s: &pack::Slice,
    l: usize,
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
    let pins = s.lane_inputs(nl, l);
    let value = |c: CellId, j: usize| -> Result<u64, String> {
        let mut idx = 0;
        for i in 0..pack::lut_size(nl.cell_kind(c)) {
            let n = nl.pin_net(c, &format!("I{i}")).ok_or("unconnected LUT input")?;
            let pos = pins.iter().position(|&x| x == n).ok_or("LUT input missing from its site")?;
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
) -> Result<BTreeSet<String>, String> {
    let mut f = BTreeSet::new();
    for (i, s) in p.slices.iter().enumerate() {
        let prefix = pl.slices[i].prefix(db);
        for l in 0..4 {
            let lane = char::from(b'A' + l as u8);
            let Some((init, modes)) = lut_config(nl, s, l)? else { continue };
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
