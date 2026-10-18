//! Static timing analysis with the IceStorm timing models (worst-case
//! corner): register-to-register paths per clock, against requested clock
//! frequencies. One timing graph serves placement and routing (with
//! estimated net delays) and the final analysis (with routed delays).
//! Clocks are ideal (no skew), as in `icetime`; a path between two clocks
//! is timed against the capturing clock.

use synth_core::FxHashMap;
use synth_netlist::{CellId, NetId, Netlist, Param};

use crate::arch::Arch;
use crate::chipdb::ArcKind;
use crate::pack::{GB, IO, LC, RAM};
use crate::place::Placement;
use crate::route::{Routing, sink_node};

/// Timing of one clock domain.
#[derive(Clone, Debug)]
pub struct ClockTiming {
    /// Name of the clock net.
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

/// Routed delay from each net's source to each of its nodes. Leaving a span
/// wire costs the delay of the span mux sized by the distance from where the
/// net enters the wire to where it leaves (as icetime computes it, per tap).
fn net_node_delays(arch: &Arch, routing: &Routing, net: NetId) -> FxHashMap<u32, f64> {
    let g = &arch.graph;
    let edges: &[u32] = routing.net_edges.get(&net).map_or(&[], |v| v.as_slice());
    let src_of = |e: u32| (g.offsets.partition_point(|&o| o as usize <= e as usize) - 1) as u32;
    // Span wires of the net: kind and where the net enters them.
    let mut entry: FxHashMap<u32, (u8, (usize, usize))> = FxHashMap::default();
    for &e in edges {
        let kind = g.edge_span[e as usize];
        if kind != 0 {
            entry.insert(g.edge_dst[e as usize], (kind, arch.pip_xy(g.edge_pip[e as usize])));
        }
    }
    let edge_delay = |e: u32| -> f64 {
        let mut d =
            if g.edge_span[e as usize] == 0 { g.edge_delay[e as usize] as f64 } else { 0.0 };
        if let Some(&(kind, (x, y))) = entry.get(&src_of(e)) {
            let (ex, ey) = arch.pip_xy(g.edge_pip[e as usize]);
            let horizontal = kind == crate::arch::SPAN4_H || kind == crate::arch::SPAN12_H;
            let far = if horizontal { ex.abs_diff(x) } else { ey.abs_diff(y) };
            d += arch.span_delay(kind, far) as f64;
        }
        d
    };
    let mut delay: FxHashMap<u32, f64> = FxHashMap::default();
    if let Some(src) = routing.net_source.get(&net) {
        delay.insert(src.raw(), 0.0);
    }
    // Edges are in tree order: each edge's source is reached earlier.
    for &e in edges {
        let d = delay.get(&src_of(e)).copied().unwrap_or(0.0) + edge_delay(e);
        delay.insert(g.edge_dst[e as usize], d);
    }
    delay
}

/// Analyses a placed and routed design against the clock targets.
pub fn analyse(
    arch: &Arch,
    nl: &Netlist,
    placement: &Placement,
    routing: &Routing,
    clocks: &ClockConstraints,
) -> TimingReport {
    let graph = TimingGraph::with_lut_pins(arch, nl, clocks, &routing.lut_pins);
    let pin_name = |c: CellId, i: usize| nl.str(nl.cells[c].pins[i].name).to_string();
    // Routed delay of every connection.
    let mut delays = vec![0.0; graph.conns.len()];
    let mut by_net: FxHashMap<NetId, Vec<usize>> = FxHashMap::default();
    for (k, c) in graph.conns.iter().enumerate() {
        if let Some(n) = nl.cells[c.sink].pins[c.pin].net {
            by_net.entry(n).or_default().push(k);
        }
    }
    for (n, ks) in by_net {
        let d = net_node_delays(arch, routing, n);
        for k in ks {
            let c = graph.conns[k];
            if let Ok(node) = sink_node(arch, nl, placement, c.sink, &pin_name(c.sink, c.pin)) {
                delays[k] = d.get(&node.raw()).copied().unwrap_or(0.0);
            }
        }
    }
    let a = graph.analyse(&delays);
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
                    // The routing node too, to compare with icetime's
                    // `net_<id>` (chipdb net ids are node ids).
                    let pin = pin_name(c, i);
                    let node = sink_node(arch, nl, placement, c, &pin)
                        .map(|n| format!(" ({} net_{})", arch.node_name(n), n.raw()))
                        .unwrap_or_default();
                    format!("{}.{pin} @{:.2}{node}", nl.str(nl.cells[c].name), t / 1000.0)
                })
                .collect(),
        });
    }
    report
}

/// Global clock distribution jitter added to every launch, as icetime does
/// (`GLOBAL_CLK_DIST_JITTER`).
const CLOCK_JITTER_PS: f64 = 100.0;

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

