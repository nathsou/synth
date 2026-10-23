//! Designs as text for the views' text mode: word-level RTL, the gate level
//! (AIG with flip-flops and carry chains) and netlists of primitives, each
//! in a Verilog-like notation (it reads as Verilog, and the editor's Verilog
//! highlighting applies, but it is a dump of the IR, not meant to be
//! simulated).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use synth_core::{Logic, LogicVec};
use synth_logic::LogicNet;
use synth_logic::aig::{Lit, Node};
use synth_logic::net::Bit;
use synth_netlist::{Const, INOUT_PORT, INPUT_PORT, Netlist, OUTPUT_PORT, Param};
use synth_rtl::{
    Attr, BinaryOp, CellKind, Clock, Control, Module, PortDir, Sig, SigBit, UnaryOp, WireId,
};

/// Above this many bytes the text is cut (at a line).
const MAX_TEXT: usize = 1 << 20;

/// `text`, cut to [`MAX_TEXT`] bytes with a note, still ending the module.
/// (The longer texts stop growing past the limit, so this is all there is
/// to cut.)
fn cap(mut text: String) -> String {
    if text.len() <= MAX_TEXT {
        return text;
    }
    let cut = text[..MAX_TEXT].rfind('\n').map_or(0, |i| i + 1);
    text.truncate(cut);
    text.push_str("  // … the rest is cut: the text is over 1 MiB\nendmodule\n");
    text
}

/// Names made unique: a repeated name gets the suffix `$<index>`.
fn unique<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let names: Vec<&str> = names.collect();
    let mut count: HashMap<&str, u32> = HashMap::new();
    for n in &names {
        *count.entry(n).or_default() += 1;
    }
    let mut seen: HashSet<&str> = HashSet::new();
    names
        .iter()
        .enumerate()
        .map(
            |(i, n)| {
                if count[n] > 1 && !seen.insert(n) { format!("{n}${i}") } else { n.to_string() }
            },
        )
        .collect()
}

fn range(width: u32) -> String {
    if width > 1 { format!("[{}:0] ", width - 1) } else { String::new() }
}

fn port_dir(d: PortDir) -> &'static str {
    match d {
        PortDir::Input => "input",
        PortDir::Output => "output",
        PortDir::Inout => "inout",
    }
}

fn attr(a: &Attr) -> String {
    match a {
        Attr::Const(v) => v.to_string(),
        Attr::Str(s) => format!("{s:?}"),
    }
}

// ----- word-level RTL -----

fn unary_op(op: UnaryOp) -> &'static str {
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

fn binary_op(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        And => "&",
        Or => "|",
        Xor => "^",
        Xnor => "~^",
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Mod => "%",
        Shl => "<<",
        Shr => ">>",
        Sshr => ">>>",
        Eq => "==",
        Ne => "!=",
        Eqx => "===",
        Nex => "!==",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        LogicAnd => "&&",
        LogicOr => "||",
    }
}

struct Rtl<'m> {
    m: &'m Module,
    names: Vec<String>,
    /// The wires the text refers to, declared before it.
    used: Vec<bool>,
}

impl Rtl<'_> {
    /// A signal: a wire or part of one, a constant, or a concatenation.
    fn sig(&mut self, s: &Sig) -> String {
        enum Run {
            Wire(WireId, u32, u32),
            Const(Vec<Logic>),
        }
        let mut runs: Vec<Run> = Vec::new();
        for &b in s.iter() {
            match (b, runs.last_mut()) {
                (SigBit::Wire(w, i), Some(Run::Wire(rw, lo, n))) if *rw == w && *lo + *n == i => {
                    *n += 1
                }
                (SigBit::Const(l), Some(Run::Const(v))) => v.push(l),
                (SigBit::Wire(w, i), _) => runs.push(Run::Wire(w, i, 1)),
                (SigBit::Const(l), _) => runs.push(Run::Const(vec![l])),
            }
        }
        let parts: Vec<String> = runs
            .iter()
            .rev()
            .map(|r| match *r {
                Run::Wire(w, lo, n) => {
                    self.used[w.index()] = true;
                    let name = &self.names[w.index()];
                    if lo == 0 && n == self.m.wires[w].width {
                        name.clone()
                    } else if n == 1 {
                        format!("{name}[{lo}]")
                    } else {
                        format!("{name}[{}:{lo}]", lo + n - 1)
                    }
                }
                Run::Const(ref v) => LogicVec::from_bits(v).to_string(),
            })
            .collect();
        match parts.len() {
            0 => "{}".into(),
            1 => parts.into_iter().next().unwrap(),
            _ => format!("{{{}}}", parts.join(", ")),
        }
    }

    fn bit(&mut self, b: SigBit) -> String {
        self.sig(&Sig::bit(b))
    }

    fn clock(&mut self, c: Clock) -> String {
        format!("{} {}", if c.posedge { "posedge" } else { "negedge" }, self.bit(c.sig))
    }

    /// A control signal as a condition: `!x` when it is active low.
    fn control(&mut self, c: Control) -> String {
        format!("{}{}", if c.pol { "" } else { "!" }, self.bit(c.sig))
    }
}

