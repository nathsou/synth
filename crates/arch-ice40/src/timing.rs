//! Static timing analysis with the IceStorm timing models (worst-case
//! corner): register-to-register paths per clock, with routed net delays.
//! Clocks are ideal (no skew), as in `icetime`.

use synth_core::{FxHashMap, FxHashSet};
use synth_netlist::{CellId, NetId, Netlist, Param};

use crate::arch::Arch;
use crate::chipdb::ArcKind;
use crate::pack::{GB, IO, LC, RAM};
use crate::place::Placement;
use crate::route::{Routing, pin_node};

/// Timing of one clock domain.
#[derive(Clone, Debug)]
pub struct ClockTiming {
    /// Name of the clock net.
    pub clock: String,
    /// Longest register-to-register path, in nanoseconds.
    pub period_ns: f64,
    pub fmax_mhz: f64,
    /// Cells along the critical path, from launch to capture.
    pub path: Vec<String>,
}

/// Results of [`analyse`].
#[derive(Clone, Debug, Default)]
pub struct TimingReport {
    pub clocks: Vec<ClockTiming>,
}

/// Worst delay of a timing arc, matching pin names without their edge
/// (`posedge:`) qualifiers.
struct Arcs<'a> {
    arch: &'a Arch,
    cache: std::cell::RefCell<FxHashMap<(String, u8, String, String), f64>>,
}

impl Arcs<'_> {
    fn get(&self, cell: &str, kind: ArcKind, from: &str, to: &str) -> f64 {
        let key = (cell.to_string(), kind as u8, from.to_string(), to.to_string());
        if let Some(&v) = self.cache.borrow().get(&key) {
            return v;
        }
        let strip = |s: &str| s.rsplit(':').next().unwrap_or(s).to_string();
        let v = self
            .arch
            .db
            .timing
            .iter()
            .find(|c| c.name == cell)
            .map(|c| {
                c.arcs
                    .iter()
                    .filter(|a| a.kind == kind && strip(&a.from) == from && strip(&a.to) == to)
                    .map(|a| a.rise.max(a.fall) as f64)
                    .fold(0.0, f64::max)
            })
            .unwrap_or(0.0);
        self.cache.borrow_mut().insert(key, v);
        v
    }
}

/// Routed delay from each net's source to each of its nodes.
fn net_node_delays(arch: &Arch, routing: &Routing, net: NetId) -> FxHashMap<u32, f64> {
    let g = &arch.graph;
    let mut incoming: FxHashMap<u32, u32> = FxHashMap::default();
    if let Some(edges) = routing.net_edges.get(&net) {
        for &e in edges {
            incoming.insert(g.edge_dst[e as usize], e);
        }
    }
    let mut delay: FxHashMap<u32, f64> = FxHashMap::default();
    if let Some(src) = routing.net_source.get(&net) {
        delay.insert(src.raw(), 0.0);
    }
    let src_of = |e: u32| (g.offsets.partition_point(|&o| o as usize <= e as usize) - 1) as u32;
    let nodes: Vec<u32> = incoming.keys().copied().collect();
    for n in nodes {
        // Walk back to a node with a known delay, then fill in forward.
        let mut chain = Vec::new();
        let mut cur = n;
        while !delay.contains_key(&cur) {
            let Some(&e) = incoming.get(&cur) else { break };
            chain.push((cur, e));
            cur = src_of(e);
        }
        let mut d = delay.get(&cur).copied().unwrap_or(0.0);
        for &(node, e) in chain.iter().rev() {
            d += g.edge_delay[e as usize] as f64;
            delay.insert(node, d);
        }
    }
    delay
}

