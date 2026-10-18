//! Designs as graphs for the schematic views: word-level RTL, the gate level
//! (AIG with flip-flops and carry chains) and netlists of primitives.
//!
//! A graph is `{nodes, edges}`. Nodes carry a kind (`k`), a type (`t`), a
//! short `label`, the full `name`, the instance `path` they belong to, input
//! and output ports as `[name, width]` pairs and `info` as `[key, value]`
//! pairs. Edges connect output port `sp` of node `s` to input port `dp` of
//! node `d`, with a width `w`, a signal name `n` and `inv` for complemented
//! AIG edges.

use std::collections::HashMap;

use synth_core::{Json, Logic};
use synth_logic::LogicNet;
use synth_logic::aig::{Lit, Node};
use synth_logic::net::Bit;
use synth_netlist::{Dir, Netlist};
use synth_rtl::{BinaryOp, CellKind, Module, PortDir, Sig, SigBit, UnaryOp, WireId};

/// The instance path of a hierarchical name (`a.b.x` → `a.b`).
pub fn path_of(name: &str) -> &str {
    let base = name.split('[').next().unwrap_or(name);
    base.rsplit_once('.').map_or("", |(p, _)| p)
}

#[derive(Default)]
struct Graph {
    nodes: Vec<Json>,
    edges: Vec<Json>,
}

impl Graph {
    #[allow(clippy::too_many_arguments)]
    fn node(
        &mut self,
        k: &str,
        t: &str,
        label: &str,
        name: &str,
        path: &str,
        ins: Vec<(String, u32)>,
        outs: Vec<(String, u32)>,
        info: Vec<(String, String)>,
    ) -> u32 {
        let id = self.nodes.len() as u32;
        let ports = |v: Vec<(String, u32)>| {
            Json::Arr(v.into_iter().map(|(n, w)| Json::Arr(vec![n.into(), w.into()])).collect())
        };
        let mut j = Json::obj()
            .with("k", k)
            .with("t", t)
            .with("label", label)
            .with("name", name)
            .with("path", path)
            .with("ins", ports(ins))
            .with("outs", ports(outs));
        if !info.is_empty() {
            j.set(
                "info",
                Json::Arr(
                    info.into_iter().map(|(a, b)| Json::Arr(vec![a.into(), b.into()])).collect(),
                ),
            );
        }
        self.nodes.push(j);
        id
    }

    #[allow(clippy::too_many_arguments)]
    fn edge(&mut self, s: u32, sp: usize, d: u32, dp: usize, w: u32, n: &str, inv: bool) {
        let mut e =
            Json::obj().with("s", s).with("sp", sp).with("d", d).with("dp", dp).with("w", w);
        if !n.is_empty() {
            e.set("n", n);
        }
        if inv {
            e.set("inv", true);
        }
        self.edges.push(e);
    }

    fn done(self) -> Json {
        Json::obj().with("nodes", self.nodes).with("edges", self.edges)
    }
}

// ----- word-level RTL -----

#[derive(Copy, Clone, PartialEq)]
enum Src {
    Node(u32, u16, u32),
    Const(Logic),
    None,
}

fn unary_label(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Not => "~",
        UnaryOp::Neg => "-",
        UnaryOp::ReduceAnd => "&",
        UnaryOp::ReduceOr => "|",
        UnaryOp::ReduceXor => "^",
        UnaryOp::ReduceXnor => "~^",
        UnaryOp::LogicNot => "!",
    }
}

fn binary_label(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        And => "&",
        Or => "|",
        Xor => "^",
        Xnor => "~^",
        Add => "+",
        Sub => "−",
        Mul => "×",
        Div => "÷",
        Mod => "%",
        Shl => "<<",
        Shr => ">>",
        Sshr => ">>>",
        Eq => "==",
        Ne => "!=",
        Eqx => "===",
        Nex => "!==",
        Lt => "<",
        Le => "≤",
        Gt => ">",
        Ge => "≥",
        LogicAnd => "&&",
        LogicOr => "||",
    }
}

