//! Static timing estimate of a routed design from Project X-Ray delay data.
//!
//! Routing delays follow nextpnr-xilinx's model of the same data (each pip's
//! slow-corner intrinsic delay plus RC terms of its wires), and cell arcs are
//! the slow-corner SDF characterization, so estimates are comparable with
//! nextpnr's. They are not calibrated against vendor timing or hardware.
//! Only register-to-register paths within one clock are analyzed, without
//! clock skew; block RAM arcs are the worst over its modes and pins.
use crate::device::Device;
use crate::flow::Owners;
use crate::pack::{self, Packed};
use crate::routing::RoutingGraph;
use crate::timing::Sdf;
use std::collections::BTreeMap;
use std::path::Path;
use synth_core::FxHashMap;
use synth_netlist::{CellId, Dir, NetId, Netlist};

/// The worst register-to-register path of one clock.
#[derive(Clone, Debug)]
pub struct Clock {
    pub net: NetId,
    /// Minimum period in ps.
    pub period: f64,
    /// Cell pins of the critical path, launch first.
    pub path: Vec<String>,
}

/// Cell arcs (ps), worst over the characterized instances of a cell type.
struct Arcs {
    sdf: Vec<Sdf>,
}

impl Arcs {
    fn load(root: &Path) -> Result<Self, String> {
        let mut sdf = Vec::new();
        for f in ["slicel.sdf", "slicem.sdf", "BRAM_L.sdf", "DSP_L.sdf"] {
            let path = root.join("timings").join(f);
            let text =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            sdf.push(Sdf::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?);
        }
        Ok(Arcs { sdf })
    }

    fn cells<'a>(
        &'a self,
        kind: &'a str,
    ) -> impl Iterator<Item = &'a crate::timing::CellTiming> + 'a {
        self.sdf
            .iter()
            .flat_map(move |s| s.cells.iter().filter(move |((_, k), _)| k == kind).map(|(_, c)| c))
    }

    /// The worst propagation delay of `kind` matching `arc`.
    fn delay(&self, kind: &str, arc: impl Fn(&str, &str) -> bool) -> Option<f64> {
        self.cells(kind)
            .flat_map(|c| c.propagation.iter())
            .filter(|((from, to), _)| arc(from, to))
            .map(|(_, &(_, max))| max * 1000.0)
            .reduce(f64::max)
    }

    /// The worst setup check of `kind` on a pin matching `pin`.
    fn setup(&self, kind: &str, pin: impl Fn(&str) -> bool) -> Option<f64> {
        self.cells(kind)
            .flat_map(|c| c.setup.iter())
            .filter(|((p, _, _), _)| pin(p))
            .map(|(_, &d)| d * 1000.0)
            .reduce(f64::max)
    }
}

/// A clock edge: the clock net and whether it is the falling edge.
type Edge = (NetId, bool);

/// The delay model of every cell kind the physical flow implements.
struct Model {
    lut6: f64,
    lut5: f64,
    f7: f64,
    f8: f64,
    carry_cin: f64,
    carry_s: f64,
    carry_di: f64,
    ff_clk_q: f64,
    ff_setup: f64,
    lutram_clk_o: f64,
    lutram_read: f64,
    lutram_setup: f64,
    srl_clk_o: f64,
    srl_setup: f64,
    bram_clk_o: f64,
    bram_setup: f64,
    dsp: f64,
}