/// Analyses a placed and routed design.
pub fn analyse(
    arch: &Arch,
    nl: &Netlist,
    placement: &Placement,
    routing: &Routing,
) -> TimingReport {
    let arcs = Arcs { arch, cache: Default::default() };
    let pin_name = |c: CellId, i: usize| nl.str(nl.cells[c].pins[i].name).to_string();
    let param = |c: CellId, p: &str| nl.param(c, p).and_then(Param::as_u64).unwrap_or(0);
    // Delay from each net's driver to each sink pin.
    let mut sink_delay: FxHashMap<(CellId, usize), f64> = FxHashMap::default();
    for n in nl.live_nets() {
        if nl.nets[n].driver.is_none() {
            continue;
        }
        let d = net_node_delays(arch, routing, n);
        for p in &nl.nets[n].users {
            let name = pin_name(p.cell, p.pin as usize);
            if let Ok(node) = pin_node(arch, nl, placement, p.cell, &name) {
                let v = d.get(&node.raw()).copied().unwrap_or(0.0);
                sink_delay.insert((p.cell, p.pin as usize), v);
            }
        }
    }
    // Clock domains: nets on the clock pins of registers.
    let clock_of = |c: CellId| -> Option<NetId> {
        match nl.cell_kind(c) {
            LC if param(c, "DFF_ENABLE") != 0 => nl.pin_net(c, "CLK"),
            RAM => nl.pin_net(c, "RCLK").or_else(|| nl.pin_net(c, "WCLK")),
            _ => None,
        }
    };
    let mut clocks: Vec<NetId> =
        nl.live_cells().filter_map(clock_of).collect::<FxHashSet<_>>().into_iter().collect();
    clocks.sort();
    let mut report = TimingReport::default();
    for clk in clocks {
        let delay = |c: CellId, i: usize| sink_delay.get(&(c, i)).copied().unwrap_or(0.0);
        let mut sta = Sta {
            nl,
            arcs: &arcs,
            sink_delay: &delay,
            clk: Some(clk),
            arrival: FxHashMap::default(),
            pred: FxHashMap::default(),
            visiting: FxHashSet::default(),
        };
        // Capture points of the domain.
        let mut worst = (0.0f64, None::<(CellId, usize)>);
        for c in nl.live_cells() {
            if clock_of(c) != Some(clk) {
                continue;
            }
            let kind = nl.cell_kind(c).to_string();
            for (i, pin) in nl.cells[c].pins.iter().enumerate() {
                if pin.dir != synth_netlist::Dir::Input || pin.net.is_none() {
                    continue;
                }
                let name = pin_name(c, i);
                let Some(setup) = setup_time(&arcs, &kind, &name) else { continue };
                let a = sta.pin_arrival(c, i);
                if a.is_finite() && a + setup > worst.0 {
                    worst = (a + setup, Some((c, i)));
                }
            }
        }
        let Some(end) = worst.1 else { continue };
        let mut path = Vec::new();
        let mut cur = Some(end);
        while let Some((c, i)) = cur {
            let t = sta.arrival.get(&(c, i)).copied().unwrap_or(0.0) / 1000.0;
            let label = format!("{}.{} @{t:.2}", nl.str(nl.cells[c].name), pin_name(c, i));
            if path.last() != Some(&label) {
                path.push(label);
            }
            cur = sta.pred.get(&(c, i)).copied();
        }
        path.reverse();
        let ns = worst.0 / 1000.0;
        report.clocks.push(ClockTiming {
            clock: nl.str(nl.nets[clk].name).to_string(),
            period_ns: ns,
            fmax_mhz: 1000.0 / ns,
            path,
        });
    }
    report
}

/// Setup time (ps) of a register input pin, if the pin is a timing endpoint.
fn setup_time(arcs: &Arcs, kind: &str, pin: &str) -> Option<f64> {
    Some(match kind {
        LC => match pin {
            "I0" | "I1" | "I2" | "I3" => arcs.get(
                "LogicCell40",
                ArcKind::Setup,
                &pin.to_lowercase().replace('i', "in"),
                "clk",
            ),
            "CEN" => arcs.get("LogicCell40", ArcKind::Setup, "ce", "clk"),
            "SR" => arcs.get("LogicCell40", ArcKind::Setup, "sr", "clk"),
            _ => return None,
        },
        RAM => {
            if pin.starts_with("RCLK") || pin.starts_with("WCLK") {
                return None;
            }
            let ck = if pin.starts_with('R') { "RCLK" } else { "WCLK" };
            arcs.get("SB_RAM40_4K", ArcKind::Setup, pin, ck)
        }
        _ => return None,
    })
}

