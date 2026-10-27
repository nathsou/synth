//! Static timing analysis with Project Trellis' timing data, as nextpnr-ecp5
//! times a design: register-to-register paths per clock against requested
//! frequencies, through the cells' worst-case arcs and the routing's switch
//! delays (each with an adder per branch the net takes from its source
//! wire). One timing graph serves placement and routing (with estimated
//! connection delays) and the final analysis (with routed ones). Clocks are
//! ideal (no skew); a path between two clocks is timed against the
//! capturing clock, and a path between the two edges of a clock counts
//! double towards its period. Carries and a flip-flop's data from its own
//! cell's LUT are dedicated connections, without delay.

use synth_core::FxHashMap;
use synth_netlist::{CellId, Dir, NetId, Netlist, Param};

use crate::arch::Arch;
use crate::cells::{CCU2C, DP16KD, FF, LUT4, MULT18X18D};
use crate::pack::lut_init;
use crate::place::Placement;
use crate::route::Routing;
use crate::speed::{CellTiming, Speed};

/// Timing of one clock domain.
#[derive(Clone, Debug)]
pub struct ClockTiming {
    /// The clock: its input port or the net's name.
    pub clock: String,
    /// Longest register-to-register path, in nanoseconds.
    pub period_ns: f64,
    pub fmax_mhz: f64,
    /// The requested frequency, if constrained.
    pub target_mhz: Option<f64>,
    /// Cells along the critical path, from launch to capture.
    pub path: Vec<String>,
}

/// Results of [`analyse`].
#[derive(Clone, Debug, Default)]
pub struct TimingReport {
    pub clocks: Vec<ClockTiming>,
}

/// Clock targets: frequencies (MHz) by clock name (port or net), and one
/// for the clocks not named. Clocks without a target are analysed against
/// their own worst path.
#[derive(Clone, Debug, Default)]
pub struct ClockConstraints {
    pub default_mhz: Option<f64>,
    pub nets: Vec<(String, f64)>,
}

impl ClockConstraints {
    fn target_ps(&self, names: &[&str]) -> Option<f64> {
        names
            .iter()
            .find_map(|&name| self.nets.iter().find(|(n, _)| n == name))
            .map(|&(_, f)| f)
            .or(self.default_mhz)
            .map(|f| 1e6 / f)
    }
}

/// A net connection: from the driver pin of a net to one of its sink pins.
#[derive(Clone, Copy, Debug)]
pub struct Connection {
    pub driver: CellId,
    pub driver_pin: usize,
    pub sink: CellId,
    pub pin: usize,
}

/// A clock domain of the timing graph.
#[derive(Clone, Debug)]
pub struct Clock {
    pub net: NetId,
    pub name: String,
    pub target_ps: Option<f64>,
}

#[derive(Clone, Copy, Debug)]
enum Edge {
    /// A net connection (index into `conns`), whose delay the caller gives.
    Net(u32),
    /// A combinational arc through a cell.
    Cell(f32),
}

/// The timing graph of a netlist: one node per cell pin, edges for net
/// connections and for the combinational arcs of cells, register outputs
/// as start points and register inputs as end points, per clock.
#[derive(Debug)]
pub struct TimingGraph {
    base: Vec<u32>,
    node_pin: Vec<(CellId, u16)>,
    fanin_off: Vec<u32>,
    fanin: Vec<(u32, Edge)>,
    /// Nodes in topological order (nodes on combinational loops left out).
    order: Vec<u32>,
    pub conns: Vec<Connection>,
    sink_conn: Vec<u32>,
    /// Start points: node, clock-to-output delay, clock, falling edge.
    starts: Vec<(u32, f64, u16, bool)>,
    /// End points: node, setup time, clock, falling edge.
    ends: Vec<(u32, f64, u16, bool)>,
    pub clocks: Vec<Clock>,
}

/// Results of [`TimingGraph::analyse`].
#[derive(Debug)]
pub struct Analysis {
    /// Worst delay of the paths captured by each clock (ps).
    pub period: Vec<f64>,
    /// Criticality (0..1) of each connection, its slack relative to the
    /// target of the clock its most critical path ends at (the clock's
    /// period if it misses the target).
    pub criticality: Vec<f64>,
    /// Least slack of each connection (ps; infinite off timed paths).
    pub slack: Vec<f64>,
    /// Critical path into each clock: cell, pin and arrival (ps).
    pub paths: Vec<Option<Vec<(CellId, usize, f64)>>>,
}