impl Model {
    fn new(a: &Arcs) -> Result<Self, String> {
        let need =
            |v: Option<f64>, what: &str| v.ok_or_else(|| format!("no SDF timing for {what}"));
        let any = |_: &str, _: &str| true;
        Ok(Model {
            lut6: need(a.delay("LUT6", |f, t| f.starts_with('A') && t == "O6"), "LUT6")?,
            lut5: need(a.delay("LUT5", |f, t| f.starts_with('A') && t == "O5"), "LUT5")?,
            f7: need(a.delay("SELMUX2_1", any), "MUXF7")?,
            f8: need(a.delay("SELMUX2_1", any), "MUXF8")?,
            carry_cin: need(a.delay("CARRY4", |f, _| f == "CIN" || f == "CYINIT"), "CARRY4 CIN")?,
            carry_s: need(a.delay("CARRY4", |f, _| f.starts_with('S')), "CARRY4 S")?,
            carry_di: ["CARRY4_AX", "CARRY4_BX", "CARRY4_CX", "CARRY4_DX"]
                .iter()
                .filter_map(|k| a.delay(k, |f, _| f.starts_with("DI")))
                .reduce(f64::max)
                .ok_or("no SDF timing for CARRY4 DI")?,
            ff_clk_q: need(a.delay("REG_INIT_FF", |f, t| f == "CLK" && t == "Q"), "FF clock")?,
            ff_setup: need(a.setup("REG_INIT_FF", |_| true), "FF setup")?,
            lutram_clk_o: need(a.delay("LUT_OR_MEM6LRAM", |f, _| f == "CLK"), "LUT RAM clock")?,
            lutram_read: need(
                a.delay("LUT_OR_MEM6LRAM", |f, _| f.starts_with('A')),
                "LUT RAM read",
            )?,
            lutram_setup: ["LUT_OR_MEM6LRAM", "LUT_OR_MEM5LRAM"]
                .iter()
                .filter_map(|k| a.setup(k, |_| true))
                .reduce(f64::max)
                .ok_or("no SDF timing for LUT RAM setup")?,
            srl_clk_o: a
                .delay("LUT_OR_MEM6SHFREG", |f, _| f == "CLK")
                .or(a.delay("LUT_OR_MEM6LRAM", |f, _| f == "CLK"))
                .ok_or("no SDF timing for shift register clock")?,
            srl_setup: ["LUT_OR_MEM6SHFREG", "LUT_OR_MEM6LRAM"]
                .iter()
                .filter_map(|k| a.setup(k, |_| true))
                .reduce(f64::max)
                .ok_or("no SDF timing for shift register setup")?,
            bram_clk_o: a
                .sdf
                .iter()
                .flat_map(|s| s.cells.values())
                .flat_map(|c| c.propagation.iter())
                .filter(|((f, t), _)| f.starts_with("CLK") && t.starts_with("DO"))
                .map(|(_, &(_, max))| max * 1000.0)
                .reduce(f64::max)
                .ok_or("no SDF timing for block RAM outputs")?,
            bram_setup: a
                .sdf
                .iter()
                .flat_map(|s| s.cells.iter().filter(|((_, k), _)| k.starts_with("RAMBFIFO36E1")))
                .flat_map(|(_, c)| c.setup.iter())
                .filter(|((p, _, _), _)| {
                    ["ADDR", "DI", "EN", "WE"].iter().any(|x| p.starts_with(x))
                })
                .map(|(_, &d)| d * 1000.0)
                .reduce(f64::max)
                .ok_or("no SDF timing for block RAM setup")?,
            dsp: a
                .sdf
                .iter()
                .flat_map(|s| s.cells.iter().filter(|((_, k), _)| k.starts_with("DSP48E1")))
                .flat_map(|(_, c)| c.propagation.iter())
                .filter(|((f, t), _)| {
                    (f.starts_with('A') || f.starts_with('B')) && t.starts_with('P')
                })
                .map(|(_, &(_, max))| max * 1000.0)
                .reduce(f64::max)
                .unwrap_or(0.0),
        })
    }
}