/// Criticality (0..1) of every connection, keyed by its input pin, for net
/// delays given by `delay` (all clocks together), and the longest path (ps).
pub(crate) fn criticalities(
    arch: &Arch,
    nl: &Netlist,
    delay: &dyn Fn(CellId, usize) -> f64,
) -> (f64, FxHashMap<(CellId, usize), f64>) {
    let arcs = Arcs { arch, cache: Default::default() };
    let param = |c: CellId, p: &str| nl.param(c, p).and_then(Param::as_u64).unwrap_or(0);
    let is_reg = |c: CellId| match nl.cell_kind(c) {
        LC => param(c, "DFF_ENABLE") != 0,
        RAM => true,
        _ => false,
    };
    let mut sta = Sta {
        nl,
        arcs: &arcs,
        sink_delay: delay,
        clk: None,
        arrival: FxHashMap::default(),
        pred: FxHashMap::default(),
        visiting: FxHashSet::default(),
    };
    // Endpoints and the longest path.
    let mut ends: Vec<(CellId, usize, f64)> = Vec::new();
    let mut period = 1.0f64;
    for c in nl.live_cells().filter(|&c| is_reg(c)) {
        let kind = nl.cell_kind(c).to_string();
        for (i, pin) in nl.cells[c].pins.iter().enumerate() {
            if pin.dir != synth_netlist::Dir::Input || pin.net.is_none() {
                continue;
            }
            let name = nl.str(pin.name).to_string();
            let Some(setup) = setup_time(&arcs, &kind, &name) else { continue };
            let a = sta.pin_arrival(c, i);
            if a.is_finite() {
                period = period.max(a + setup);
                ends.push((c, i, setup));
            }
        }
    }
    // Required times, backwards from the endpoints.
    let mut required: FxHashMap<(CellId, usize), f64> = FxHashMap::default();
    for &(c, i, setup) in &ends {
        required.insert((c, i), period - setup);
    }
    // Process pins in decreasing arrival order: a pin's fanout arrives later.
    // (Inputs before outputs on ties: zero-delay connections, as along carry
    // chains, have equal arrival at both ends.)
    let is_input = |k: &(CellId, usize)| nl.cells[k.0].pins[k.1].dir == synth_netlist::Dir::Input;
    let mut pins: Vec<((CellId, usize), f64)> =
        sta.arrival.iter().filter(|(_, a)| a.is_finite()).map(|(&k, &a)| (k, a)).collect();
    pins.sort_by(|a, b| b.1.total_cmp(&a.1).then(is_input(&b.0).cmp(&is_input(&a.0))));
    let mut crit: FxHashMap<(CellId, usize), f64> = FxHashMap::default();
    for ((c, i), _) in pins {
        let pin = &nl.cells[c].pins[i];
        let Some(&req) = required.get(&(c, i)) else { continue };
        if pin.dir == synth_netlist::Dir::Input {
            // Back through the net to its driver.
            if let Some(d) = pin.net.and_then(|n| nl.nets[n].driver) {
                let r = req - delay(c, i);
                let e = required.entry((d.cell, d.pin as usize)).or_insert(f64::INFINITY);
                *e = e.min(r);
                let arr = sta.arrival.get(&(d.cell, d.pin as usize)).copied().unwrap_or(0.0);
                let slack = req - (arr + delay(c, i));
                crit.insert((c, i), (1.0 - slack / period).clamp(0.0, 1.0));
            }
        } else {
            // Back through the cell to the inputs that reach this output.
            let ins = sta.cell_arcs(c, i);
            for (j, d) in ins {
                let e = required.entry((c, j)).or_insert(f64::INFINITY);
                *e = e.min(req - d);
            }
        }
    }
    (period, crit)
}

struct Sta<'a> {
    nl: &'a Netlist,
    arcs: &'a Arcs<'a>,
    /// Delay of the connection to an input pin from its net's driver.
    sink_delay: &'a dyn Fn(CellId, usize) -> f64,
    /// Launch clock; `None` takes every register as a start point.
    clk: Option<NetId>,
    arrival: FxHashMap<(CellId, usize), f64>,
    pred: FxHashMap<(CellId, usize), (CellId, usize)>,
    visiting: FxHashSet<(CellId, usize)>,
}

