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
use crate::physical_dsp::Config;
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
    /// CARRY4 arcs by SDF pin names (`CIN`, `S0`, `DI0` to `CO3`, `O3`...).
    carry: FxHashMap<(String, String), f64>,
    ff_clk_q: f64,
    ff_setup: f64,
    lutram_clk_o: f64,
    lutram_read: f64,
    lutram_setup: f64,
    srl_clk_o: f64,
    srl_setup: f64,
    bram_clk_o: f64,
    bram_setup: f64,
    dsp: Dsp,
}

/// DSP48E1 multiplier timing (ps) by register stage, from the SDF cell of
/// each configuration (no pre-adder, no pattern detection).
struct Dsp {
    /// Combinational A, B and C to P.
    a_p: f64,
    b_p: f64,
    c_p: f64,
    /// Setup into the A, B and C registers, or through the multiplier into
    /// the M register, or into the P register.
    a_reg: f64,
    b_reg: f64,
    c_reg: f64,
    a_m: f64,
    b_m: f64,
    a_p_reg: f64,
    b_p_reg: f64,
    c_p_reg: f64,
    /// Clock to P from each register.
    clk_a: f64,
    clk_b: f64,
    clk_c: f64,
    clk_m: f64,
    clk_p: f64,
    /// Setup of the register clock enables and resets.
    control: f64,
}

impl Dsp {
    fn new(a: &Arcs) -> Result<Self, String> {
        let cell = |k: &str| format!("DSP48E1_{k}");
        let prop = |k: &str, from: &str| {
            a.delay(&cell(k), |f, t| f == from && t == "P")
                .ok_or_else(|| format!("no SDF timing for DSP48E1 {from} to P in {k}"))
        };
        let setup = |k: &str, pin: &str| {
            a.setup(&cell(k), |p| p == pin)
                .ok_or_else(|| format!("no SDF setup for DSP48E1 {pin} in {k}"))
        };
        let (mult_a, mult_b) = ("USE_DPORT_FALSE_USE_MULT_MULTIPLY", "USE_MULT_MULTIPLY");
        let no_detect = "USE_PATTERN_DETECT_NO_PATDET";
        Ok(Dsp {
            a_p: prop(&format!("AREG_0_A_INPUT_DIRECT_MREG_0_PREG_0_{mult_a}"), "A")?,
            b_p: prop(&format!("BREG_0_B_INPUT_DIRECT_MREG_0_PREG_0_{mult_b}"), "B")?,
            c_p: prop("CREG_0_PREG_0", "C")?,
            a_reg: setup("AREG_1_A_INPUT_DIRECT", "A")?,
            b_reg: setup("BREG_1_B_INPUT_DIRECT", "B")?,
            c_reg: setup("CREG_1", "C")?,
            a_m: setup(&format!("AREG_0_A_INPUT_DIRECT_MREG_1_{mult_a}"), "A")?,
            b_m: setup(&format!("BREG_0_B_INPUT_DIRECT_MREG_1_{mult_b}"), "B")?,
            a_p_reg: setup(
                &format!("AREG_0_A_INPUT_DIRECT_MREG_0_PREG_1_{mult_a}_{no_detect}"),
                "A",
            )?,
            b_p_reg: setup(
                &format!("BREG_0_B_INPUT_DIRECT_MREG_0_PREG_1_{mult_b}_{no_detect}"),
                "B",
            )?,
            c_p_reg: setup(&format!("CREG_0_PREG_1_{no_detect}"), "C")?,
            clk_a: prop(&format!("AREG_1_MREG_0_PREG_0_{mult_a}"), "CLK")?,
            clk_b: prop(&format!("BREG_1_MREG_0_PREG_0_{mult_b}"), "CLK")?,
            clk_c: prop("CREG_1_PREG_0", "CLK")?,
            clk_m: prop("MREG_1_PREG_0", "CLK")?,
            clk_p: prop("PREG_1", "CLK")?,
            control: [
                ("AREG_1", "CEA2"),
                ("AREG_1", "RSTA"),
                ("BREG_1", "CEB2"),
                ("MREG_1", "CEM"),
                ("PREG_1", "CEP"),
                ("PREG_1", "RSTP"),
            ]
            .iter()
            .map(|&(k, p)| setup(k, p))
            .try_fold(0.0, |m: f64, d| d.map(|d| m.max(d)))?,
        })
    }

