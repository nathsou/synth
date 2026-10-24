//! Designs as text for the views' text mode: word-level RTL, the gate level
//! (AIG with flip-flops and carry chains) and netlists of primitives.
//!
//! The notation borrows Weft's (ports, types, literals, `concat`, `if`,
//! `match`, `inst`) to write the IR as it is, cell by cell, rather than as
//! source: `=` for a combinational cell, `:=` for a state element written as
//! the cell it is (`dff(d, clk: …, en: …, srst: r => 0, init: 0)`), and
//! `->` for the outputs of instances and primitives. It is not meant to be
//! compiled.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use synth_core::{Logic, LogicVec};
use synth_logic::LogicNet;
use synth_logic::aig::{Lit, Node};
use synth_logic::net::Bit;
use synth_netlist::{Const, Dir, INOUT_PORT, INPUT_PORT, Netlist, OUTPUT_PORT, Param};
use synth_rtl::{
    Attr, BinaryOp, CellKind, Clock, Control, Module, PortDir, Sig, SigBit, UnaryOp, WireId,
};

/// Above this many bytes the text is cut (at a line).
const MAX_TEXT: usize = 1 << 20;

/// `text`, cut to [`MAX_TEXT`] bytes with a note, still closing the module.
/// (The longer texts stop growing past the limit, so this is all there is
/// to cut.)
fn cap(mut text: String) -> String {
    if text.len() <= MAX_TEXT {
        return text;
    }
    let cut = text[..MAX_TEXT].rfind('\n').map_or(0, |i| i + 1);
    text.truncate(cut);
    text.push_str("    // … the rest is cut: the text is over 1 MiB\n}\n");
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

/// The type of a value of `width` bits.
fn ty(width: u32, signed: bool) -> String {
    match (width, signed) {
        (1, false) => "bool".into(),
        (w, false) => format!("uint<{w}>"),
        (w, true) => format!("sint<{w}>"),
    }
}

fn port_dir(d: PortDir) -> &'static str {
    match d {
        PortDir::Input => "in",
        PortDir::Output => "out",
        PortDir::Inout => "inout",
    }
}

/// `module name(ports) {`, on one line when it is short.
fn header(name: &str, ports: &[String]) -> String {
    let line = format!("module {name}({}) {{\n", ports.join(", "));
    if line.len() <= 100 {
        return line;
    }
    let mut s = format!("module {name}(\n");
    for p in ports {
        let _ = writeln!(s, "    {p},");
    }
    s.push_str(") {\n");
    s
}

/// A constant, least significant bit first. Fully defined, it is a decimal
/// number, or with `natural` (where its width is needed, as in `concat`) a
/// hexadecimal or binary literal of the full width. Undefined bits are `?`
/// in a binary literal, and a fully undefined value is `undef`.
fn konst(bits: &[Logic], natural: bool) -> String {
    if !bits.is_empty() && bits.iter().all(|b| *b == Logic::X) {
        return "undef".into();
    }
    let defined = bits.iter().all(|b| matches!(b, Logic::Zero | Logic::One));
    if defined && !natural && bits.len() <= 64 {
        let v = bits.iter().rev().fold(0u64, |acc, b| acc << 1 | (*b == Logic::One) as u64);
        return v.to_string();
    }
    if defined && bits.len().is_multiple_of(4) {
        let digits: String = bits
            .chunks(4)
            .rev()
            .map(|c| {
                let d = c.iter().rev().fold(0, |a, b| a << 1 | (*b == Logic::One) as u32);
                char::from_digit(d, 16).unwrap().to_ascii_uppercase()
            })
            .collect();
        return format!("0x{digits}");
    }
    let digits: String = bits
        .iter()
        .rev()
        .map(|b| match b {
            Logic::Zero => '0',
            Logic::One => '1',
            Logic::Z => 'z',
            _ => '?',
        })
        .collect();
    format!("0b{digits}")
}

fn logic_vec(v: &LogicVec) -> String {
    konst(&v.bits().collect::<Vec<_>>(), false)
}