/// A constant, in decimal when it is fully defined and fits.
fn logic_str(bits: &[Logic]) -> String {
    let defined = bits.iter().all(|b| matches!(b, Logic::Zero | Logic::One));
    if defined && bits.len() <= 64 {
        let v = bits.iter().rev().fold(0u64, |acc, b| acc << 1 | (*b == Logic::One) as u64);
        return format!("{}'d{v}", bits.len());
    }
    let mut s = format!("{}'b", bits.len());
    s.extend(bits.iter().rev().map(|b| b.to_char()));
    s
}

/// Text of a wire range.
fn wire_range(m: &Module, w: WireId, lo: u32, hi: u32) -> String {
    let wire = &m.wires[w];
    if lo == 0 && hi + 1 == wire.width {
        wire.name.clone()
    } else if lo == hi {
        format!("{}[{lo}]", wire.name)
    } else {
        format!("{}[{hi}:{lo}]", wire.name)
    }
}

struct Rtl<'m> {
    m: &'m Module,
    g: Graph,
    drv: HashMap<(WireId, u32), (u32, u16, u32)>,
    alias: HashMap<(WireId, u32), SigBit>,
}

impl Rtl<'_> {
    fn src(&self, mut b: SigBit) -> Src {
        for _ in 0..256 {
            match b {
                SigBit::Const(l) => return Src::Const(l),
                SigBit::Wire(w, i) => {
                    if let Some(&(n, p, k)) = self.drv.get(&(w, i)) {
                        return Src::Node(n, p, k);
                    }
                    match self.alias.get(&(w, i)) {
                        Some(&next) => b = next,
                        None => return Src::None,
                    }
                }
            }
        }
        Src::None
    }

    fn drive(&mut self, node: u32, port: u16, sig: &Sig) {
        for (k, b) in sig.iter().enumerate() {
            if let SigBit::Wire(w, i) = *b {
                self.drv.insert((w, i), (node, port, k as u32));
            }
        }
    }

    /// Connects every bit of `sig` to input `dp` of `d`.
    fn sink(&mut self, d: u32, dp: usize, sig: &Sig) {
        let bits: Vec<SigBit> = sig.iter().copied().collect();
        let srcs: Vec<Src> = bits.iter().map(|&b| self.src(b)).collect();
        let mut consts: Vec<Logic> = Vec::new();
        let mut i = 0;
        while i < srcs.len() {
            match srcs[i] {
                Src::Node(n, p, k) => {
                    let mut j = i + 1;
                    while j < srcs.len() && srcs[j] == Src::Node(n, p, k + (j - i) as u32) {
                        j += 1;
                    }
                    let label = self.label(&bits[i..j]);
                    self.g.edge(n, p as usize, d, dp, (j - i) as u32, &label, false);
                    i = j;
                }
                Src::Const(l) => {
                    consts.push(l);
                    i += 1;
                }
                Src::None => i += 1,
            }
        }
        if !consts.is_empty() {
            let v = if consts.len() == srcs.len() {
                logic_str(&consts)
            } else {
                format!("{} const bits", consts.len())
            };
            let c = self.g.node(
                "const",
                "Const",
                &v,
                &v,
                "",
                vec![],
                vec![(String::new(), consts.len() as u32)],
                vec![],
            );
            self.g.edge(c, 0, d, dp, consts.len() as u32, "", false);
        }
    }

    /// The name of a run of sink bits (a public wire when there is one).
    fn label(&self, bits: &[SigBit]) -> String {
        let SigBit::Wire(w, lo) = bits[0] else { return String::new() };
        let contiguous = bits.iter().enumerate().all(|(k, b)| *b == SigBit::Wire(w, lo + k as u32));
        if self.m.wires[w].public && contiguous {
            return wire_range(self.m, w, lo, lo + bits.len() as u32 - 1);
        }
        // The driving wire's name.
        let mut b = bits[0];
        for _ in 0..64 {
            let SigBit::Wire(w, i) = b else { break };
            if self.m.wires[w].public {
                return wire_range(self.m, w, i, i + bits.len() as u32 - 1);
            }
            match self.alias.get(&(w, i)) {
                Some(&n) => b = n,
                None => break,
            }
        }
        String::new()
    }
}