fn str_param<'a>(nl: &'a Netlist, c: CellId, name: &str, default: &'a str) -> &'a str {
    match nl.param(c, name) {
        Some(Param::Str(s)) => s.as_str(),
        _ => default,
    }
}

/// The block RAM timing of its ports' configuration.
fn bram_timing(speed: &Speed) -> Option<&CellTiming> {
    speed.cells.get("DP16KD:REGMODE_A=NOREG,REGMODE_B=NOREG")
}

/// The setup time of block RAM input `pin` before its port's clock: the
/// pin's own, else the worst of its bus.
fn bram_setup(t: &CellTiming, pin: &str, clock: &str) -> f64 {
    if let Some(&(s, _)) = t.setup.get(&(pin.to_string(), clock.to_string())) {
        return s as f64;
    }
    let bus = pin.trim_end_matches(|c: char| c.is_ascii_digit());
    t.setup
        .iter()
        .filter(|((p, c), _)| c == clock && p.trim_end_matches(|c: char| c.is_ascii_digit()) == bus)
        .map(|(_, &(s, _))| s as f64)
        .fold(0.0, f64::max)
}

/// Whether a 16-entry table depends on input `k`.
fn depends(table: u16, k: usize) -> bool {
    (0..16).any(|i| (table >> i & 1) != (table >> (i ^ 1 << k) & 1))
}

/// The port (`A` or `B`) of a block RAM pin (`ADA5`, `CEB`, `DOA0`, ...).
fn bram_port(pin: &str) -> Option<char> {
    let base = pin.trim_end_matches(|c: char| c.is_ascii_digit());
    base.chars().last().filter(|c| matches!(c, 'A' | 'B'))
}

/// Whether a connection is dedicated (no routing): a carry, or a
/// flip-flop's data from the LUT of its own logic cell.
pub fn dedicated(nl: &Netlist, placement: &Placement, c: &Connection) -> bool {
    let dpin = nl.str(nl.cells[c.driver].pins[c.driver_pin].name);
    let spin = nl.str(nl.cells[c.sink].pins[c.pin].name);
    if dpin == "COUT" && spin == "CIN" {
        return true;
    }
    if nl.cell_kind(c.sink) != FF || spin != "DI" {
        return false;
    }
    let (Some(a), Some(b)) = (placement.sites.get(&c.driver), placement.sites.get(&c.sink)) else {
        return false;
    };
    let offset = match (nl.cell_kind(c.driver), dpin) {
        (LUT4, "Z") | (CCU2C, "S0") => 0,
        (CCU2C, "S1") => 1,
        _ => return false,
    };
    (a.x, a.y, a.z + offset) == (b.x, b.y, b.z)
}

/// nextpnr-ecp5's estimate of a connection's delay over `(dx, dy)` tiles
/// (`Arch::predictDelay`), for speed grade `grade`.
pub fn estimate(grade: &str, dx: usize, dy: usize) -> f64 {
    let speed: f64 = match grade {
        "6" => 6.0,
        "7" => 7.0,
        _ => 8.0,
    };
    let near = dx.min(5) + dy.min(5);
    let far = dx.saturating_sub(5) + dy.saturating_sub(5);
    (80.0 - 9.0 * speed) * (6 + far + 2 * near) as f64
}