    /// The combinational delay from input `pin` to P, if not registered.
    fn arc(&self, c: &Config, pin: &str) -> Option<f64> {
        let out = c.mreg || c.preg;
        match pin.as_bytes()[0] {
            b'A' if pin.starts_with("A[") && !c.areg && !out => Some(self.a_p),
            b'B' if pin.starts_with("B[") && !c.breg && !out => Some(self.b_p),
            b'C' if pin.starts_with("C[") && c.add_c && !c.creg && !c.preg => Some(self.c_p),
            _ => None,
        }
    }

    /// The setup check of input `pin`, if it reaches a register.
    fn setup(&self, c: &Config, pin: &str) -> Option<f64> {
        let first = |reg: bool, m: Option<f64>, p: f64, r: f64| {
            if reg {
                Some(r)
            } else if let (true, Some(m)) = (c.mreg, m) {
                Some(m)
            } else {
                c.preg.then_some(p)
            }
        };
        if pin.starts_with("A[") {
            first(c.areg, Some(self.a_m), self.a_p_reg, self.a_reg)
        } else if pin.starts_with("B[") {
            first(c.breg, Some(self.b_m), self.b_p_reg, self.b_reg)
        } else if pin.starts_with("C[") && c.add_c {
            first(c.creg, None, self.c_p_reg, self.c_reg)
        } else if pin.starts_with("CE") || pin.starts_with("RST") {
            Some(self.control)
        } else {
            None
        }
    }

    /// Clock to P: from the last register before P.
    fn clk_to_out(&self, c: &Config) -> f64 {
        if c.preg {
            self.clk_p
        } else if c.mreg {
            self.clk_m
        } else {
            [(c.areg, self.clk_a), (c.breg, self.clk_b), (c.creg && c.add_c, self.clk_c)]
                .iter()
                .filter(|r| r.0)
                .map(|r| r.1)
                .fold(0.0, f64::max)
        }
    }
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
            // Carry inputs from CIN, CYINIT and S; DI arcs taken from the
            // lanes' X inputs (the native packer's DI source).
            carry: ["CARRY4", "CARRY4_AX", "CARRY4_BX", "CARRY4_CX", "CARRY4_DX"]
                .iter()
                .flat_map(|k| a.cells(k).flat_map(|c| c.propagation.iter()))
                .filter(|((f, _), _)| {
                    f == "CIN" || f == "CYINIT" || f.starts_with('S') || f.starts_with("DI")
                })
                .fold(FxHashMap::default(), |mut m, ((f, t), &(_, max))| {
                    let d = m.entry((f.clone(), t.clone())).or_insert(0.0);
                    *d = f64::max(*d, max * 1000.0);
                    m
                }),
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
            dsp: Dsp::new(a)?,
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

/// An arrival time (ps) at an output pin, with the input pin it came
/// through (none at a launch), for tracing the path.
#[derive(Clone, Copy)]
struct Arrival {
    time: f64,
    from: Option<u32>,
}

/// A sequential cell: its clock edge, clock-to-output delay and setup.
#[derive(Debug, Clone)]
struct Sequential {
    cell: CellId,
    edge: Edge,
    clk_to_out: f64,
    setup: f64,
    /// The inputs checked and their setups, if not all data inputs alike.
    capture: Option<Vec<(u32, f64)>>,
}

/// The timing graph of a packed design: cell arcs, launch and capture
/// points and a topological order of the combinational cells. Wire delays
/// are supplied per analysis: estimated from placement or routed.
#[derive(Debug)]
pub struct Timer<'a> {
    nl: &'a Netlist,
    /// Combinational cells, fanins first.
    order: Vec<CellId>,
    /// Input pin, output pin and delay of each combinational arc.
    arcs: FxHashMap<CellId, Vec<(u32, u32, f64)>>,
    seq: Vec<Sequential>,
}

/// What an analysis finds: the worst period of each clock and the slack of
/// each cell's reading of a net (on paths within one clock).
#[derive(Debug)]
pub struct Analysis {
    pub clocks: Vec<Clock>,
    pub slack: FxHashMap<(CellId, NetId), f64>,
}

impl Analysis {
    /// Criticality of a cell's reading of a net in [0, 1]: 1 on the worst
    /// path of the slowest clock.
    pub fn criticality(&self, c: CellId, n: NetId) -> f32 {
        let worst = self.clocks.iter().map(|c| c.period).fold(0.0, f64::max);
        match self.slack.get(&(c, n)) {
            Some(&s) if worst > 0.0 => (1.0 - s / worst).clamp(0.0, 1.0) as f32,
            _ => 0.0,
        }
    }
}