/// What the graph shows of an RTL cell: kind, type, label, inputs (name,
/// width), outputs (name, signal), properties, and the input signals.
type CellParts<'a> = (
    &'a str,
    String,
    String,
    Vec<(&'a str, u32)>,
    Vec<(&'a str, &'a Sig)>,
    Vec<(String, String)>,
    Vec<Sig>,
);

/// A word-level RTL module as a graph.
pub fn rtl_graph(m: &Module) -> Json {
    let mut r = Rtl { m, g: Graph::default(), drv: HashMap::new(), alias: HashMap::new() };
    for (lhs, rhs) in &m.connections {
        for (a, b) in lhs.iter().zip(rhs.iter()) {
            if let SigBit::Wire(w, i) = *a {
                r.alias.insert((w, i), *b);
            }
        }
    }
    // Pass 1: nodes and what they drive.
    let mut outs_ports: Vec<(u32, WireId)> = Vec::new();
    for w in m.ports() {
        let wire = &m.wires[w];
        let (dir, _) = wire.port.unwrap();
        let sig = m.wire_sig(w);
        if dir != PortDir::Output {
            let n = r.g.node(
                "in",
                "Input",
                &wire.name,
                &wire.name,
                "",
                vec![],
                vec![(String::new(), wire.width)],
                vec![],
            );
            r.drive(n, 0, &sig);
        }
        if dir != PortDir::Input {
            let n = r.g.node(
                "out",
                "Output",
                &wire.name,
                &wire.name,
                "",
                vec![(String::new(), wire.width)],
                vec![],
                vec![],
            );
            outs_ports.push((n, w));
        }
    }
    let mut sinks: Vec<(u32, Vec<Sig>)> = Vec::new();
    for c in m.live_cells() {
        let cell = &m.cells[c];
        let path = path_of(&cell.name);
        let w = |s: &Sig| s.width() as u32;
        let (k, t, label, ins, outs, info, inputs): CellParts = match &cell.kind {
            CellKind::Unary { op, a, y } => (
                "op",
                format!("{op:?}"),
                unary_label(*op).into(),
                vec![("a", w(a))],
                vec![("y", y)],
                vec![],
                vec![a.clone()],
            ),
            CellKind::Binary { op, signed, a, b, y } => {
                let info =
                    if *signed { vec![("signed".to_string(), "yes".to_string())] } else { vec![] };
                (
                    "op",
                    format!("{op:?}"),
                    binary_label(*op).into(),
                    vec![("a", w(a)), ("b", w(b))],
                    vec![("y", y)],
                    info,
                    vec![a.clone(), b.clone()],
                )
            }
            CellKind::Mux { a, b, s, y, .. } => (
                "mux",
                "Mux".into(),
                "mux".into(),
                vec![("0", w(a)), ("1", w(b)), ("s", 1)],
                vec![("y", y)],
                vec![],
                vec![a.clone(), b.clone(), Sig::bit(*s)],
            ),
            CellKind::Pmux { default, cases, s, y } => {
                let mut ins = vec![("default", w(default))];
                let mut inputs = vec![default.clone()];
                for c in cases {
                    ins.push(("case", w(c)));
                    inputs.push(c.clone());
                }
                ins.push(("s", w(s)));
                inputs.push(s.clone());
                let info = vec![("cases".to_string(), cases.len().to_string())];
                ("pmux", "Pmux".into(), "pmux".into(), ins, vec![("y", y)], info, inputs)
            }
            CellKind::Shiftx { a, b, offset, y, .. } => {
                let info = vec![("offset".to_string(), offset.to_string())];
                (
                    "shiftx",
                    "Shiftx".into(),
                    "[ ]".into(),
                    vec![("a", w(a)), ("b", w(b))],
                    vec![("y", y)],
                    info,
                    vec![a.clone(), b.clone()],
                )
            }
            CellKind::Dff(d) => {
                let mut ins = vec![("d", w(&d.d)), ("clk", 1)];
                let mut inputs = vec![d.d.clone(), Sig::bit(d.clk.sig)];
                let mut info = vec![
                    (
                        "edge".to_string(),
                        if d.clk.posedge { "rising" } else { "falling" }.to_string(),
                    ),
                    ("init".to_string(), d.init.to_string()),
                ];
                if let Some(en) = d.en {
                    ins.push(("en", 1));
                    inputs.push(Sig::bit(en.sig));
                    if !en.pol {
                        info.push(("enable".into(), "active low".into()));
                    }
                }
                if let Some((c, v)) = &d.srst {
                    ins.push(("srst", 1));
                    inputs.push(Sig::bit(c.sig));
                    info.push((
                        "sync reset".into(),
                        format!("{v}{}", if c.pol { "" } else { " (active low)" }),
                    ));
                }
                if let Some((c, v)) = &d.arst {
                    ins.push(("arst", 1));
                    inputs.push(Sig::bit(c.sig));
                    info.push((
                        "async reset".into(),
                        format!("{v}{}", if c.pol { "" } else { " (active low)" }),
                    ));
                }
                ("dff", "Dff".into(), "D Q".into(), ins, vec![("q", &d.q)], info, inputs)
            }
            CellKind::Latch(l) => (
                "latch",
                "Latch".into(),
                "latch".into(),
                vec![("d", w(&l.d)), ("en", 1)],
                vec![("q", &l.q)],
                vec![],
                vec![l.d.clone(), Sig::bit(l.en.sig)],
            ),
            CellKind::Instance(inst) => {
                let mut ins = Vec::new();
                let mut outs = Vec::new();
                let mut inputs = Vec::new();
                for (n, d, s) in &inst.conns {
                    if *d == PortDir::Output {
                        outs.push((n.as_str(), s));
                    } else {
                        ins.push((n.as_str(), w(s)));
                        inputs.push(s.clone());
                    }
                }
                let info = inst
                    .params
                    .iter()
                    .map(|(k, v)| {
                        let v = match v {
                            synth_rtl::Attr::Const(c) => c.to_string(),
                            synth_rtl::Attr::Str(s) => format!("{s:?}"),
                        };
                        (k.clone(), v)
                    })
                    .collect();
                ("inst", inst.module.clone(), inst.module.clone(), ins, outs, info, inputs)
            }
        };
        let n = r.g.node(
            k,
            &t,
            &label,
            &cell.name,
            path,
            ins.into_iter().map(|(a, b)| (a.to_string(), b)).collect(),
            outs.iter().map(|(a, s)| (a.to_string(), s.width() as u32)).collect(),
            info,
        );
        for (p, (_, s)) in outs.iter().enumerate() {
            r.drive(n, p as u16, s);
        }
        sinks.push((n, inputs));
    }
    for (_, mem) in m.memories.iter_enumerated() {
        if !mem.alive {
            continue;
        }
        let mut ins = Vec::new();
        let mut inputs = Vec::new();
        for (i, wp) in mem.writes.iter().enumerate() {
            ins.push((format!("w{i}.clk"), 1));
            inputs.push(Sig::bit(wp.clk.sig));
            ins.push((format!("w{i}.en"), wp.en.width() as u32));
            inputs.push(wp.en.clone());
            ins.push((format!("w{i}.addr"), wp.addr.width() as u32));
            inputs.push(wp.addr.clone());
            ins.push((format!("w{i}.data"), wp.data.width() as u32));
            inputs.push(wp.data.clone());
        }
        let mut outs = Vec::new();
        for (i, rp) in mem.reads.iter().enumerate() {
            if let Some(c) = rp.clk {
                ins.push((format!("r{i}.clk"), 1));
                inputs.push(Sig::bit(c.sig));
            }
            ins.push((format!("r{i}.en"), 1));
            inputs.push(Sig::bit(rp.en));
            ins.push((format!("r{i}.addr"), rp.addr.width() as u32));
            inputs.push(rp.addr.clone());
            outs.push((format!("r{i}.data"), rp.data.width() as u32));
        }
        let info = vec![
            ("size".to_string(), format!("{} × {} bits", mem.depth, mem.width)),
            ("read ports".to_string(), mem.reads.len().to_string()),
            ("write ports".to_string(), mem.writes.len().to_string()),
        ];
        let n =
            r.g.node("mem", "Memory", &mem.name, &mem.name, path_of(&mem.name), ins, outs, info);
        for (i, rp) in mem.reads.iter().enumerate() {
            r.drive(n, i as u16, &rp.data);
        }
        sinks.push((n, inputs));
    }
    // Pass 2: edges.
    for (n, inputs) in sinks {
        for (p, s) in inputs.iter().enumerate() {
            r.sink(n, p, s);
        }
    }
    for (n, w) in outs_ports {
        let s = m.wire_sig(w);
        // The port wire's own driver, not the port node itself.
        r.sink(n, 0, &s);
    }
    r.g.done()
}