impl TimingGraph {
    pub fn new(arch: &Arch, nl: &Netlist, constraints: &ClockConstraints) -> Self {
        let speed = &arch.speed;
        let slice = speed.cells.get("SLOGICB").cloned().unwrap_or_default();
        let carry = speed.cells.get("SCCU2C").cloned().unwrap_or_default();
        let bram = bram_timing(speed).cloned().unwrap_or_default();
        let arc = |t: &CellTiming, from: &str, to: &str| -> f32 {
            t.delays.get(&(from.to_string(), to.to_string())).copied().unwrap_or(0.0)
        };
        let setup = |t: &CellTiming, pin: &str| -> f64 {
            t.setup.get(&(pin.to_string(), "CLK".to_string())).map_or(0.0, |s| s.0 as f64)
        };
        let mut base = vec![u32::MAX; nl.cells.len()];
        let mut node_pin = Vec::new();
        for c in nl.live_cells() {
            base[c.index()] = node_pin.len() as u32;
            for i in 0..nl.cells[c].pins.len() {
                node_pin.push((c, i as u16));
            }
        }
        let n = node_pin.len();
        let node = |c: CellId, i: usize| base[c.index()] + i as u32;
        let pin_of = |c: CellId, name: &str| -> Option<usize> {
            let i = nl.cells[c].pin_index(nl.names.get(name)?)?;
            nl.cells[c].pins[i].net.is_some().then_some(i)
        };
        let mut edges: Vec<Vec<(u32, Edge)>> = vec![Vec::new(); n];
        let mut conns = Vec::new();
        let mut sink_conn = vec![u32::MAX; n];
        for net in nl.live_nets() {
            let Some(d) = nl.nets[net].driver else { continue };
            for u in &nl.nets[net].users {
                let k = conns.len() as u32;
                conns.push(Connection {
                    driver: d.cell,
                    driver_pin: d.pin as usize,
                    sink: u.cell,
                    pin: u.pin as usize,
                });
                let to = node(u.cell, u.pin as usize);
                sink_conn[to as usize] = k;
                edges[to as usize].push((node(d.cell, d.pin as usize), Edge::Net(k)));
            }
        }
        let mut clocks: Vec<Clock> = Vec::new();
        let mut clock_index = |net: NetId| -> u16 {
            if let Some(k) = clocks.iter().position(|c| c.net == net) {
                return k as u16;
            }
            // A clock from an input port is named after it.
            let name = nl.nets[net]
                .driver
                .map(|d| nl.str(nl.cells[d.cell].name).to_string())
                .unwrap_or_else(|| nl.str(nl.nets[net].name).to_string());
            let target_ps = constraints.target_ps(&[&name, nl.str(nl.nets[net].name)]);
            clocks.push(Clock { net, name, target_ps });
            (clocks.len() - 1) as u16
        };
        let (mut starts, mut ends) = (Vec::new(), Vec::new());
        for c in nl.live_cells() {
            let mut cell_arc = |from: Option<usize>, to: Option<usize>, d: f32| {
                if let (Some(f), Some(t)) = (from, to) {
                    edges[node(c, t) as usize].push((node(c, f), Edge::Cell(d)));
                }
            };
            match nl.cell_kind(c) {
                LUT4 => {
                    let init = lut_init(nl, c, "INIT");
                    for (k, p) in ["A", "B", "C", "D"].into_iter().enumerate() {
                        if depends(init, k) {
                            let d = arc(&slice, &format!("{p}0"), "F0");
                            cell_arc(pin_of(c, p), pin_of(c, "Z"), d);
                        }
                    }
                }
                CCU2C => {
                    // Each half's sum is P ^ carry in (P with INJECT1),
                    // its carry out P ? carry in : G: arcs only where the
                    // tables depend on an input, so that a chain's first
                    // half (P constant 0) does not depend on the carry in.
                    let (s0, s1, cout) = (pin_of(c, "S0"), pin_of(c, "S1"), pin_of(c, "COUT"));
                    let p = [lut_init(nl, c, "INIT0"), lut_init(nl, c, "INIT1")];
                    let g = [p[0] & 0xf, p[1] & 0xf];
                    let p_of = |h: usize| (p[h] >> 12) & 0xf;
                    let sum_reads_carry =
                        |h: usize| str_param(nl, c, &format!("INJECT1_{h}"), "YES") == "NO";
                    let propagates = |h: usize| p_of(h) != 0;
                    let into_carry = |h: usize, k: usize| depends(p[h], k) || depends(g[h], k);
                    for (k, n) in ["A", "B", "C", "D"].into_iter().enumerate() {
                        let (i0, i1) = (pin_of(c, &format!("{n}0")), pin_of(c, &format!("{n}1")));
                        if depends(p[0], k) {
                            cell_arc(i0, s0, arc(&carry, &format!("{n}0"), "F0"));
                        }
                        if into_carry(0, k) && sum_reads_carry(1) {
                            cell_arc(i0, s1, arc(&carry, &format!("{n}0"), "F1"));
                        }
                        if into_carry(0, k) && propagates(1) {
                            cell_arc(i0, cout, arc(&carry, &format!("{n}0"), "FCO"));
                        }
                        if depends(p[1], k) {
                            cell_arc(i1, s1, arc(&carry, &format!("{n}1"), "F1"));
                        }
                        if into_carry(1, k) {
                            cell_arc(i1, cout, arc(&carry, &format!("{n}1"), "FCO"));
                        }
                    }
                    let cin = pin_of(c, "CIN");
                    if sum_reads_carry(0) {
                        cell_arc(cin, s0, arc(&carry, "FCI", "F0"));
                    }
                    if propagates(0) && sum_reads_carry(1) {
                        cell_arc(cin, s1, arc(&carry, "FCI", "F1"));
                    }
                    if propagates(0) && propagates(1) {
                        cell_arc(cin, cout, arc(&carry, "FCI", "FCO"));
                    }
                }
                FF => {
                    let Some(clk) = nl.pin_net(c, "CLK") else { continue };
                    let k = clock_index(clk);
                    let neg = str_param(nl, c, "CLKMUX", "CLK") == "INV";
                    if let Some(q) = pin_of(c, "Q") {
                        starts.push((node(c, q), arc(&slice, "CLK", "Q0") as f64, k, neg));
                    }
                    for (p, s) in [("DI", "DI0"), ("CE", "CE"), ("LSR", "LSR")] {
                        if let Some(i) = pin_of(c, p) {
                            ends.push((node(c, i), setup(&slice, s), k, neg));
                        }
                    }
                }
                DP16KD => {
                    for (i, pin) in nl.cells[c].pins.iter().enumerate() {
                        let name = nl.str(pin.name);
                        let Some(port) = bram_port(name) else { continue };
                        let clock = format!("CLK{port}");
                        if pin.net.is_none() || name == clock {
                            continue;
                        }
                        let Some(clk) = nl.pin_net(c, &clock) else { continue };
                        let k = clock_index(clk);
                        let neg = str_param(nl, c, &format!("{clock}MUX"), "") == "INV";
                        if name.starts_with("DO") {
                            let d = arc(&bram, &clock, name).max(arc(
                                &bram,
                                &clock,
                                &format!("DO{port}0"),
                            ));
                            starts.push((node(c, i), d as f64, k, neg));
                        } else if pin.dir == Dir::Input {
                            ends.push((node(c, i), bram_setup(&bram, name, &clock), k, neg));
                        }
                    }
                }
                MULT18X18D => {
                    // Without registers: every input to every output.
                    let t = speed.cells.get("MULT18X18D:REGS=NONE").cloned().unwrap_or_default();
                    let outs: Vec<usize> =
                        (0..36).filter_map(|i| pin_of(c, &format!("P{i}"))).collect();
                    for (i, pin) in nl.cells[c].pins.iter().enumerate() {
                        let name = nl.str(pin.name);
                        if pin.dir != Dir::Input || pin.net.is_none() {
                            continue;
                        }
                        let from = if name.starts_with("SIGNED") { name } else { &name[..1] };
                        let d = arc(&t, from, "P");
                        for &o in &outs {
                            cell_arc(Some(i), Some(o), d);
                        }
                    }
                }
                _ => {} // ports are not register paths
            }
        }
        // Register outputs start paths; nothing propagates into them.
        for &(s, _, _, _) in &starts {
            edges[s as usize].clear();
        }
        // Each node after its fanins (depth first), cutting the edges that
        // close a combinational loop, if any.
        let mut order: Vec<u32> = Vec::with_capacity(n);
        let mut state = vec![0u8; n]; // 0 new, 1 on the stack, 2 done
        let mut stack: Vec<(u32, usize)> = Vec::new();
        let mut cut: Vec<(u32, u32)> = Vec::new();
        for root in 0..n as u32 {
            if state[root as usize] != 0 {
                continue;
            }
            state[root as usize] = 1;
            stack.push((root, 0));
            while let Some(&mut (v, ref mut next)) = stack.last_mut() {
                let fanin = &edges[v as usize];
                if *next < fanin.len() {
                    let from = fanin[*next].0;
                    *next += 1;
                    match state[from as usize] {
                        0 => {
                            state[from as usize] = 1;
                            stack.push((from, 0));
                        }
                        1 => cut.push((from, v)),
                        _ => {}
                    }
                } else {
                    state[v as usize] = 2;
                    order.push(v);
                    stack.pop();
                }
            }
        }
        for &(from, to) in &cut {
            edges[to as usize].retain(|&(f, _)| f != from);
        }
        if !cut.is_empty() && std::env::var_os("SYNTH_DEBUG").is_some() {
            eprintln!("debug: {} combinational loop edges cut", cut.len());
        }
        let mut fanin_off = Vec::with_capacity(n + 1);
        let mut fanin = Vec::new();
        fanin_off.push(0);
        for e in &edges {
            fanin.extend_from_slice(e);
            fanin_off.push(fanin.len() as u32);
        }
        TimingGraph {
            base,
            node_pin,
            fanin_off,
            fanin,
            order,
            conns,
            sink_conn,
            starts,
            ends,
            clocks,
        }
    }