fn attr(a: &Attr) -> String {
    match a {
        Attr::Const(v) => logic_vec(v),
        Attr::Str(s) => format!("{s:?}"),
    }
}

/// A value made of single bits, most significant first: one bit, or a
/// `concat` in which runs of consecutive bits of one name (`x[5], x[4]`)
/// become slices (`x[5:4]`).
fn join_bits(parts: &[String]) -> String {
    if parts.len() == 1 {
        return parts[0].clone();
    }
    let mut runs: Vec<(String, &str, u32, u32)> = Vec::new();
    for p in parts {
        // A complemented bit stays on its own (`!x[3:2]` would read as one
        // operation on a slice).
        let indexed = p.ends_with(']') && !p.starts_with('!');
        let (base, i) = split_index(p);
        if let Some(last) = runs.last_mut()
            && indexed
            && last.1 == base
            && last.3 == i + 1
        {
            last.3 = i;
            continue;
        }
        runs.push((p.clone(), if indexed { base } else { "" }, i, i));
    }
    let parts: Vec<String> = runs
        .into_iter()
        .map(|(p, base, hi, lo)| if hi == lo { p } else { format!("{base}[{hi}:{lo}]") })
        .collect();
    if parts.len() == 1 { parts[0].clone() } else { format!("concat({})", parts.join(", ")) }
}

/// `<A: 1, B: 2>`, or nothing.
fn generics(params: &[String]) -> String {
    if params.is_empty() { String::new() } else { format!("<{}>", params.join(", ")) }
}

/// The arguments of a `dff(…)`: data, clock, enable, resets and power-on
/// value. With `en_over_srst` the synchronous reset acts only when enabled,
/// which is a reset on `en && srst`.
#[allow(clippy::too_many_arguments)]
fn dff_args(
    d: String,
    clk: String,
    posedge: bool,
    en: Option<String>,
    srst: Option<(String, String)>,
    en_over_srst: bool,
    arst: Option<(String, String)>,
    init: Option<String>,
) -> String {
    let mut a = vec![d, format!("clk: {}", if posedge { clk } else { format!("fall({clk})") })];
    if let Some(e) = &en {
        a.push(format!("en: {e}"));
    }
    if let Some((r, v)) = srst {
        match &en {
            Some(e) if en_over_srst => a.push(format!("srst: {e} && {r} => {v}")),
            _ => a.push(format!("srst: {r} => {v}")),
        }
    }
    if let Some((r, v)) = arst {
        a.push(format!("arst: {r} => {v}"));
    }
    if let Some(i) = init {
        a.push(format!("init: {i}"));
    }
    a.join(", ")
}

// ----- word-level RTL -----