// ----- gate level -----

/// A logic network (AIG, flip-flops, carry chains, instances) as a graph.
pub fn aig_graph(net: &LogicNet) -> Json {
    let mut g = Graph::default();
    let aig = &net.aig;
    // Owner of each AIG input (source) and output (sink).
    let mut src_of: HashMap<u32, (u32, u16)> = HashMap::new();
    let mut sinks: Vec<(usize, u32, usize, String)> = Vec::new();
    let mut sink_path: HashMap<usize, String> = HashMap::new();
    for p in &net.ports {
        let path = "";
        for (i, b) in p.bits.iter().enumerate() {
            let label = if p.bits.len() == 1 { p.name.clone() } else { format!("{}[{i}]", p.name) };
            match b {
                Bit::Src(s) => {
                    let n = g.node(
                        "in",
                        "Input",
                        &label,
                        &label,
                        path,
                        vec![],
                        vec![(String::new(), 1)],
                        vec![],
                    );
                    src_of.insert(*s, (n, 0));
                }
                Bit::Snk(k) => {
                    let n = g.node(
                        "out",
                        "Output",
                        &label,
                        &label,
                        path,
                        vec![(String::new(), 1)],
                        vec![],
                        vec![],
                    );
                    sinks.push((*k, n, 0, label.clone()));
                    sink_path.insert(*k, String::new());
                }
                Bit::Pad(_) => {}
            }
        }
    }
    for ff in &net.ffs {
        let path = path_of(&ff.name).to_string();
        let mut ins = vec![("d".to_string(), 1), ("clk".to_string(), 1)];
        let mut ks = vec![ff.d, ff.clk];
        if let Some(e) = ff.en {
            ins.push(("en".into(), 1));
            ks.push(e);
        }
        if let Some((s, _)) = ff.srst {
            ins.push(("srst".into(), 1));
            ks.push(s);
        }
        if let Some((s, _)) = ff.arst {
            ins.push(("arst".into(), 1));
            ks.push(s);
        }
        let mut info = vec![
            ("edge".to_string(), if ff.posedge { "rising" } else { "falling" }.to_string()),
            ("init".to_string(), ff.init.to_char().to_string()),
        ];
        if let Some((_, v)) = ff.srst {
            info.push(("sync reset to".into(), (v as u8).to_string()));
        }
        if let Some((_, v)) = ff.arst {
            info.push(("async reset to".into(), (v as u8).to_string()));
        }
        let n = g.node("ff", "FF", "D Q", &ff.name, &path, ins, vec![("q".into(), 1)], info);
        src_of.insert(ff.q, (n, 0));
        for (p, k) in ks.into_iter().enumerate() {
            sinks.push((k, n, p, String::new()));
            sink_path.insert(k, path.clone());
        }
    }
    for (ci, c) in net.carries.iter().enumerate() {
        let w = c.sum.len();
        let mut ins = Vec::new();
        let mut ks = Vec::new();
        for i in 0..w {
            ins.push((format!("a{i}"), 1));
            ks.push(c.a[i]);
        }
        for i in 0..w {
            ins.push((format!("b{i}"), 1));
            ks.push(c.b[i]);
        }
        ins.push(("cin".into(), 1));
        ks.push(c.cin);
        let mut outs: Vec<(String, u32)> = (0..w).map(|i| (format!("s{i}"), 1)).collect();
        if c.cout.is_some() {
            outs.push(("cout".into(), 1));
        }
        let name = format!("carry{ci}");
        let info = vec![("width".to_string(), w.to_string())];
        let n = g.node("carry", "Carry", "+", &name, "", ins, outs, info);
        for (i, s) in c.sum.iter().enumerate() {
            src_of.insert(*s, (n, i as u16));
        }
        if let Some(co) = c.cout {
            src_of.insert(co, (n, w as u16));
        }
        for (p, k) in ks.into_iter().enumerate() {
            sinks.push((k, n, p, String::new()));
        }
    }
    for inst in &net.instances {
        let path = path_of(&inst.name).to_string();
        let mut ins = Vec::new();
        let mut outs = Vec::new();
        let mut in_bits = Vec::new();
        for (name, _, bits) in &inst.conns {
            for (i, b) in bits.iter().enumerate() {
                let pn = if bits.len() == 1 { name.clone() } else { format!("{name}[{i}]") };
                match b {
                    Bit::Src(s) => {
                        outs.push((pn, 1, *s));
                    }
                    Bit::Snk(k) => {
                        ins.push((pn, 1));
                        in_bits.push(*k);
                    }
                    Bit::Pad(_) => {}
                }
            }
        }
        let n = g.node(
            "inst",
            &inst.module,
            &inst.module,
            &inst.name,
            &path,
            ins,
            outs.iter().map(|(n, w, _)| (n.clone(), *w)).collect(),
            vec![],
        );
        for (p, (_, _, s)) in outs.iter().enumerate() {
            src_of.insert(*s, (n, p as u16));
        }
        for (p, k) in in_bits.into_iter().enumerate() {
            sinks.push((k, n, p, String::new()));
            sink_path.insert(k, path.clone());
        }
    }
    // AND nodes, with the path of a sink they feed.
    let mut and_path: Vec<Option<String>> = vec![None; aig.nodes.len()];
    for (k, _, _, _) in &sinks {
        let path = sink_path.get(k).cloned().unwrap_or_default();
        let mut stack = vec![aig.outputs[*k].node()];
        while let Some(n) = stack.pop() {
            if let Node::And(a, b) = aig.nodes[n as usize]
                && and_path[n as usize].is_none()
            {
                and_path[n as usize] = Some(path.clone());
                stack.push(a.node());
                stack.push(b.node());
            }
        }
    }
    let mut node_of: Vec<u32> = vec![u32::MAX; aig.nodes.len()];
    let mut const_node = None;
    for (i, n) in aig.nodes.iter().enumerate() {
        if let Node::And(..) = n {
            let Some(path) = &and_path[i] else { continue };
            let name = format!("and{i}");
            node_of[i] = g.node(
                "and",
                "AND",
                "&",
                &name,
                path,
                vec![("a".into(), 1), ("b".into(), 1)],
                vec![("y".into(), 1)],
                vec![],
            );
        }
    }
    let mut lit_src = |g: &mut Graph, l: Lit| -> Option<(u32, u16)> {
        let n = l.node() as usize;
        match aig.nodes[n] {
            Node::Const => {
                let c = *const_node.get_or_insert_with(|| {
                    g.node("const", "Const", "0", "0", "", vec![], vec![(String::new(), 1)], vec![])
                });
                Some((c, 0))
            }
            Node::Input(i) => src_of.get(&i).copied(),
            Node::And(..) => (node_of[n] != u32::MAX).then_some((node_of[n], 0)),
        }
    };
    for (i, n) in aig.nodes.iter().enumerate() {
        if let Node::And(a, b) = *n
            && node_of[i] != u32::MAX
        {
            for (p, l) in [a, b].into_iter().enumerate() {
                if let Some((s, sp)) = lit_src(&mut g, l) {
                    g.edge(s, sp as usize, node_of[i], p, 1, "", l.is_compl());
                }
            }
        }
    }
    for (k, n, p, label) in &sinks {
        let l = aig.outputs[*k];
        if let Some((s, sp)) = lit_src(&mut g, l) {
            g.edge(s, sp as usize, *n, *p, 1, label, l.is_compl());
        }
    }
    g.done()
}