/// Clock targets: frequencies (MHz) by clock net name, and one for the
/// clocks not named. Clocks without a target are analysed against their own
/// worst path.
#[derive(Clone, Debug, Default)]
pub struct ClockConstraints {
    pub default_mhz: Option<f64>,
    pub nets: Vec<(String, f64)>,
}

impl ClockConstraints {
    /// The target period (ps) of a clock known by any of `names`.
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
    /// What the clock comes from: the input port or the register driving
    /// it (through global buffers).
    pub name: String,
    pub target_ps: Option<f64>,
}

/// The name of the port or register a clock net comes from.
fn clock_name(nl: &Netlist, net: NetId) -> String {
    let mut n = net;
    for _ in 0..4 {
        let Some(d) = nl.nets[n].driver else { break };
        match nl.cell_kind(d.cell) {
            GB => match nl.pin_net(d.cell, "USER_SIGNAL_TO_GLOBAL_BUFFER") {
                Some(i) => n = i,
                None => break,
            },
            IO => match crate::place::port_name(nl, d.cell) {
                Some(p) => return p,
                None => break,
            },
            LC => return nl.str(nl.cells[d.cell].name).trim_end_matches("$lc").to_string(),
            _ => break,
        }
    }
    nl.str(nl.nets[n].name).to_string()
}

#[derive(Clone, Copy, Debug)]
enum Edge {
    /// A net connection (index into `conns`), whose delay the caller gives.
    Net(u32),
    /// A combinational arc through a cell.
    Cell(f32),
}

/// The timing graph of a netlist, shared by placement, routing and
/// analysis: one node per cell pin, edges for net connections and for the
/// combinational arcs of cells, register outputs as start points and
/// register inputs as end points, per clock. Net delays are supplied per
/// analysis (estimated from positions, or routed).
#[derive(Debug)]
pub struct TimingGraph {
    /// First node of each cell's pins (cells by index).
    base: Vec<u32>,
    /// Cell and pin of each node.
    node_pin: Vec<(CellId, u16)>,
    /// Fanin edges of each node: `fanin[fanin_off[n]..fanin_off[n + 1]]`.
    fanin_off: Vec<u32>,
    fanin: Vec<(u32, Edge)>,
    /// Nodes in topological order (nodes on combinational loops left out).
    order: Vec<u32>,
    pub conns: Vec<Connection>,
    /// Connection ending at each node (`u32::MAX` if none).
    sink_conn: Vec<u32>,
    /// Start points: node, clock-to-output delay, clock.
    starts: Vec<(u32, f64, u16)>,
    /// End points: node, setup time, clock.
    ends: Vec<(u32, f64, u16)>,
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
    /// Least slack of each connection against the same targets (ps;
    /// infinite off timed paths).
    pub slack: Vec<f64>,
    /// Critical path into each clock: cell, pin and arrival (ps).
    pub paths: Vec<Option<Vec<(CellId, usize, f64)>>>,
}

impl TimingGraph {
    pub fn new(arch: &Arch, nl: &Netlist, constraints: &ClockConstraints) -> Self {
        Self::with_lut_pins(arch, nl, constraints, &FxHashMap::default())
    }