/// Routing delay of each routed (net, sink node), nextpnr-xilinx's way:
/// a pip costs its intrinsic delay, plus the RC delay of the driving
/// wires' resistance (scaled by the distance from the pip driving them)
/// into its input capacitance, plus, through pass transistors, the RC
/// delay into the wire it drives.
fn net_delays(
    db: &Device,
    graph: &RoutingGraph,
    requests: &[synth_route::NetRequest],
    routes: &[synth_route::NetRoute],
) -> FxHashMap<(usize, u32), f64> {
    let mut out = FxHashMap::default();
    for (i, (req, route)) in requests.iter().zip(routes).enumerate() {
        // The edge into each node of the route tree.
        let mut parent: FxHashMap<u32, u32> = FxHashMap::default();
        for &e in &route.edges {
            parent.insert(graph.dst[e as usize], e);
        }
        let mut delay: FxHashMap<u32, f64> = FxHashMap::default();
        delay.insert(req.source, 0.0);
        for &sink in &req.sinks {
            // The path from the source, walked back from the sink.
            let mut path = Vec::new();
            let mut at = sink;
            while !delay.contains_key(&at) {
                let Some(&e) = parent.get(&at) else { break };
                path.push(e);
                at = edge_src(graph, e);
            }
            let mut t = delay.get(&at).copied().unwrap_or(0.0);
            for &e in path.iter().rev() {
                let (ti, pi) = graph.pip[e as usize];
                let tile = &db.tiles[ti as usize];
                let tmpl = &db.templates[tile.template];
                let pip = &tmpl.pips[pi as usize];
                // The distance from the pip driving this pip's input wire.
                let src_len = parent
                    .get(&edge_src(graph, e))
                    .map(|&d| {
                        let from = &db.tiles[graph.pip[d as usize].0 as usize];
                        i32::from((from.x - tile.x).abs()) + i32::from((from.y - tile.y).abs())
                    })
                    .unwrap_or(1)
                    .max(1) as f64;
                let (src_r, _) = tmpl.wire_rc[pip.src as usize];
                let (_, dst_c) = tmpl.wire_rc[pip.dst as usize];
                // Resistance in ohms and capacitance in fF: ohm x fF / 1000 = ps.
                let r = src_len * f64::from(src_r) + f64::from(pip.res);
                let mut d = f64::from(pip.delay.unwrap_or(0.0)) + r * f64::from(pip.cap) / 1000.0;
                if !pip.buffered {
                    d += (f64::from(src_r) + f64::from(pip.res)) * f64::from(dst_c) / 1000.0;
                }
                t += d;
                delay.insert(graph.dst[e as usize], t);
            }
            out.insert((i, sink), t);
        }
    }
    out
}

fn edge_src(graph: &RoutingGraph, e: u32) -> u32 {
    // Edges are stored by source node: find the node whose range holds `e`.
    graph.offsets.partition_point(|&o| o <= e) as u32 - 1
}

/// A timing point: the launching edge and the arrival (ps), with the
/// predecessor pin for reporting the path.
#[derive(Clone, Copy)]
struct Arrival {
    time: f64,
    from: Option<(CellId, u32)>,
}