impl Sta<'_> {
    fn pin(&self, c: CellId, name: &str) -> Option<usize> {
        let sym = self.nl.names.get(name)?;
        self.nl.cells[c].pin_index(sym)
    }

    /// Latest arrival time (ps) at a pin, from registers of this clock
    /// (`-inf` if none reaches it).
    fn pin_arrival(&mut self, c: CellId, i: usize) -> f64 {
        if let Some(&a) = self.arrival.get(&(c, i)) {
            return a;
        }
        if !self.visiting.insert((c, i)) {
            return f64::NEG_INFINITY; // combinational loop
        }
        let nl = self.nl;
        let pin = &nl.cells[c].pins[i];
        let r = if pin.dir == synth_netlist::Dir::Input {
            match pin.net.and_then(|n| nl.nets[n].driver) {
                Some(d) => {
                    let a = self.pin_arrival(d.cell, d.pin as usize);
                    if a.is_finite() {
                        self.pred.insert((c, i), (d.cell, d.pin as usize));
                    }
                    a + (self.sink_delay)(c, i)
                }
                None => f64::NEG_INFINITY,
            }
        } else {
            self.output_arrival(c, i)
        };
        self.visiting.remove(&(c, i));
        self.arrival.insert((c, i), r);
        r
    }

    /// Input pins (and arc delays) that drive output pin `i` of cell `c`
    /// combinationally.
    fn cell_arcs(&self, c: CellId, i: usize) -> Vec<(usize, f64)> {
        let nl = self.nl;
        let name = nl.str(nl.cells[c].pins[i].name).to_string();
        let kind = nl.cell_kind(c).to_string();
        let param = |p: &str| nl.param(c, p).and_then(Param::as_u64).unwrap_or(0);
        let mut v = Vec::new();
        let mut add = |pin: &str, d: f64| {
            if let Some(j) = self.pin(c, pin)
                && nl.cells[c].pins[j].net.is_some()
            {
                v.push((j, d));
            }
        };
        match (kind.as_str(), name.as_str()) {
            (LC, "O") if param("DFF_ENABLE") == 0 => {
                for (p, a) in [("I0", "in0"), ("I1", "in1"), ("I2", "in2"), ("I3", "in3")] {
                    add(p, self.arcs.get("LogicCell40", ArcKind::IoPath, a, "lcout"));
                }
                if nl.pin_net(c, "I3").is_some() && nl.pin_net(c, "I3") == nl.pin_net(c, "CIN") {
                    add("CIN", self.arcs.get("LogicCell40", ArcKind::IoPath, "in3", "lcout"));
                }
            }
            (LC, "COUT") => {
                for (p, a) in [("I1", "in1"), ("I2", "in2"), ("CIN", "carryin")] {
                    add(p, self.arcs.get("LogicCell40", ArcKind::IoPath, a, "carryout"));
                }
            }
            (GB, _) => add(
                "USER_SIGNAL_TO_GLOBAL_BUFFER",
                self.arcs.get(
                    "ICE_GB",
                    ArcKind::IoPath,
                    "USERSIGNALTOGLOBALBUFFER",
                    "GLOBALBUFFEROUTPUT",
                ),
            ),
            _ => {}
        }
        v
    }

    fn output_arrival(&mut self, c: CellId, i: usize) -> f64 {
        let nl = self.nl;
        let name = nl.str(nl.cells[c].pins[i].name).to_string();
        let kind = nl.cell_kind(c).to_string();
        let param = |p: &str| nl.param(c, p).and_then(Param::as_u64).unwrap_or(0);
        let mut best = f64::NEG_INFINITY;
        let through = |sta: &mut Self, pin: &str, delay: f64, best: &mut f64| {
            if let Some(j) = sta.pin(c, pin)
                && nl.cells[c].pins[j].net.is_some()
            {
                let a = sta.pin_arrival(c, j) + delay;
                if a > *best {
                    *best = a;
                    sta.pred.insert((c, i), (c, j));
                }
            }
        };
        match (kind.as_str(), name.as_str()) {
            (LC, "O") if param("DFF_ENABLE") != 0 => {
                if self.clk.is_none_or(|k| nl.pin_net(c, "CLK") == Some(k)) {
                    best = self.arcs.get("LogicCell40", ArcKind::IoPath, "clk", "lcout");
                }
            }
            (LC, "O") => {
                for (p, a) in [("I0", "in0"), ("I1", "in1"), ("I2", "in2"), ("I3", "in3")] {
                    let d = self.arcs.get("LogicCell40", ArcKind::IoPath, a, "lcout");
                    through(self, p, d, &mut best);
                }
                // The carry input reaches the LUT through in3.
                if nl.pin_net(c, "I3").is_some() && nl.pin_net(c, "I3") == nl.pin_net(c, "CIN") {
                    let d = self.arcs.get("LogicCell40", ArcKind::IoPath, "in3", "lcout");
                    through(self, "CIN", d, &mut best);
                }
            }
            (LC, "COUT") => {
                for (p, a) in [("I1", "in1"), ("I2", "in2"), ("CIN", "carryin")] {
                    let d = self.arcs.get("LogicCell40", ArcKind::IoPath, a, "carryout");
                    through(self, p, d, &mut best);
                }
            }
            (RAM, n) if n.starts_with("RDATA") => {
                if self.clk.is_none_or(|k| nl.pin_net(c, "RCLK") == Some(k)) {
                    best = self.arcs.get("SB_RAM40_4K", ArcKind::IoPath, "RCLK", n);
                }
            }
            (GB, _) => {
                let d = self.arcs.get(
                    "ICE_GB",
                    ArcKind::IoPath,
                    "USERSIGNALTOGLOBALBUFFER",
                    "GLOBALBUFFEROUTPUT",
                );
                through(self, "USER_SIGNAL_TO_GLOBAL_BUFFER", d, &mut best);
            }
            (IO, _) => {} // inputs from outside are not register paths
            _ => {}
        }
        best
    }
}