/// A word-level RTL module as text.
pub fn rtl_text(m: &Module) -> String {
    let mut r = Rtl {
        m,
        names: unique(m.wires.iter().map(|w| w.name.as_str())),
        used: vec![false; m.wires.len()],
    };
    let mut body = String::new();
    let o = &mut body;

    for (lhs, rhs) in &m.connections {
        let (l, rr) = (r.sig(lhs), r.sig(rhs));
        let _ = writeln!(o, "  assign {l} = {rr};");
    }
    if !m.connections.is_empty() {
        o.push('\n');
    }
    for c in m.live_cells() {
        let cell = &m.cells[c];
        let name = &cell.name;
        let line = match &cell.kind {
            CellKind::Unary { op, a, y } => {
                format!("assign {} = {}{};", r.sig(y), unary_op(*op), r.sig(a))
            }
            CellKind::Binary { op, signed, a, b, y } => {
                let (mut a, mut b) = (r.sig(a), r.sig(b));
                if *signed {
                    a = format!("$signed({a})");
                    b = format!("$signed({b})");
                }
                format!("assign {} = {a} {} {b};", r.sig(y), binary_op(*op))
            }
            CellKind::Mux { a, b, s, y, .. } => {
                format!("assign {} = {} ? {} : {};", r.sig(y), r.bit(*s), r.sig(b), r.sig(a))
            }
            CellKind::Pmux { default, cases, s, y } => {
                // A one-hot select: case `i` when bit `i` of `s` is set.
                let y = r.sig(y);
                let pad = " ".repeat(y.len() + 10);
                let mut t = format!("assign {y} = ");
                for (i, case) in cases.iter().enumerate() {
                    let sel = r.bit(s.0[i]);
                    let _ = write!(t, "{sel} ? {} :\n  {pad}", r.sig(case));
                }
                let _ = write!(t, "{};", r.sig(default));
                t
            }
            CellKind::Shiftx { a, b, b_signed, offset, y } => {
                let mut idx = r.sig(b);
                if *b_signed {
                    idx = format!("$signed({idx})");
                }
                match offset {
                    0 => {}
                    o if *o > 0 => idx = format!("{idx} + {o}"),
                    o => idx = format!("{idx} - {}", -o),
                }
                format!("assign {} = {}[{idx} +: {}];", r.sig(y), r.sig(a), y.width())
            }
            CellKind::Dff(d) => {
                let mut events = vec![r.clock(d.clk)];
                let mut t = String::new();
                let q = r.sig(&d.q);
                if let Some((c, v)) = &d.arst {
                    let pol = if c.pol { "posedge" } else { "negedge" };
                    events.push(format!("{pol} {}", r.bit(c.sig)));
                    let _ = write!(t, "if ({}) {q} <= {v}; else ", r.control(*c));
                }
                let en = d.en.map(|c| r.control(c));
                let srst = d.srst.as_ref().map(|(c, v)| (r.control(*c), v));
                match (en, srst) {
                    (Some(en), Some((rst, v))) if d.en_over_srst => {
                        let _ = write!(t, "if ({en}) {q} <= {rst} ? {v} : ");
                    }
                    (en, srst) => {
                        if let Some((rst, v)) = srst {
                            let _ = write!(t, "if ({rst}) {q} <= {v}; else ");
                        }
                        if let Some(en) = en {
                            let _ = write!(t, "if ({en}) ");
                        }
                        let _ = write!(t, "{q} <= ");
                    }
                }
                let _ = write!(t, "{};", r.sig(&d.d));
                if d.init.bits().any(|b| matches!(b, Logic::Zero | Logic::One)) {
                    let _ = write!(t, " // {name}, init {}", d.init);
                }
                format!("always @({}) {t}", events.join(" or "))
            }
            CellKind::Latch(l) => {
                format!("always @* if ({}) {} = {};", r.control(l.en), r.sig(&l.q), r.sig(&l.d))
            }
            CellKind::Instance(inst) => {
                let params: Vec<String> =
                    inst.params.iter().map(|(k, v)| format!(".{k}({})", attr(v))).collect();
                let conns: Vec<String> =
                    inst.conns.iter().map(|(k, _, s)| format!(".{k}({})", r.sig(s))).collect();
                let ps = if params.is_empty() {
                    String::new()
                } else {
                    format!(" #({})", params.join(", "))
                };
                format!("{}{ps} {name} ({});", inst.module, conns.join(", "))
            }
        };
        if line.contains("//") || matches!(cell.kind, CellKind::Instance(_)) {
            let _ = writeln!(o, "  {line}");
        } else {
            let _ = writeln!(o, "  {line} // {name}");
        }
    }
    for (_, mem) in m.memories.iter_enumerated() {
        if !mem.alive {
            continue;
        }
        let name = &mem.name;
        let first = mem.offset;
        let last = mem.offset + mem.depth as i64 - 1;
        let init = mem.init.iter().filter(|w| w.bits().any(|b| b != Logic::X)).count();
        let note = if init > 0 { format!(" // {init} words initialised") } else { String::new() };
        let _ = writeln!(o, "\n  reg {}{name} [{first}:{last}];{note}", range(mem.width));
        for wp in &mem.writes {
            let clk = r.clock(wp.clk);
            let (addr, data) = (r.sig(&wp.addr), r.sig(&wp.data));
            let en = if wp.en.iter().all(|b| *b == wp.en.0[0]) {
                r.bit(wp.en.0[0])
            } else {
                r.sig(&wp.en)
            };
            let _ = writeln!(o, "  always @({clk}) if ({en}) {name}[{addr}] <= {data};");
        }
        for rp in &mem.reads {
            let (addr, data) = (r.sig(&rp.addr), r.sig(&rp.data));
            match rp.clk {
                Some(c) => {
                    let clk = r.clock(c);
                    let en = if rp.en == SigBit::ONE {
                        String::new()
                    } else {
                        format!("if ({}) ", r.bit(rp.en))
                    };
                    let _ = writeln!(o, "  always @({clk}) {en}{data} <= {name}[{addr}];");
                }
                None => {
                    let _ = writeln!(o, "  assign {data} = {name}[{addr}];");
                }
            }
        }
    }

    let mut out = String::new();
    let ports = m.ports();
    let decls: Vec<String> = ports
        .iter()
        .map(|&w| {
            let wire = &m.wires[w];
            let signed = if wire.signed { "signed " } else { "" };
            let dir = port_dir(wire.port.unwrap().0);
            format!("  {dir} {signed}{}{}", range(wire.width), r.names[w.index()])
        })
        .collect();
    if decls.is_empty() {
        let _ = writeln!(out, "module {};", m.name);
    } else {
        let _ = writeln!(out, "module {} (\n{}\n);", m.name, decls.join(",\n"));
    }
    let mut any = false;
    for (w, wire) in m.wires.iter_enumerated() {
        if r.used[w.index()] && wire.port.is_none() {
            let signed = if wire.signed { "signed " } else { "" };
            let _ = writeln!(out, "  wire {signed}{}{};", range(wire.width), r.names[w.index()]);
            any = true;
        }
    }
    if any {
        out.push('\n');
    }
    out.push_str(&body);
    out.push_str("endmodule\n");
    cap(out)
}