/// Analyzes the routed design: the worst register-to-register period of
/// each clock.
#[allow(clippy::too_many_arguments)]
pub fn analyze(
    root: &Path,
    db: &Device,
    nl: &Netlist,
    p: &Packed,
    graph: &RoutingGraph,
    requests: &[synth_route::NetRequest],
    request_nets: &[Option<NetId>],
    routes: &[synth_route::NetRoute],
    owners: &Owners,
) -> Result<Vec<Clock>, String> {
    let model = Model::new(&Arcs::load(root)?)?;
    let routed = net_delays(db, graph, requests, routes);
    // Routing delay from each net to each cell reading it (none for
    // in-site paths); the worst where a cell reads a net at several pins.
    let mut wire: FxHashMap<(CellId, NetId), f64> = FxHashMap::default();
    for (i, req) in requests.iter().enumerate() {
        let Some(net) = request_nets[i] else { continue };
        for &sink in &req.sinks {
            for &(c, _, n) in owners.get(&sink).into_iter().flatten() {
                if n == net {
                    let d = routed.get(&(i, sink)).copied().unwrap_or(0.0);
                    let w = wire.entry((c, n)).or_insert(0.0);
                    *w = w.max(d);
                }
            }
        }
    }
    let kind = |c: CellId| nl.cell_kind(c);
    let pin_name = |c: CellId, i: u32| nl.str(nl.cells[c].pins[i as usize].name).to_string();
    // Sequential cells launch on their clock edge and capture at it.
    let clock_of = |c: CellId| -> Option<Edge> {
        let k = kind(c);
        let (pin, inv) = if pack::is_ff(k) {
            ("C", "IS_C_INVERTED")
        } else if pack::is_srl(k) {
            ("CLK", "IS_CLK_INVERTED")
        } else if pack::is_ram(k) {
            ("WCLK", "IS_WCLK_INVERTED")
        } else if k == "RAMB18E1" {
            ("CLKARDCLK", "IS_CLKARDCLK_INVERTED")
        } else {
            return None;
        };
        let n = nl.pin_net(c, pin)?;
        (nl.nets[n].constant.is_none()).then(|| (n, pack::param(nl, c, inv, 0) != 0))
    };
    // Arrival times at output pins, per launching edge, in topological
    // order of the combinational cells.
    let mut arrival: FxHashMap<(CellId, u32), BTreeMap<Edge, Arrival>> = FxHashMap::default();
    let comb = |c: CellId| {
        let k = kind(c);
        pack::is_lut(k) || matches!(k, "MUXF7" | "MUXF8" | "CARRY4" | "DSP48E1") || pack::is_ram(k)
    };
    // Combinational delay from input pin `i` to output pin `o` of cell `c`.
    let arc = |c: CellId, i: &str, o: &str| -> Option<f64> {
        let k = kind(c);
        Some(match k {
            k if pack::is_lut(k) => {
                let (s, l) = p.slots[&c];
                if p.slices[s].lut5[l] == Some(c) { model.lut5 } else { model.lut6 }
            }
            "MUXF7" => model.f7,
            "MUXF8" => model.f8,
            "CARRY4" => match i {
                "CI" | "CYINIT" => model.carry_cin,
                i if i.starts_with('S') => model.carry_s,
                _ => model.carry_di,
            },
            "DSP48E1" => model.dsp,
            k if pack::is_ram(k) && i.starts_with("ADDR") && o.starts_with("DO") => {
                // A port reads at its own address.
                if i.as_bytes().get(4) != o.as_bytes().get(2) {
                    return None;
                }
                model.lutram_read
            }
            _ => return None,
        })
    };
    // Launch arrivals of sequential outputs.
    for c in nl.live_cells() {
        let Some(edge) = clock_of(c) else { continue };
        let k = kind(c);
        let t = if pack::is_ff(k) {
            model.ff_clk_q
        } else if pack::is_srl(k) {
            model.srl_clk_o
        } else if pack::is_ram(k) {
            model.lutram_clk_o
        } else {
            model.bram_clk_o
        };
        for (i, pin) in nl.cells[c].pins.iter().enumerate() {
            if pin.dir == Dir::Output && pin.net.is_some() {
                arrival
                    .entry((c, i as u32))
                    .or_default()
                    .insert(edge, Arrival { time: t, from: None });
            }
        }
    }
    // Kahn's algorithm over combinational cells.
    let driver = |n: NetId| nl.nets[n].driver.filter(|d| comb(d.cell));
    let mut pending: FxHashMap<CellId, usize> = FxHashMap::default();
    let mut ready = Vec::new();
    for c in nl.live_cells().filter(|&c| comb(c)) {
        let n = nl.cells[c]
            .pins
            .iter()
            .filter(|p| p.dir == Dir::Input)
            .filter_map(|p| p.net)
            .filter(|&n| driver(n).is_some())
            .count();
        pending.insert(c, n);
        if n == 0 {
            ready.push(c);
        }
    }
    // The arrivals at an input pin: its driver's, plus routing.
    let input_arrival = |arrival: &FxHashMap<(CellId, u32), BTreeMap<Edge, Arrival>>,
                         c: CellId,
                         pin: usize| {
        let p = &nl.cells[c].pins[pin];
        let n = p.net?;
        let d = nl.nets[n].driver?;
        let delay = wire.get(&(c, n)).copied().unwrap_or(0.0);
        let at = arrival.get(&(d.cell, d.pin))?;
        Some(
            at.iter()
                .map(|(&e, a)| (e, Arrival { time: a.time + delay, from: Some((d.cell, d.pin)) }))
                .collect::<Vec<_>>(),
        )
    };
    let mut visited = 0;
    while let Some(c) = ready.pop() {
        visited += 1;
        let outs: Vec<usize> = (0..nl.cells[c].pins.len())
            .filter(|&i| nl.cells[c].pins[i].dir == Dir::Output)
            .collect();
        for &o in &outs {
            let oname = pin_name(c, o as u32);
            let mut best: BTreeMap<Edge, Arrival> =
                arrival.get(&(c, o as u32)).cloned().unwrap_or_default();
            for i in (0..nl.cells[c].pins.len()).filter(|&i| nl.cells[c].pins[i].dir == Dir::Input)
            {
                let Some(d) = arc(c, &pin_name(c, i as u32), &oname) else { continue };
                for (e, a) in input_arrival(&arrival, c, i).unwrap_or_default() {
                    let t = a.time + d;
                    let slot = best.entry(e).or_insert(Arrival { time: f64::MIN, from: None });
                    if t > slot.time {
                        *slot = Arrival { time: t, from: Some((c, i as u32)) };
                    }
                }
            }
            if !best.is_empty() {
                arrival.insert((c, o as u32), best);
            }
            if let Some(n) = nl.cells[c].pins[o].net {
                for u in &nl.nets[n].users {
                    if let Some(k) = pending.get_mut(&u.cell) {
                        *k -= 1;
                        if *k == 0 {
                            ready.push(u.cell);
                        }
                    }
                }
            }
        }
    }
    if visited != pending.len() {
        return Err("combinational loop in the timing graph".into());
    }
    // Setup checks at the data inputs of sequential cells.
    let mut clocks: BTreeMap<NetId, Clock> = BTreeMap::new();
    for c in nl.live_cells() {
        let Some((clk, falling)) = clock_of(c) else { continue };
        let k = kind(c);
        let setup = if pack::is_ff(k) {
            model.ff_setup
        } else if pack::is_srl(k) {
            model.srl_setup
        } else if pack::is_ram(k) {
            model.lutram_setup
        } else {
            model.bram_setup
        };
        for i in 0..nl.cells[c].pins.len() {
            let pin = &nl.cells[c].pins[i];
            let name = nl.str(pin.name);
            if pin.dir != Dir::Input || name.starts_with("CLK") || matches!(name, "C" | "WCLK") {
                continue;
            }
            for ((launch, launch_falling), a) in input_arrival(&arrival, c, i).unwrap_or_default() {
                // Only paths within one clock; opposite edges have half a period.
                if launch != clk {
                    continue;
                }
                let factor = if launch_falling == falling { 1.0 } else { 0.5 };
                let period = (a.time + setup) / factor;
                let worst =
                    clocks.entry(clk).or_insert(Clock { net: clk, period: 0.0, path: Vec::new() });
                if period > worst.period {
                    worst.period = period;
                    worst.path =
                        trace(nl, &arrival, (launch, launch_falling), a.from, (c, i as u32));
                }
            }
        }
    }
    Ok(clocks.into_values().collect())
}

/// The cell pins of a path ending at `end`, launch first.
fn trace(
    nl: &Netlist,
    arrival: &FxHashMap<(CellId, u32), BTreeMap<Edge, Arrival>>,
    edge: Edge,
    mut from: Option<(CellId, u32)>,
    end: (CellId, u32),
) -> Vec<String> {
    let name = |(c, i): (CellId, u32)| {
        format!("{}.{}", nl.str(nl.cells[c].name), nl.str(nl.cells[c].pins[i as usize].name))
    };
    let mut path = vec![name(end)];
    let mut guard = 0;
    while let Some(pin) = from {
        path.push(name(pin));
        guard += 1;
        if guard > 100_000 {
            break;
        }
        // An output pin's arrival records the input it came through; that
        // input's arrival is its driver's output.
        let next = arrival.get(&pin).and_then(|m| m.get(&edge)).and_then(|a| a.from);
        from = match next {
            Some((c, i)) if nl.cells[c].pins[i as usize].dir == Dir::Input => {
                path.push(name((c, i)));
                nl.cells[c].pins[i as usize]
                    .net
                    .and_then(|n| nl.nets[n].driver)
                    .map(|d| (d.cell, d.pin))
            }
            other => other,
        };
    }
    path.reverse();
    path
}