    /// The connection ending at input pin `pin` of `cell`.
    pub fn conn_to(&self, cell: CellId, pin: usize) -> Option<usize> {
        let b = *self.base.get(cell.index())?;
        if b == u32::MAX {
            return None;
        }
        let k = self.sink_conn[(b + pin as u32) as usize];
        (k != u32::MAX).then_some(k as usize)
    }

    /// Arrival and required times for connection delays `delay` (ps,
    /// indexed like `conns`), paths timed against the target of the clock
    /// that captures them (clocks without a target take the worst path of
    /// the design as theirs).
    pub fn analyse(&self, delay: &[f64]) -> Analysis {
        let n = self.node_pin.len();
        let edge_delay = |e: Edge| match e {
            Edge::Net(k) => delay[k as usize],
            Edge::Cell(d) => d as f64,
        };
        let fanin =
            |v: usize| &self.fanin[self.fanin_off[v] as usize..self.fanin_off[v + 1] as usize];
        let mut out = Analysis {
            period: vec![0.0; self.clocks.len()],
            criticality: vec![0.0; self.conns.len()],
            slack: vec![f64::INFINITY; self.conns.len()],
            paths: vec![None; self.clocks.len()],
        };
        // Paths launched by each clock edge are propagated separately: a
        // path captured by the other edge has half a period.
        let launches: Vec<bool> = [false, true]
            .into_iter()
            .filter(|&neg| self.starts.iter().any(|s| s.3 == neg))
            .collect();
        let mut arrivals = Vec::with_capacity(launches.len());
        let mut worst_end: Vec<Option<(usize, u32)>> = vec![None; self.clocks.len()];
        for (li, &launch) in launches.iter().enumerate() {
            let mut arr = vec![f64::NEG_INFINITY; n];
            for &(s, d, _, neg) in &self.starts {
                if neg == launch {
                    arr[s as usize] = d;
                }
            }
            for &v in &self.order {
                let v = v as usize;
                for &(from, e) in fanin(v) {
                    let a = arr[from as usize] + edge_delay(e);
                    if a > arr[v] {
                        arr[v] = a;
                    }
                }
            }
            for &(e, setup, k, neg) in &self.ends {
                let t = arr[e as usize] + setup;
                let (k, t) = (k as usize, if neg == launch { t } else { 2.0 * t });
                if t.is_finite() && t > out.period[k] {
                    out.period[k] = t;
                    worst_end[k] = Some((li, e));
                }
            }
            arrivals.push(arr);
        }
        for (k, end) in worst_end.iter().enumerate() {
            if let Some((li, e)) = *end {
                out.paths[k] = Some(self.trace(&arrivals[li], e, &edge_delay));
            }
        }
        let overall = out.period.iter().copied().fold(1.0, f64::max);
        // A missed target judges paths by the clock's achieved period
        // instead: against the target every path would be equally critical.
        let target: Vec<f64> = self
            .clocks
            .iter()
            .zip(&out.period)
            .map(|(c, &p)| c.target_ps.map_or(overall, |t| t.max(p)).max(1.0))
            .collect();
        for (&launch, arr) in launches.iter().zip(&arrivals) {
            let mut req = vec![f64::INFINITY; n];
            let mut scale = vec![overall; n];
            for &(e, setup, k, neg) in &self.ends {
                let t = target[k as usize] * if neg == launch { 1.0 } else { 0.5 };
                let r = t - setup;
                if r < req[e as usize] {
                    req[e as usize] = r;
                    scale[e as usize] = t;
                }
            }
            for &v in self.order.iter().rev() {
                let v = v as usize;
                if !req[v].is_finite() {
                    continue;
                }
                for &(from, e) in fanin(v) {
                    let d = edge_delay(e);
                    let r = req[v] - d;
                    if r < req[from as usize] {
                        req[from as usize] = r;
                        scale[from as usize] = scale[v];
                    }
                    if let Edge::Net(k) = e
                        && arr[from as usize].is_finite()
                    {
                        let slack = req[v] - (arr[from as usize] + d);
                        let k = k as usize;
                        out.slack[k] = out.slack[k].min(slack);
                        let crit = (1.0 - slack / scale[v]).clamp(0.0, 1.0);
                        out.criticality[k] = out.criticality[k].max(crit);
                    }
                }
            }
        }
        out
    }