// ----- gate level -----

/// A logic network (AIG, flip-flops, carry chains, instances) as text. AND
/// gates are `$g<node>`; the values the graph takes in are named after
/// what makes them (ports, flip-flops, carry chains, instances).
pub fn aig_text(net: &LogicNet) -> String {
    let aig = &net.aig;
    let mut src = vec![String::new(); aig.inputs.len()];
    let bit_name = |name: &str, bits: &[Bit], i: usize| {
        if bits.len() == 1 { name.to_string() } else { format!("{name}[{i}]") }
    };
    for p in &net.ports {
        for (i, b) in p.bits.iter().enumerate() {
            if let Bit::Src(s) = b {
                src[*s as usize] = bit_name(&p.name, &p.bits, i);
            }
        }
    }
    // A word's flip-flops share its name: number its bits.
    let mut bits_of: HashMap<&str, usize> = HashMap::new();
    for ff in &net.ffs {
        *bits_of.entry(&ff.name).or_default() += 1;
    }
    let mut next: HashMap<&str, usize> = HashMap::new();
    let ff_names: Vec<String> = net
        .ffs
        .iter()
        .map(|ff| {
            if bits_of[ff.name.as_str()] == 1 {
                return ff.name.clone();
            }
            let i = next.entry(&ff.name).or_default();
            *i += 1;
            format!("{}[{}]", ff.name, *i - 1)
        })
        .collect();
    for (ff, name) in net.ffs.iter().zip(&ff_names) {
        src[ff.q as usize] = name.clone();
    }
    for (ci, c) in net.carries.iter().enumerate() {
        for (i, s) in c.sum.iter().enumerate() {
            src[*s as usize] = format!("carry{ci}.s[{i}]");
        }
        if let Some(co) = c.cout {
            src[co as usize] = format!("carry{ci}.cout");
        }
    }
    for inst in &net.instances {
        for (name, _, bits) in &inst.conns {
            for (i, b) in bits.iter().enumerate() {
                if let Bit::Src(s) = b {
                    src[*s as usize] = format!("{}.{}", inst.name, bit_name(name, bits, i));
                }
            }
        }
    }
    let lit = |l: Lit| -> String {
        let neg = if l.is_compl() { "~" } else { "" };
        match aig.nodes[l.node() as usize] {
            Node::Const => if l.is_compl() { "1'b1" } else { "1'b0" }.into(),
            Node::Input(i) => format!("{neg}{}", src[i as usize]),
            Node::And(..) => format!("{neg}$g{}", l.node()),
        }
    };
    let sink = |k: usize| lit(aig.outputs[k]);
    let sinks = |ks: &[usize]| -> String {
        if ks.len() == 1 {
            return sink(ks[0]);
        }
        let consts: Option<Vec<Logic>> = ks
            .iter()
            .map(|&k| {
                let l = aig.outputs[k];
                l.is_const().then_some(if l.is_compl() { Logic::One } else { Logic::Zero })
            })
            .collect();
        if let Some(c) = consts {
            return LogicVec::from_bits(&c).to_string();
        }
        format!("{{{}}}", ks.iter().rev().map(|&k| sink(k)).collect::<Vec<_>>().join(", "))
    };

    // The AND gates that feed a sink, in topological order.
    let mut live = vec![false; aig.nodes.len()];
    let mut stack: Vec<u32> = aig.outputs.iter().map(|l| l.node()).collect();
    while let Some(n) = stack.pop() {
        if let Node::And(a, b) = aig.nodes[n as usize]
            && !live[n as usize]
        {
            live[n as usize] = true;
            stack.push(a.node());
            stack.push(b.node());
        }
    }
    let ands = live.iter().filter(|x| **x).count();

    let mut o = String::new();
    let _ = writeln!(o, "module {};", net.name);
    let _ = writeln!(
        o,
        "  // {ands} AND gates, {} flip-flops, {} carry chains, {} instances",
        net.ffs.len(),
        net.carries.len(),
        net.instances.len()
    );
    for p in &net.ports {
        let _ = writeln!(o, "  {} {}{};", port_dir(p.dir), range(p.bits.len() as u32), p.name);
    }
    if ands > 0 {
        o.push('\n');
    }
    for (i, n) in aig.nodes.iter().enumerate() {
        if let Node::And(a, b) = *n
            && live[i]
        {
            let _ = writeln!(o, "  assign $g{i} = {} & {};", lit(a), lit(b));
        }
        if o.len() > MAX_TEXT {
            break;
        }
    }
    let outs: Vec<_> = net
        .ports
        .iter()
        .flat_map(|p| p.bits.iter().enumerate().map(move |(i, b)| (p, i, b)))
        .filter_map(|(p, i, b)| match b {
            Bit::Snk(k) => Some((bit_name(&p.name, &p.bits, i), *k)),
            _ => None,
        })
        .collect();
    if !outs.is_empty() {
        o.push('\n');
    }
    for (name, k) in outs {
        let _ = writeln!(o, "  assign {name} = {};", sink(k));
    }
    if !net.ffs.is_empty() {
        o.push('\n');
    }
    for (ff, q) in net.ffs.iter().zip(&ff_names) {
        let edge = if ff.posedge { "posedge" } else { "negedge" };
        let mut events = format!("{edge} {}", sink(ff.clk));
        let mut t = String::new();
        if let Some((k, v)) = ff.arst {
            let _ = write!(events, " or posedge {}", sink(k));
            let _ = write!(t, "if ({}) {q} <= {}; else ", sink(k), v as u8);
        }
        match (ff.en, ff.srst) {
            (Some(en), Some((k, v))) if ff.en_over_srst => {
                let _ = write!(t, "if ({}) {q} <= {} ? {} : ", sink(en), sink(k), v as u8);
            }
            (en, srst) => {
                if let Some((k, v)) = srst {
                    let _ = write!(t, "if ({}) {q} <= {}; else ", sink(k), v as u8);
                }
                if let Some(en) = en {
                    let _ = write!(t, "if ({}) ", sink(en));
                }
                let _ = write!(t, "{q} <= ");
            }
        }
        let init = match ff.init {
            Logic::Zero | Logic::One => format!(" // init {}", ff.init.to_char()),
            _ => String::new(),
        };
        let _ = writeln!(o, "  always @({events}) {t}{};{init}", sink(ff.d));
        if o.len() > MAX_TEXT {
            break;
        }
    }
    if !net.carries.is_empty() {
        o.push('\n');
    }
    for (ci, c) in net.carries.iter().enumerate() {
        let w = c.sum.len();
        let cout = if c.cout.is_some() { format!("carry{ci}.cout, ") } else { String::new() };
        let _ = writeln!(
            o,
            "  assign {{{cout}carry{ci}.s[{}:0]}} = {} + {} + {};",
            w.saturating_sub(1),
            sinks(&c.a),
            sinks(&c.b),
            sink(c.cin)
        );
    }
    if !net.instances.is_empty() {
        o.push('\n');
    }
    for inst in &net.instances {
        let params: Vec<String> =
            inst.params.iter().map(|(k, v)| format!(".{k}({})", attr(v))).collect();
        let conns: Vec<String> = inst
            .conns
            .iter()
            .map(|(name, _, bits)| {
                let parts: Vec<String> = bits
                    .iter()
                    .enumerate()
                    .rev()
                    .map(|(i, b)| match b {
                        Bit::Src(s) => src[*s as usize].clone(),
                        Bit::Snk(k) => sink(*k),
                        Bit::Pad(_) => format!("{}.{}", inst.name, bit_name(name, bits, i)),
                    })
                    .collect();
                let e = if parts.len() == 1 {
                    parts.into_iter().next().unwrap()
                } else {
                    format!("{{{}}}", parts.join(", "))
                };
                format!(".{name}({e})")
            })
            .collect();
        let ps =
            if params.is_empty() { String::new() } else { format!(" #({})", params.join(", ")) };
        let _ = writeln!(o, "  {}{ps} {} ({});", inst.module, inst.name, conns.join(", "));
    }
    o.push_str("endmodule\n");
    cap(o)
}

