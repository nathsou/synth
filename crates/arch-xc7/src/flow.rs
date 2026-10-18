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
    let (requests, names) = requests(&db, &nl, &p, &placement)?;
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
        .map_err(|e| {
            format!(
                "{e:?}; physical nets: {}",
                names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| format!("{i}={n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
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
    log.push(
        "timing is not certified: complete cell and wire RC timing integration remains required"
            .into(),
    );
    Ok(FlowResult { features, bitstream, log, placement })
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
    if let Some(site) = pl.rams.get(&d.cell) {
        let pin = nl.str(nl.cells[d.cell].pins[d.pin as usize].name).replace(['[', ']'], "");
        return site.node(db, &pin);
    }
    let &(s, l) = p.slots.get(&d.cell).ok_or("driver has no placement")?;
    let site = &pl.slices[s];
    let lane = char::from(b'A' + l as u8);
    let pin = nl.str(nl.cells[d.cell].pins[d.pin as usize].name);
    match nl.cell_kind(d.cell) {
        k if pack::is_lut(k) => site.node(db, &lane.to_string()),
        k if pack::is_ff(k) => site.node(db, &format!("{lane}Q")),
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
fn requests(
    db: &Device,
    nl: &Netlist,
    p: &Packed,
    pl: &Placement,
) -> Result<(Vec<synth_route::NetRequest>, Vec<String>), String> {
    let mut sinks: BTreeMap<NetId, BTreeSet<u32>> = BTreeMap::new();
    for c in nl.live_cells() {
        if let Some(site) = pl.ports.get(&c) {
            if nl.cell_kind(c) == "$output" {
                let n = nl.pin_net(c, "PAD").ok_or("unconnected output")?;
                sinks.entry(n).or_default().insert(site.node(db, "O")?);
                let (&zero, _) = p
                    .constant_luts
                    .iter()
                    .find(|(n, _)| !p.constants[n])
                    .ok_or("missing physical ground source")?;
                sinks.entry(zero).or_default().insert(site.node(db, "T")?);
            }
            continue;
        }
        if let Some(site) = pl.rams.get(&c) {
            for (pin, n) in crate::physical_bram::inputs(nl, c, p)? {
                sinks.entry(n).or_default().insert(site.node(db, &pin)?);
            }
            continue;
        }
        let (s, l) = p.slots[&c];
        let site = &pl.slices[s];
        let lane = char::from(b'A' + l as u8);
        for pin in &nl.cells[c].pins {
            if pin.dir != Dir::Input {
                continue;
            }
            let Some(n) = pin.net else { continue };
            let name = nl.str(pin.name);
            let target = match nl.cell_kind(c) {
                k if pack::is_lut(k) => {
                    let init = pack::param(nl, c, "INIT", 0);
                    let k: usize = k[3..].parse().map_err(|_| "invalid LUT")?;
                    let mask = if k == 6 { u64::MAX } else { (1u64 << (1 << k)) - 1 };
                    if init & mask == 0 || init & mask == mask {
                        continue;
                    }
                    let i: usize = name
                        .strip_prefix('I')
                        .ok_or("unexpected LUT pin")?
                        .parse()
                        .map_err(|_| "invalid LUT input")?;
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
                sinks.entry(n).or_default().insert(site.node(db, &target)?);
            }
        }
    }
    let mut requests = Vec::new();
    let mut names = Vec::new();
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
    }
    Ok((requests, names))
}
fn enable(f: &mut BTreeSet<String>, prefix: &str, s: &str) {
    f.insert(format!("{prefix}.{s}"));
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
        for (l, &lut) in s.luts.iter().enumerate() {
            if let Some(c) = lut {
                let k: usize = nl.cell_kind(c)[3..].parse().map_err(|_| "invalid LUT type")?;
                let init = pack::param(nl, c, "INIT", 0);
                let lane = char::from(b'A' + l as u8);
                for bit in 0..64 {
                    if init >> (bit % (1 << k)) & 1 != 0 {
                        enable(&mut f, &prefix, &format!("{lane}LUT.INIT[{bit}]"));
                    }
                }
            }
        }
        for (l, &ff) in s.ffs.iter().enumerate() {
            if let Some(c) = ff {
                let lane = char::from(b'A' + l as u8);
                let kind = nl.cell_kind(c);
                let mux = pack::ff_mux(nl, s, l, c);
                let mux = if mux == "BYPASS" { format!("{lane}X") } else { mux.into() };
                enable(&mut f, &prefix, &format!("{lane}FFMUX.{mux}"));
                if pack::param(nl, c, "INIT", u64::from(matches!(kind, "FDSE" | "FDPE"))) == 0 {
                    enable(&mut f, &prefix, &format!("{lane}FF.ZINI"));
                }
                if matches!(kind, "FDRE" | "FDCE") {
                    enable(&mut f, &prefix, &format!("{lane}FF.ZRST"));
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