    /// The latest path into node `end`, from its start point.
    fn trace(
        &self,
        arr: &[f64],
        end: u32,
        edge_delay: &dyn Fn(Edge) -> f64,
    ) -> Vec<(CellId, usize, f64)> {
        let mut path = Vec::new();
        let mut cur = Some(end as usize);
        while let Some(v) = cur {
            let (c, i) = self.node_pin[v];
            path.push((c, i as usize, arr[v]));
            cur = self.fanin[self.fanin_off[v] as usize..self.fanin_off[v + 1] as usize]
                .iter()
                .filter(|(from, _)| arr[*from as usize].is_finite())
                .max_by(|a, b| {
                    (arr[a.0 as usize] + edge_delay(a.1))
                        .total_cmp(&(arr[b.0 as usize] + edge_delay(b.1)))
                })
                .map(|&(from, _)| from as usize);
        }
        path.reverse();
        path
    }
}

/// Routed delay from a net's source to each node of its route (`edges` in
/// tree order): each switch's delay, plus its fanout adder for each branch
/// the net takes from the switch's source wire.
pub fn routed_node_delays(arch: &Arch, source: u32, edges: &[u32]) -> FxHashMap<u32, f64> {
    let g = &arch.graph;
    let src = |e: u32| crate::route::edge_source(g, e);
    let mut branches: FxHashMap<u32, u32> = FxHashMap::default();
    for &e in edges {
        *branches.entry(src(e)).or_default() += 1;
    }
    let mut delay: FxHashMap<u32, f64> = FxHashMap::default();
    delay.insert(source, 0.0);
    for &e in edges {
        let s = src(e);
        let class = &g.classes[g.edge_class[e as usize] as usize];
        let d = delay.get(&s).copied().unwrap_or(0.0)
            + class.delay as f64
            + class.fanout as f64 * branches[&s] as f64;
        delay.insert(g.edge_dst[e as usize], d);
    }
    delay
}