/// A unary cell applied to `a` (of `width` bits).
fn unary(op: UnaryOp, a: &str, width: usize) -> String {
    match op {
        UnaryOp::Not => format!("~{a}"),
        UnaryOp::Neg => format!("-{a}"),
        UnaryOp::ReduceAnd => format!("{a}.all()"),
        UnaryOp::ReduceOr => format!("{a}.any()"),
        UnaryOp::ReduceXor => format!("{a}.parity()"),
        UnaryOp::ReduceXnor => format!("!{a}.parity()"),
        UnaryOp::LogicNot if width == 1 => format!("!{a}"),
        UnaryOp::LogicNot => format!("!{a}.any()"),
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
        let natural = runs.len() > 1;
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
                Run::Const(ref v) => konst(v, natural),
            })
            .collect();
        match parts.len() {
            0 => "concat()".into(),
            1 => parts.into_iter().next().unwrap(),
            _ => format!("concat({})", parts.join(", ")),
        }
    }

    fn bit(&mut self, b: SigBit) -> String {
        self.sig(&Sig::bit(b))
    }

    /// A control signal as a condition: `!x` when it is active low.
    fn control(&mut self, c: Control) -> String {
        format!("{}{}", if c.pol { "" } else { "!" }, self.bit(c.sig))
    }

    /// `clk: c`, or `clk: fall(c)` on the falling edge.
    fn clock(&mut self, c: Clock) -> String {
        let s = self.bit(c.sig);
        format!("clk: {}", if c.posedge { s } else { format!("fall({s})") })
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
        let _ = writeln!(o, "    {l} = {rr};");
    }
    if !m.connections.is_empty() {
        o.push('\n');
    }
    for c in m.live_cells() {
        let cell = &m.cells[c];
        let name = &cell.name;
        let mut note = String::new();
        let line = match &cell.kind {
            CellKind::Unary { op, a, y } => {
                format!("{} = {};", r.sig(y), unary(*op, &r.sig(a), a.width()))
            }
            CellKind::Binary { op, signed, a, b, y } => {
                let (wa, wb) = (a.width(), b.width());
                let (a, b) = (r.sig(a), r.sig(b));
                let (a, b) = match op {
                    BinaryOp::LogicAnd | BinaryOp::LogicOr => (
                        if wa > 1 { format!("{a}.any()") } else { a },
                        if wb > 1 { format!("{b}.any()") } else { b },
                    ),
                    _ => (a, b),
                };
                if *signed {
                    note = ", signed".into();
                }
                format!("{} = {a} {} {b};", r.sig(y), binary_op(*op))
            }
            CellKind::Mux { a, b, s, y, .. } => {
                format!(
                    "{} = if {} {{ {} }} else {{ {} }};",
                    r.sig(y),
                    r.bit(*s),
                    r.sig(b),
                    r.sig(a)
                )
            }
            CellKind::Pmux { default, cases, s, y } => {
                // A one-hot select: case `i` when bit `i` of `s` is set.
                let n = s.width();
                let mut t = format!("{} = match {} {{\n", r.sig(y), r.sig(s));
                for (i, case) in cases.iter().enumerate() {
                    let pattern: String =
                        (0..n).rev().map(|k| if k == i { '1' } else { '0' }).collect();
                    let _ = writeln!(t, "        0b{pattern} => {},", r.sig(case));
                }
                let _ = write!(t, "        _ => {},\n    }};", r.sig(default));
                t
            }
            CellKind::Shiftx { a, b, b_signed, offset, y } => {
                let mut idx = r.sig(b);
                match offset {
                    0 => {}
                    o if *o > 0 => idx = format!("{idx} + {o}"),
                    o => idx = format!("{idx} - {}", -o),
                }
                if *b_signed {
                    note = ", signed index".into();
                }
                format!("{} = {}[{idx} +: {}];", r.sig(y), r.sig(a), y.width())
            }
            CellKind::Dff(d) => {
                let defined = d.init.bits().any(|b| matches!(b, Logic::Zero | Logic::One));
                let args = dff_args(
                    r.sig(&d.d),
                    r.bit(d.clk.sig),
                    d.clk.posedge,
                    d.en.map(|c| r.control(c)),
                    d.srst.as_ref().map(|(c, v)| (r.control(*c), logic_vec(v))),
                    d.en_over_srst,
                    d.arst.as_ref().map(|(c, v)| (r.control(*c), logic_vec(v))),
                    defined.then(|| logic_vec(&d.init)),
                );
                format!("{} := dff({args});", r.sig(&d.q))
            }
            CellKind::Latch(l) => {
                format!("{} = latch({}, en: {});", r.sig(&l.q), r.sig(&l.d), r.control(l.en))
            }
            CellKind::Instance(inst) => {
                let params: Vec<String> =
                    inst.params.iter().map(|(k, v)| format!("{k}: {}", attr(v))).collect();
                let conns: Vec<String> = inst
                    .conns
                    .iter()
                    .map(|(k, d, s)| match d {
                        PortDir::Output => format!("{k} -> {}", r.sig(s)),
                        _ => format!("{k}: {}", r.sig(s)),
                    })
                    .collect();
                format!("inst {name} = {}{}({});", inst.module, generics(&params), conns.join(", "))
            }
        };
        if matches!(cell.kind, CellKind::Instance(_)) {
            let _ = writeln!(o, "    {line}");
        } else {
            let _ = writeln!(o, "    {line} // {name}{note}");
        }
    }
    for (_, mem) in m.memories.iter_enumerated() {
        if !mem.alive {
            continue;
        }
        let name = &mem.name;
        let init = mem.init.iter().filter(|w| w.bits().any(|b| b != Logic::X)).count();
        let mut notes = Vec::new();
        if mem.offset != 0 {
            notes.push(format!("addresses from {}", mem.offset));
        }
        if init > 0 {
            notes.push(format!("{init} words initialised"));
        }
        let note =
            if notes.is_empty() { String::new() } else { format!(" // {}", notes.join(", ")) };
        let _ = writeln!(o, "\n    mem {name}: [{}; {}];{note}", ty(mem.width, false), mem.depth);
        for wp in &mem.writes {
            let clk = r.clock(wp.clk);
            let (addr, data) = (r.sig(&wp.addr), r.sig(&wp.data));
            let en = if wp.en.iter().all(|b| *b == wp.en.0[0]) {
                r.bit(wp.en.0[0])
            } else {
                r.sig(&wp.en)
            };
            let _ = writeln!(o, "    {name}.write({addr}, {data}, en: {en}, {clk});");
        }
        for rp in &mem.reads {
            let (addr, data) = (r.sig(&rp.addr), r.sig(&rp.data));
            match rp.clk {
                Some(c) => {
                    let clk = r.clock(c);
                    let en = if rp.en == SigBit::ONE {
                        String::new()
                    } else {
                        format!("en: {}, ", r.bit(rp.en))
                    };
                    let _ = writeln!(o, "    {data} := {name}.read({addr}, {en}{clk});");
                }
                None => {
                    let _ = writeln!(o, "    {data} = {name}[{addr}];");
                }
            }
        }
    }

    let ports: Vec<String> = m
        .ports()
        .iter()
        .map(|&w| {
            let wire = &m.wires[w];
            let dir = port_dir(wire.port.unwrap().0);
            format!("{dir} {}: {}", r.names[w.index()], ty(wire.width, wire.signed))
        })
        .collect();
    let mut out = header(&m.name, &ports);
    let mut any = false;
    for (w, wire) in m.wires.iter_enumerated() {
        if r.used[w.index()] && wire.port.is_none() {
            let t = ty(wire.width, wire.signed);
            let _ = writeln!(out, "    wire {}: {t};", r.names[w.index()]);
            any = true;
        }
    }
    if any && !body.is_empty() {
        out.push('\n');
    }
    out.push_str(&body);
    out.push_str("}\n");
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
        let neg = if l.is_compl() { "!" } else { "" };
        match aig.nodes[l.node() as usize] {
            Node::Const => if l.is_compl() { "1" } else { "0" }.into(),
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
            return konst(&c, false);
        }
        join_bits(&ks.iter().rev().map(|&k| sink(k)).collect::<Vec<_>>())
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

    let ports: Vec<String> = net
        .ports
        .iter()
        .map(|p| format!("{} {}: {}", port_dir(p.dir), p.name, ty(p.bits.len() as u32, false)))
        .collect();
    let mut o = header(&net.name, &ports);
    let _ = writeln!(
        o,
        "    // {ands} AND gates, {} flip-flops, {} carry chains, {} instances",
        net.ffs.len(),
        net.carries.len(),
        net.instances.len()
    );
    if ands > 0 {
        o.push('\n');
    }
    for (i, n) in aig.nodes.iter().enumerate() {
        if let Node::And(a, b) = *n
            && live[i]
        {
            let _ = writeln!(o, "    $g{i} = {} & {};", lit(a), lit(b));
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
        let _ = writeln!(o, "    {name} = {};", sink(k));
    }
    if !net.ffs.is_empty() {
        o.push('\n');
    }
    for (ff, q) in net.ffs.iter().zip(&ff_names) {
        let init = match ff.init {
            Logic::Zero | Logic::One => Some(ff.init.to_char().to_string()),
            _ => None,
        };
        let args = dff_args(
            sink(ff.d),
            sink(ff.clk),
            ff.posedge,
            ff.en.map(sink),
            ff.srst.map(|(k, v)| (sink(k), (v as u8).to_string())),
            ff.en_over_srst,
            ff.arst.map(|(k, v)| (sink(k), (v as u8).to_string())),
            init,
        );
        let _ = writeln!(o, "    {q} := dff({args});");
        if o.len() > MAX_TEXT {
            break;
        }
    }
    if !net.carries.is_empty() {
        o.push('\n');
    }
    for (ci, c) in net.carries.iter().enumerate() {
        let w = c.sum.len();
        let cout = if c.cout.is_some() { format!(", carry{ci}.cout") } else { String::new() };
        let _ = writeln!(
            o,
            "    carry{ci} = carry(a: {}, b: {}, cin: {}); // carry{ci}.s[{}:0]{cout}",
            sinks(&c.a),
            sinks(&c.b),
            sink(c.cin),
            w.saturating_sub(1),
        );
    }
    if !net.instances.is_empty() {
        o.push('\n');
    }
    for inst in &net.instances {
        let params: Vec<String> =
            inst.params.iter().map(|(k, v)| format!("{k}: {}", attr(v))).collect();
        let conns: Vec<String> = inst
            .conns
            .iter()
            .map(|(name, dir, bits)| {
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
                let e = join_bits(&parts);
                match dir {
                    PortDir::Output => format!("{name} -> {e}"),
                    _ => format!("{name}: {e}"),
                }
            })
            .collect();
        let _ = writeln!(
            o,
            "    inst {} = {}{}({});",
            inst.name,
            inst.module,
            generics(&params),
            conns.join(", ")
        );
    }
    o.push_str("}\n");
    cap(o)
}