// ----- netlists of primitives -----

fn param_text(p: &synth_netlist::Param, name: &str) -> String {
    if name == "LUT_INIT"
        && let Some(v) = p.as_u64()
    {
        return format!("16'h{v:04X}");
    }
    p.to_string()
}

/// A netlist of primitives (mapped or packed) as a graph.
pub fn netlist_graph(nl: &Netlist) -> Json {
    let mut g = Graph::default();
    let mut node_of: HashMap<synth_netlist::CellId, u32> = HashMap::new();
    let mut pin_port: HashMap<(synth_netlist::CellId, u32), (bool, usize)> = HashMap::new();
    for c in nl.live_cells() {
        let cell = &nl.cells[c];
        let kind = nl.cell_kind(c);
        let name = nl.str(cell.name);
        let mut ins = Vec::new();
        let mut outs = Vec::new();
        for (i, p) in cell.pins.iter().enumerate() {
            if p.net.is_none() {
                continue;
            }
            let pn = nl.str(p.name).to_string();
            if p.dir == Dir::Input {
                pin_port.insert((c, i as u32), (false, ins.len()));
                ins.push((pn, 1));
            } else {
                pin_port.insert((c, i as u32), (true, outs.len()));
                outs.push((pn, 1));
            }
        }
        let (k, label) = match kind {
            "$input" => ("in", name.to_string()),
            "$output" => ("out", name.to_string()),
            "SB_LUT4" => ("lut", "LUT4".to_string()),
            "ICESTORM_LC" => ("lc", "LC".to_string()),
            "SB_CARRY" => ("carry", "CARRY".to_string()),
            k if k.starts_with("SB_DFF") => ("ff", k.trim_start_matches("SB_").to_string()),
            "SB_IO" => ("io", "IO".to_string()),
            k if k.contains("RAM") => ("mem", "RAM".to_string()),
            "SB_GB" => ("gb", "GB".to_string()),
            other => ("cell", other.to_string()),
        };
        let info = cell
            .params
            .iter()
            .map(|(k, v)| {
                let k = nl.str(*k);
                (k.to_string(), param_text(v, k))
            })
            .collect();
        // Mapped cells are named after nothing hierarchical; use the net
        // they drive.
        let mut path = path_of(name).to_string();
        if path.is_empty() {
            let out_net = cell.pins.iter().find(|p| p.dir != Dir::Input).and_then(|p| p.net);
            if let Some(n) = out_net {
                path = path_of(nl.str(nl.nets[n].name)).to_string();
            }
        }
        let id = g.node(k, kind, &label, name, &path, ins, outs, info);
        node_of.insert(c, id);
    }
    for n in nl.live_nets() {
        let net = &nl.nets[n];
        let Some(d) = net.driver else { continue };
        let (Some(&s), Some(&(_, sp))) = (node_of.get(&d.cell), pin_port.get(&(d.cell, d.pin)))
        else {
            continue;
        };
        let name = nl.str(net.name);
        for u in &net.users {
            if let (Some(&t), Some(&(false, dp))) =
                (node_of.get(&u.cell), pin_port.get(&(u.cell, u.pin)))
            {
                g.edge(s, sp, t, dp, 1, name, false);
            }
        }
    }
    g.done()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(path_of("uart.$binary12"), "uart");
        assert_eq!(path_of("a.b.state[3]"), "a.b");
        assert_eq!(path_of("$lut4"), "");
    }
}