/// The routed delay of every connection of `graph` (dedicated connections
/// and clocks on the global network take none).
pub fn routed_delays(
    arch: &Arch,
    nl: &Netlist,
    placement: &Placement,
    routing: &Routing,
    graph: &TimingGraph,
) -> Vec<f64> {
    let mut delays = vec![0.0; graph.conns.len()];
    let mut by_net: FxHashMap<NetId, Vec<usize>> = FxHashMap::default();
    for (k, c) in graph.conns.iter().enumerate() {
        if let Some(n) = nl.cells[c.sink].pins[c.pin].net
            && !dedicated(nl, placement, c)
        {
            by_net.entry(n).or_default().push(k);
        }
    }
    for (n, ks) in by_net {
        let Some(edges) = routing.nets.get(&n) else { continue };
        let Some(source) = routing.sources.get(&n).copied() else { continue };
        let d = routed_node_delays(arch, source, edges);
        for k in ks {
            let c = graph.conns[k];
            if let Some(node) = routing.sink_nodes.get(&(c.sink, c.pin as u32)) {
                match d.get(node) {
                    Some(&t) => delays[k] = t,
                    None if std::env::var_os("SYNTH_DEBUG").is_some() => eprintln!(
                        "debug: no route to {} of {}",
                        arch.graph.full_name(*node),
                        nl.str(nl.cells[c.sink].name)
                    ),
                    None => {}
                }
            }
        }
    }
    delays
}

/// Analyses a placed and routed design against the clock targets.
pub fn analyse(
    arch: &Arch,
    nl: &Netlist,
    placement: &Placement,
    routing: &Routing,
    clocks: &ClockConstraints,
) -> TimingReport {
    let graph = TimingGraph::new(arch, nl, clocks);
    let delays = routed_delays(arch, nl, placement, routing, &graph);
    let a = graph.analyse(&delays);
    let pin_name = |c: CellId, i: usize| nl.str(nl.cells[c].pins[i].name).to_string();
    let mut report = TimingReport::default();
    for (ci, clk) in graph.clocks.iter().enumerate() {
        let Some(path) = a.paths[ci].as_ref() else { continue };
        let ns = a.period[ci] / 1000.0;
        report.clocks.push(ClockTiming {
            clock: clk.name.clone(),
            period_ns: ns,
            fmax_mhz: 1000.0 / ns,
            target_mhz: clk.target_ps.map(|t| 1e6 / t),
            path: path
                .iter()
                .map(|&(c, i, t)| {
                    format!("{}.{} @{:.2}", nl.str(nl.cells[c].name), pin_name(c, i), t / 1000.0)
                })
                .collect(),
        });
    }
    report
}