impl<'a> Timer<'a> {
    pub fn new(root: &Path, nl: &'a Netlist, p: &Packed) -> Result<Self, String> {
        let model = Model::new(&Arcs::load(root)?)?;
        let kind = |c: CellId| nl.cell_kind(c);
        let pin = |c: CellId, i: usize| nl.str(nl.cells[c].pins[i].name);
        let mut arcs: FxHashMap<CellId, Vec<(u32, u32, f64)>> = FxHashMap::default();
        let mut seq = Vec::new();
        for c in nl.live_cells() {
            let k = kind(c);
            let pins = &nl.cells[c].pins;
            let ins: Vec<usize> = (0..pins.len()).filter(|&i| pins[i].dir == Dir::Input).collect();
            let outs: Vec<usize> =
                (0..pins.len()).filter(|&i| pins[i].dir == Dir::Output).collect();
            let dsp = (k == "DSP48E1").then(|| crate::physical_dsp::validate(nl, c)).transpose()?;
            // Combinational delay from input `i` to output `o`.
            let delay = |i: &str, o: &str| -> Option<f64> {
                Some(match k {
                    k if pack::is_lut(k) => {
                        let (s, l) = p.slots[&c];
                        if p.slices[s].lut5[l] == Some(c) { model.lut5 } else { model.lut6 }
                    }
                    "MUXF7" => model.f7,
                    "MUXF8" => model.f8,
                    "CARRY4" => {
                        // Netlist pins `S[0]`, `CO[3]` are SDF pins `S0`, `CO3`.
                        let from =
                            if i == "CI" { "CIN".to_string() } else { i.replace(['[', ']'], "") };
                        *model.carry.get(&(from, o.replace(['[', ']'], "")))?
                    }
                    "DSP48E1" => model.dsp.arc(dsp.as_ref()?, i)?,
                    // A LUT RAM port reads at its own address.
                    k if pack::is_ram(k)
                        && i.starts_with("ADDR")
                        && o.starts_with("DO")
                        && i.as_bytes().get(4) == o.as_bytes().get(2) =>
                    {
                        model.lutram_read
                    }
                    _ => return None,
                })
            };
            let cell_arcs: Vec<_> = ins
                .iter()
                .flat_map(|&i| outs.iter().map(move |&o| (i, o)))
                .filter_map(|(i, o)| delay(pin(c, i), pin(c, o)).map(|d| (i as u32, o as u32, d)))
                .collect();
            if !cell_arcs.is_empty() {
                arcs.insert(c, cell_arcs);
            }
            if let Some(config) = dsp.filter(|d| d.clocked()) {
                let capture = ins
                    .iter()
                    .filter_map(|&i| model.dsp.setup(&config, pin(c, i)).map(|d| (i as u32, d)))
                    .collect();
                if let Some(n) = nl.pin_net(c, "CLK").filter(|&n| nl.nets[n].constant.is_none()) {
                    seq.push(Sequential {
                        cell: c,
                        edge: (n, false),
                        clk_to_out: model.dsp.clk_to_out(&config),
                        setup: 0.0,
                        capture: Some(capture),
                    });
                }
                continue;
            }
            let (clk, inv, clk_to_out, setup) = if pack::is_ff(k) {
                ("C", "IS_C_INVERTED", model.ff_clk_q, model.ff_setup)
            } else if pack::is_srl(k) {
                ("CLK", "IS_CLK_INVERTED", model.srl_clk_o, model.srl_setup)
            } else if pack::is_ram(k) {
                ("WCLK", "IS_WCLK_INVERTED", model.lutram_clk_o, model.lutram_setup)
            } else if k == "RAMB18E1" {
                ("CLKARDCLK", "IS_CLKARDCLK_INVERTED", model.bram_clk_o, model.bram_setup)
            } else {
                continue;
            };
            if let Some(n) = nl.pin_net(c, clk).filter(|&n| nl.nets[n].constant.is_none()) {
                let edge = (n, pack::param(nl, c, inv, 0) != 0);
                seq.push(Sequential { cell: c, edge, clk_to_out, setup, capture: None });
            }
        }
        // Kahn's algorithm over the cells with combinational arcs.
        let mut pending: FxHashMap<CellId, usize> = FxHashMap::default();
        let mut ready = Vec::new();
        for &c in arcs.keys() {
            let n = nl.cells[c]
                .pins
                .iter()
                .filter(|p| p.dir == Dir::Input)
                .filter_map(|p| p.net)
                .filter(|&n| nl.nets[n].driver.is_some_and(|d| arcs.contains_key(&d.cell)))
                .count();
            pending.insert(c, n);
            if n == 0 {
                ready.push(c);
            }
        }
        ready.sort();
        let mut order = Vec::new();
        while let Some(c) = ready.pop() {
            order.push(c);
            for p in nl.cells[c].pins.iter().filter(|p| p.dir == Dir::Output) {
                for u in p.net.map(|n| nl.nets[n].users.as_slice()).unwrap_or_default() {
                    if let Some(k) = pending.get_mut(&u.cell) {
                        *k -= 1;
                        if *k == 0 {
                            ready.push(u.cell);
                        }
                    }
                }
            }
        }
        if order.len() != arcs.len() {
            return Err("combinational loop in the timing graph".into());
        }
        Ok(Timer { nl, order, arcs, seq })
    }