    /// Like [`TimingGraph::new`], with the physical LUT input each logical
    /// input of a routed logic cell arrives on (see `Routing::lut_pins`):
    /// the LUT delay depends on the physical input.
    pub fn with_lut_pins(
        arch: &Arch,
        nl: &Netlist,
        constraints: &ClockConstraints,
        lut_pins: &FxHashMap<CellId, [u8; 4]>,
    ) -> Self {
        let arcs = Arcs { arch, cache: Default::default() };
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
        let param = |c: CellId, p: &str| nl.param(c, p).and_then(Param::as_u64).unwrap_or(0);
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
        // Clock domains, start and end points, cell arcs.
        let mut clocks: Vec<Clock> = Vec::new();
        let clock_index = |net: NetId, clocks: &mut Vec<Clock>| -> u16 {
            if let Some(k) = clocks.iter().position(|c| c.net == net) {
                return k as u16;
            }
            let name = clock_name(nl, net);
            let target_ps = constraints.target_ps(&[&name, nl.str(nl.nets[net].name)]);
            clocks.push(Clock { net, name, target_ps });
            (clocks.len() - 1) as u16
        };
        let (mut starts, mut ends) = (Vec::new(), Vec::new());
        let lc_arc =
            |from: &str, to: &str| arcs.get("LogicCell40", ArcKind::IoPath, from, to) as f32;
        for c in nl.live_cells() {
            let kind = nl.cell_kind(c);
            let mut arc = |from: Option<usize>, to: Option<usize>, d: f32| {
                if let (Some(f), Some(t)) = (from, to) {
                    edges[node(c, t) as usize].push((node(c, f), Edge::Cell(d)));
                }
            };
            match kind {
                LC => {
                    let o = pin_of(c, "O");
                    let cout = pin_of(c, "COUT");
                    let cin = pin_of(c, "CIN");
                    let i3_is_cin = cin.is_some() && nl.pin_net(c, "I3") == nl.pin_net(c, "CIN");
                    if param(c, "DFF_ENABLE") != 0 {
                        if let Some(clk) = nl.pin_net(c, "CLK") {
                            let k = clock_index(clk, &mut clocks);
                            if let Some(o) = o {
                                starts.push((node(c, o), lc_arc("clk", "lcout") as f64, k));
                            }
                            let perm = lut_pins.get(&c).copied().unwrap_or([0, 1, 2, 3]);
                            for (j, p) in
                                ["I0", "I1", "I2", "I3", "CEN", "SR"].into_iter().enumerate()
                            {
                                let phys = match j {
                                    0..4 => ["I0", "I1", "I2", "I3"][perm[j] as usize],
                                    _ => p,
                                };
                                if let Some(i) = pin_of(c, p)
                                    && let Some(s) = setup_time(&arcs, LC, phys)
                                {
                                    ends.push((node(c, i), s, k));
                                }
                            }
                            if i3_is_cin && let Some(s) = setup_time(&arcs, LC, "I3") {
                                ends.push((node(c, cin.unwrap()), s, k));
                            }
                        }
                    } else {
                        const INS: [&str; 4] = ["in0", "in1", "in2", "in3"];
                        let perm = lut_pins.get(&c).copied().unwrap_or([0, 1, 2, 3]);
                        for (k, p) in ["I0", "I1", "I2", "I3"].into_iter().enumerate() {
                            arc(pin_of(c, p), o, lc_arc(INS[perm[k] as usize], "lcout"));
                        }
                        if i3_is_cin {
                            arc(cin, o, lc_arc("in3", "lcout"));
                        }
                    }
                    for (p, a) in [("I1", "in1"), ("I2", "in2"), ("CIN", "carryin")] {
                        arc(pin_of(c, p), cout, lc_arc(a, "carryout"));
                    }
                }
                RAM => {
                    let rclk = nl.pin_net(c, "RCLK").map(|n| clock_index(n, &mut clocks));
                    let wclk = nl.pin_net(c, "WCLK").map(|n| clock_index(n, &mut clocks));
                    for (i, pin) in nl.cells[c].pins.iter().enumerate() {
                        if pin.net.is_none() {
                            continue;
                        }
                        let name = nl.str(pin.name);
                        if name.starts_with("RDATA") {
                            if let Some(k) = rclk {
                                let d = arcs.get("SB_RAM40_4K", ArcKind::IoPath, "RCLK", name);
                                starts.push((node(c, i), d, k));
                            }
                        } else if pin.dir == synth_netlist::Dir::Input
                            && let Some(s) = setup_time(&arcs, RAM, name)
                            && let Some(k) = if name.starts_with('R') { rclk } else { wclk }
                        {
                            ends.push((node(c, i), s, k));
                        }
                    }
                }
                GB => {
                    let d = arcs.get(
                        "ICE_GB",
                        ArcKind::IoPath,
                        "USERSIGNALTOGLOBALBUFFER",
                        "GLOBALBUFFEROUTPUT",
                    );
                    arc(
                        pin_of(c, "USER_SIGNAL_TO_GLOBAL_BUFFER"),
                        pin_of(c, "GLOBAL_BUFFER_OUTPUT"),
                        d as f32,
                    );
                }
                _ => {} // inputs from outside are not register paths
            }
        }
        // Register outputs start paths; nothing propagates into them.
        for &(s, _, _) in &starts {
            edges[s as usize].clear();
        }
        let mut fanin_off = Vec::with_capacity(n + 1);
        let mut fanin = Vec::new();
        fanin_off.push(0);
        for e in &edges {
            fanin.extend_from_slice(e);
            fanin_off.push(fanin.len() as u32);
        }
        // Kahn's algorithm over fanin counts.
        let mut fanouts: Vec<Vec<u32>> = vec![Vec::new(); n];
        let mut pending = vec![0u32; n];
        for (to, e) in edges.iter().enumerate() {
            pending[to] = e.len() as u32;
            for &(from, _) in e {
                fanouts[from as usize].push(to as u32);
            }
        }
        let mut order: Vec<u32> = (0..n as u32).filter(|&v| pending[v as usize] == 0).collect();
        let mut head = 0;
        while head < order.len() {
            let v = order[head] as usize;
            head += 1;
            for &w in &fanouts[v] {
                pending[w as usize] -= 1;
                if pending[w as usize] == 0 {
                    order.push(w);
                }
            }
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
    /// indexed like `conns`). Paths start at every register and are timed
    /// against the target of the clock that captures them (clocks without
    /// a target take the worst path of the design as theirs, so related
    /// clocks such as a divided clock are timed together).
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
        let mut arr = vec![f64::NEG_INFINITY; n];
        for &(s, d, _) in &self.starts {
            arr[s as usize] = d + CLOCK_JITTER_PS;
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
        let mut worst_end: Vec<Option<u32>> = vec![None; self.clocks.len()];
        for &(e, setup, k) in &self.ends {
            let (k, t) = (k as usize, arr[e as usize] + setup);
            if t.is_finite() && t > out.period[k] {
                out.period[k] = t;
                worst_end[k] = Some(e);
            }
        }
        for (k, end) in worst_end.iter().enumerate() {
            if let Some(e) = *end {
                out.paths[k] = Some(self.trace(&arr, e, &edge_delay));
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
        // Required times, and the target each node's paths are judged by.
        let mut req = vec![f64::INFINITY; n];
        let mut scale = vec![overall; n];
        for &(e, setup, k) in &self.ends {
            let r = target[k as usize] - setup;
            if r < req[e as usize] {
                req[e as usize] = r;
                scale[e as usize] = target[k as usize];
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

#[cfg(test)]
mod tests {
    use super::*;
    use synth_netlist::Dir;

    /// `q0 -> LUT -> q1`: flip-flop cells around a LUT, one clock.
    fn chain(nl: &mut Netlist) {
        let clk = nl.add_net("clk");
        let (a, b) = (nl.add_net("a"), nl.add_net("b"));
        let q1 = nl.add_net("q1");
        let lc = |nl: &mut Netlist, name: &str, dff: bool, i: (&str, NetId), o: NetId| {
            let c = nl.add_cell(name, LC);
            nl.add_pin(c, i.0, Dir::Input, Some(i.1)).unwrap();
            nl.add_pin(c, "O", Dir::Output, Some(o)).unwrap();
            if dff {
                nl.add_pin(c, "CLK", Dir::Input, Some(clk)).unwrap();
                nl.set_param(c, "DFF_ENABLE", Param::Int(1));
            }
        };
        lc(nl, "ff0", true, ("I0", q1), a);
        lc(nl, "lut", false, ("I0", a), b);
        lc(nl, "ff1", true, ("I1", b), q1);
    }

    #[test]
    fn register_to_register_path() {
        let arch = Arch::new(crate::arch::Part::Hx1k, "vq100").unwrap();
        let arcs = Arcs { arch: &arch, cache: Default::default() };
        let lc = |from: &str, to: &str| arcs.get("LogicCell40", ArcKind::IoPath, from, to);
        let mut nl = Netlist::new();
        chain(&mut nl);
        let g = TimingGraph::new(&arch, &nl, &ClockConstraints::default());
        assert_eq!(g.clocks.len(), 1);
        let delays: Vec<f64> = g
            .conns
            .iter()
            .map(|c| if nl.str(nl.cells[c.sink].name) == "lut" { 300.0 } else { 500.0 })
            .collect();
        let a = g.analyse(&delays);
        let setup = setup_time(&arcs, LC, "I1").unwrap();
        // The path into ff1 is the longest (into ff0 there is no logic).
        let want =
            CLOCK_JITTER_PS + lc("clk", "lcout") + 300.0 + lc("in0", "lcout") + 500.0 + setup;
        assert!((a.period[0] - want).abs() < 1e-6, "{} vs {want}", a.period[0]);
        let into_ff1 = g.conns.iter().position(|c| nl.str(nl.cells[c.sink].name) == "ff1").unwrap();
        assert!(a.slack[into_ff1].abs() < 1e-6);
        assert!((a.criticality[into_ff1] - 1.0).abs() < 1e-9);
        // Against a looser target the path has slack.
        let loose = ClockConstraints { default_mhz: Some(1e6 / (want + 1000.0)), nets: Vec::new() };
        let a = TimingGraph::new(&arch, &nl, &loose).analyse(&delays);
        assert!((a.slack[into_ff1] - 1000.0).abs() < 1e-6);
        assert!(a.criticality[into_ff1] < 1.0);
        // A target that cannot be met judges paths as no target does.
        let tight = ClockConstraints { default_mhz: Some(1e6 / (want / 2.0)), nets: Vec::new() };
        let a_tight = TimingGraph::new(&arch, &nl, &tight).analyse(&delays);
        let a_none = g.analyse(&delays);
        assert_eq!(a_tight.criticality, a_none.criticality);
    }
}