// ----- netlists of primitives -----

/// A parameter: bit vectors as hexadecimal or binary literals (long ones,
/// memory contents, without their leading zeros), `LUT_INIT` in hexadecimal.
fn param(p: &Param, name: &str) -> String {
    match p {
        Param::Bits(bits) => {
            let v: Vec<Logic> = bits
                .iter()
                .map(|b| match b.to_char() {
                    '0' => Logic::Zero,
                    '1' => Logic::One,
                    'z' => Logic::Z,
                    _ => Logic::X,
                })
                .collect();
            let s = konst(&v, true);
            match s.strip_prefix("0x") {
                Some(digits) if v.len() > 16 => {
                    let d = digits.trim_start_matches('0');
                    format!("0x{}", if d.is_empty() { "0" } else { d })
                }
                _ => s,
            }
        }
        Param::Int(v) if name == "LUT_INIT" => format!("0x{v:04X}"),
        Param::Int(v) => v.to_string(),
        Param::Str(s) => format!("{s:?}"),
    }
}

/// `base[i]` as (`base`, `i`); other names as (`name`, 0).
fn split_index(name: &str) -> (&str, u32) {
    if let Some((b, r)) = name.split_once('[')
        && let Some(i) = r.strip_suffix(']').and_then(|i| i.parse().ok())
    {
        return (b, i);
    }
    (name, 0)
}