    /// The timing graph without block RAM and DSP cells: the paths
    /// nextpnr-xilinx times, which has no timing for them.
    pub fn fabric(&self) -> Timer<'a> {
        let nl = self.nl;
        let hard = |c: CellId| matches!(nl.cell_kind(c), "RAMB18E1" | "DSP48E1");
        let arcs: FxHashMap<_, _> =
            self.arcs.iter().filter(|(c, _)| !hard(**c)).map(|(&c, a)| (c, a.clone())).collect();
        Timer {
            nl,
            order: self.order.iter().copied().filter(|c| arcs.contains_key(c)).collect(),
            arcs,
            seq: self.seq.iter().filter(|s| !hard(s.cell)).cloned().collect(),
        }
    }

    /// Analyzes the design with wire delays `wire(cell, net)` (ps) from each
    /// net's driver to each cell reading it.
    pub fn analyze(&self, wire: &dyn Fn(CellId, NetId) -> f64) -> Analysis {
        let nl = self.nl;
        let mut arrival: FxHashMap<(CellId, u32), BTreeMap<Edge, Arrival>> = FxHashMap::default();
        for s in &self.seq {
            for (i, pin) in nl.cells[s.cell].pins.iter().enumerate() {
                if pin.dir == Dir::Output && pin.net.is_some() {
                    let a = Arrival { time: s.clk_to_out, from: None };
                    arrival.entry((s.cell, i as u32)).or_default().insert(s.edge, a);
                }
            }
        }
        // The arrivals at an input pin: its driver's, plus the wire.
        let at_input =
            |arrival: &FxHashMap<(CellId, u32), BTreeMap<Edge, Arrival>>, c: CellId, i: u32| {
                let n = nl.cells[c].pins[i as usize].net?;
                let d = nl.nets[n].driver?;
                let w = wire(c, n);
                arrival
                    .get(&(d.cell, d.pin))
                    .map(|m| m.iter().map(|(&e, a)| (e, a.time + w)).collect::<Vec<_>>())
            };
        for &c in &self.order {
            for &(i, o, d) in &self.arcs[&c] {
                for (e, t) in at_input(&arrival, c, i).unwrap_or_default() {
                    let slot = arrival
                        .entry((c, o))
                        .or_default()
                        .entry(e)
                        .or_insert(Arrival { time: f64::MIN, from: None });
                    if t + d > slot.time {
                        *slot = Arrival { time: t + d, from: Some(i) };
                    }
                }
            }
        }
        // Setup checks: paths within one clock; opposite edges have half a
        // period.
        let checks = |s: &Sequential| -> Vec<(u32, f64)> {
            if let Some(c) = &s.capture {
                return c.clone();
            }
            let pins = &nl.cells[s.cell].pins;
            (0..pins.len() as u32)
                .filter(|&i| {
                    let name = nl.str(pins[i as usize].name);
                    pins[i as usize].dir == Dir::Input
                        && !name.starts_with("CLK")
                        && !matches!(name, "C" | "WCLK")
                })
                .map(|i| (i, s.setup))
                .collect()
        };
        let mut clocks: BTreeMap<NetId, Clock> = BTreeMap::new();
        for s in &self.seq {
            let (clk, falling) = s.edge;
            for (i, setup) in checks(s) {
                for ((launch, launch_falling), t) in
                    at_input(&arrival, s.cell, i).unwrap_or_default()
                {
                    if launch != clk {
                        continue;
                    }
                    let factor = if launch_falling == falling { 1.0 } else { 0.5 };
                    let period = (t + setup) / factor;
                    let worst = clocks.entry(clk).or_insert(Clock {
                        net: clk,
                        period: 0.0,
                        path: Vec::new(),
                    });
                    if period > worst.period {
                        worst.period = period;
                        worst.path = self.trace(&arrival, (launch, launch_falling), (s.cell, i));
                    }
                }
            }
        }
        // Required times, backwards from the setup checks at each clock's
        // worst period: slack is zero on the critical path.
        let mut required: FxHashMap<(CellId, u32), f64> = FxHashMap::default();
        let mut slack: FxHashMap<(CellId, NetId), f64> = FxHashMap::default();
        let mut sink =
            |required: &mut FxHashMap<(CellId, u32), f64>, c: CellId, i: u32, req: f64| {
                let Some(n) = nl.cells[c].pins[i as usize].net else { return };
                let Some(d) = nl.nets[n].driver else { return };
                let w = wire(c, n);
                if let Some(arr) = arrival
                    .get(&(d.cell, d.pin))
                    .map(|m| m.values().map(|a| a.time).fold(f64::MIN, f64::max))
                {
                    let s = slack.entry((c, n)).or_insert(f64::MAX);
                    *s = s.min(req - arr - w);
                }
                let r = required.entry((d.cell, d.pin)).or_insert(f64::MAX);
                *r = r.min(req - w);
            };
        for s in &self.seq {
            let Some(t) = clocks.get(&s.edge.0).map(|c| c.period) else { continue };
            for (i, setup) in checks(s) {
                sink(&mut required, s.cell, i, t - setup);
            }
        }
        for &c in self.order.iter().rev() {
            let mut at_input: FxHashMap<u32, f64> = FxHashMap::default();
            for &(i, o, d) in &self.arcs[&c] {
                if let Some(&r) = required.get(&(c, o)) {
                    let v = at_input.entry(i).or_insert(f64::MAX);
                    *v = v.min(r - d);
                }
            }
            for (i, r) in at_input {
                sink(&mut required, c, i, r);
            }
        }
        Analysis { clocks: clocks.into_values().collect(), slack }
    }

    /// The cell pins of the path ending at input `end`, launch first.
    fn trace(
        &self,
        arrival: &FxHashMap<(CellId, u32), BTreeMap<Edge, Arrival>>,
        edge: Edge,
        end: (CellId, u32),
    ) -> Vec<String> {
        let nl = self.nl;
        let name = |c: CellId, i: u32| {
            let at = arrival
                .get(&(c, i))
                .and_then(|m| m.get(&edge))
                .map(|a| format!(" @{:.0}", a.time))
                .unwrap_or_default();
            format!(
                "{}.{} ({}){at}",
                nl.str(nl.cells[c].name),
                nl.str(nl.cells[c].pins[i as usize].name),
                nl.cell_kind(c)
            )
        };
        let mut path = Vec::new();
        let (mut c, mut i) = end;
        loop {
            path.push(name(c, i));
            let Some(d) = nl.cells[c].pins[i as usize].net.and_then(|n| nl.nets[n].driver) else {
                break;
            };
            path.push(name(d.cell, d.pin));
            match arrival.get(&(d.cell, d.pin)).and_then(|m| m.get(&edge)).and_then(|a| a.from) {
                Some(input) => (c, i) = (d.cell, input),
                None => break,
            }
        }
        path.reverse();
        path
    }
}

/// Routing delay from each net to each cell reading it (none for in-site
/// paths): the worst where a cell reads a net at several pins.
pub fn routed_wires(
    db: &Device,
    graph: &RoutingGraph,
    requests: &[synth_route::NetRequest],
    request_nets: &[Option<NetId>],
    routes: &[synth_route::NetRoute],
    owners: &Owners,
) -> FxHashMap<(CellId, NetId), f64> {
    let routed = net_delays(db, graph, requests, routes);
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
    wire
}

/// nextpnr-xilinx's estimate of the routing delay between two tiles (ps),
/// for timing before routing: vertical distance costs more than horizontal.
pub fn predicted_delay(dx: i32, dy: i32) -> f64 {
    let (dx, dy) = (dx.abs(), dy.abs());
    let base =
        30 * dx.min(18) + 10 * (dx - 18).max(0) + 60 * dy.min(6) + 20 * (dy - 6).max(0) + 300;
    f64::from(base) * 1.5
}