// ----- netlists of primitives -----

/// A parameter: bit vectors in hexadecimal when they are fully defined.
fn param(p: &Param, name: &str) -> String {
    let Param::Bits(bits) = p else { return crate::graph::param_text(p, name) };
    let defined: Option<Vec<u8>> = bits
        .iter()
        .map(|b| match b.to_char() {
            '0' => Some(0),
            '1' => Some(1),
            _ => None,
        })
        .collect();
    match defined {
        Some(v) if v.len() > 1 => {
            let mut hex: String = v
                .chunks(4)
                .rev()
                .map(|c| {
                    let d = c.iter().rev().fold(0u32, |a, b| a << 1 | *b as u32);
                    char::from_digit(d, 16).unwrap().to_ascii_uppercase()
                })
                .collect();
            if v.len() > 16 {
                // Long vectors (memory contents): without the leading zeros.
                let digits = hex.trim_start_matches('0');
                hex = if digits.is_empty() { "0".into() } else { digits.into() };
            }
            format!("{}'h{hex}", v.len())
        }
        _ => format!(
            "{}'b{}",
            bits.len(),
            bits.iter().rev().map(|b| b.to_char()).collect::<String>()
        ),
    }
}

/// A netlist of primitives (mapped or packed) as text: one line per cell,
/// its pins connected to nets by name.
pub fn netlist_text(nl: &Netlist) -> String {
    let net_names = unique(nl.nets.iter().map(|n| nl.str(n.name)));
    let net = |n: synth_netlist::NetId| -> String {
        match nl.nets[n].constant {
            Some(Const::Zero) => "1'b0".into(),
            Some(Const::One) => "1'b1".into(),
            Some(Const::Undef) => "1'bx".into(),
            None => net_names[n.index()].clone(),
        }
    };
    let top = nl.top.map_or("top", |t| nl.str(t));
    let mut ports = String::new();
    let mut assigns = String::new();
    let mut cells = String::new();
    let mut count = 0;
    for c in nl.live_cells() {
        let cell = &nl.cells[c];
        let kind = nl.cell_kind(c);
        let name = nl.str(cell.name);
        if matches!(kind, INPUT_PORT | OUTPUT_PORT | INOUT_PORT) {
            let dir = match kind {
                INPUT_PORT => "input",
                OUTPUT_PORT => "output",
                _ => "inout",
            };
            let _ = writeln!(ports, "  {dir} {name};");
            if let Some(n) = cell.pins.first().and_then(|p| p.net) {
                let n = net(n);
                if n != name {
                    let _ = match kind {
                        INPUT_PORT => writeln!(assigns, "  assign {n} = {name};"),
                        _ => writeln!(assigns, "  assign {name} = {n};"),
                    };
                }
            }
            continue;
        }
        count += 1;
        if cells.len() > MAX_TEXT {
            continue;
        }
        let params: Vec<String> = cell
            .params
            .iter()
            .map(|(k, v)| format!(".{}({})", nl.str(*k), param(v, nl.str(*k))))
            .collect();
        // Pins `P[i]` grouped into vectors, in the order of their first bit.
        let mut order: Vec<&str> = Vec::new();
        let mut buses: HashMap<&str, BTreeMap<u32, String>> = HashMap::new();
        for p in &cell.pins {
            let Some(n) = p.net else { continue };
            let pname = nl.str(p.name);
            let (base, i) = match pname.split_once('[') {
                Some((b, r)) => match r.strip_suffix(']').and_then(|i| i.parse().ok()) {
                    Some(i) => (b, i),
                    None => (pname, 0),
                },
                None => (pname, 0),
            };
            buses
                .entry(base)
                .or_insert_with(|| {
                    order.push(base);
                    BTreeMap::new()
                })
                .insert(i, net(n));
        }
        let pins: Vec<String> = order
            .iter()
            .map(|base| {
                let bits = &buses[base];
                let top = bits.keys().max().copied().unwrap_or(0);
                if top == 0 {
                    return format!(".{base}({})", bits[&0]);
                }
                let parts: Vec<&str> =
                    (0..=top).rev().map(|i| bits.get(&i).map_or("1'bz", |n| n.as_str())).collect();
                format!(".{base}({{{}}})", parts.join(", "))
            })
            .collect();
        let ps =
            if params.is_empty() { String::new() } else { format!(" #({})", params.join(", ")) };
        let _ = writeln!(cells, "  {kind}{ps} {name} ({});", pins.join(", "));
    }
    let mut o = format!("module {top};\n  // {count} cells\n");
    o.push_str(&ports);
    if !assigns.is_empty() {
        o.push('\n');
        o.push_str(&assigns);
    }
    if !cells.is_empty() {
        o.push('\n');
        o.push_str(&cells);
    }
    o.push_str("endmodule\n");
    cap(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_at_a_line() {
        let long = "x = y;\n".repeat(MAX_TEXT / 4);
        let t = cap(long);
        assert!(t.len() <= MAX_TEXT + 64);
        assert!(t.ends_with("over 1 MiB\nendmodule\n"));
        assert_eq!(t.lines().rev().nth(2), Some("x = y;"));
    }

    #[test]
    fn unique_names() {
        assert_eq!(unique(["a", "b", "a"].into_iter()), ["a", "b", "a$2"]);
    }
}