/// A netlist of primitives (mapped or packed) as text: one `inst` per cell,
/// its inputs as `PIN: net` and its outputs as `PIN -> net`.
pub fn netlist_text(nl: &Netlist) -> String {
    let net_names = unique(nl.nets.iter().map(|n| nl.str(n.name)));
    let net = |n: synth_netlist::NetId| -> String {
        match nl.nets[n].constant {
            Some(Const::Zero) => "0".into(),
            Some(Const::One) => "1".into(),
            Some(Const::Undef) => "undef".into(),
            None => net_names[n.index()].clone(),
        }
    };
    let top = nl.top.map_or("top", |t| nl.str(t));
    // Ports: (direction, base name) -> highest bit.
    let mut ports: Vec<(&str, &str, u32)> = Vec::new();
    let mut assigns = String::new();
    let mut cells = String::new();
    let mut count = 0;
    for c in nl.live_cells() {
        let cell = &nl.cells[c];
        let kind = nl.cell_kind(c);
        let name = nl.str(cell.name);
        if matches!(kind, INPUT_PORT | OUTPUT_PORT | INOUT_PORT) {
            let dir = match kind {
                INPUT_PORT => "in",
                OUTPUT_PORT => "out",
                _ => "inout",
            };
            let (base, i) = split_index(name);
            match ports.iter_mut().find(|p| p.0 == dir && p.1 == base) {
                Some(p) => p.2 = p.2.max(i),
                None => ports.push((dir, base, i)),
            }
            if let Some(n) = cell.pins.first().and_then(|p| p.net) {
                let n = net(n);
                if n != name {
                    let _ = match kind {
                        INPUT_PORT => writeln!(assigns, "    {n} = {name};"),
                        _ => writeln!(assigns, "    {name} = {n};"),
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
            .map(|(k, v)| {
                let k = nl.str(*k);
                format!("{k}: {}", param(v, k))
            })
            .collect();
        // Pins `P[i]` grouped into vectors, in the order of their first bit.
        let mut order: Vec<&str> = Vec::new();
        let mut buses: HashMap<&str, (Dir, BTreeMap<u32, String>)> = HashMap::new();
        for p in &cell.pins {
            let Some(n) = p.net else { continue };
            let (base, i) = split_index(nl.str(p.name));
            buses
                .entry(base)
                .or_insert_with(|| {
                    order.push(base);
                    (p.dir, BTreeMap::new())
                })
                .1
                .insert(i, net(n));
        }
        let pins: Vec<String> = order
            .iter()
            .map(|base| {
                let (dir, bits) = &buses[base];
                let top = bits.keys().max().copied().unwrap_or(0);
                let parts: Vec<String> = (0..=top)
                    .rev()
                    .map(|i| bits.get(&i).cloned().unwrap_or_else(|| "_".into()))
                    .collect();
                let e = join_bits(&parts);
                match dir {
                    Dir::Input => format!("{base}: {e}"),
                    _ => format!("{base} -> {e}"),
                }
            })
            .collect();
        let _ =
            writeln!(cells, "    inst {name} = {kind}{}({});", generics(&params), pins.join(", "));
    }
    let ports: Vec<String> = ports
        .iter()
        .map(|(dir, base, top)| format!("{dir} {base}: {}", ty(top + 1, false)))
        .collect();
    let mut o = header(top, &ports);
    let _ = writeln!(o, "    // {count} cells");
    if !assigns.is_empty() {
        o.push('\n');
        o.push_str(&assigns);
    }
    if !cells.is_empty() {
        o.push('\n');
        o.push_str(&cells);
    }
    o.push_str("}\n");
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
        assert!(t.ends_with("over 1 MiB\n}\n"));
        assert_eq!(t.lines().rev().nth(2), Some("x = y;"));
    }

    #[test]
    fn unique_names() {
        assert_eq!(unique(["a", "b", "a"].into_iter()), ["a", "b", "a$2"]);
    }

    #[test]
    fn bit_runs() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(join_bits(&s(&["a[3]", "a[2]", "a[1]"])), "a[3:1]");
        assert_eq!(
            join_bits(&s(&["a[3]", "!a[2]", "!a[1]", "b", "a[0]"])),
            "concat(a[3], !a[2], !a[1], b, a[0])"
        );
        assert_eq!(join_bits(&s(&["x"])), "x");
    }

    #[test]
    fn constants() {
        use Logic::{One as I, X, Zero as O};
        assert_eq!(konst(&[I, O, I], false), "5");
        assert_eq!(konst(&[I, O, I], true), "0b101");
        assert_eq!(konst(&[I, O, I, O, O, O, O, I], true), "0x85");
        assert_eq!(konst(&[X, I], false), "0b1?");
        assert_eq!(konst(&[X, X], false), "undef");
    }
}
