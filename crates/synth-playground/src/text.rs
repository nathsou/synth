//! Designs as text, in three small IR languages that share a lexical layer
//! (specified in docs/IR_TEXT.md): word-level RTL (`.rtl`), the gate level
//! (`.gates`) and netlists of primitives (`.netlist`). Each writes its IR as
//! it is, cell by cell, under the IR's own names.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use synth_core::{Logic, LogicVec};
use synth_logic::LogicNet;
use synth_logic::aig::{Lit, Node};
use synth_logic::net::Bit;
use synth_netlist::{Const, Dir, INOUT_PORT, INPUT_PORT, Netlist, OUTPUT_PORT, Param};
use synth_rtl::{Attr, BinaryOp, CellKind, Clock, Control, Module, PortDir, Sig, SigBit, UnaryOp};

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

// ----- lexical layer -----

/// Words that begin a statement: a name spelled like one is quoted.
const KEYWORDS: &[&str] = &[
    "module", "input", "output", "inout", "wire", "connect", "inst", "cell", "memory", "init",
    "write",
];

/// A name as written: bare when it is an identifier
/// (`[A-Za-z_$][A-Za-z0-9_$.]*`, not a keyword), quoted otherwise.
fn name(s: &str) -> String {
    let mut chars = s.chars();
    let ident = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '.'))
        && !KEYWORDS.contains(&s);
    if ident {
        return s.to_string();
    }
    let mut q = String::with_capacity(s.len() + 2);
    q.push('"');
    for c in s.chars() {
        if matches!(c, '"' | '\\') {
            q.push('\\');
        }
        q.push(c);
    }
    q.push('"');
    q
}

/// A sized constant, least significant bit first: `W'dV` when it is fully
/// defined and fits 64 bits, `W'hH` for wider defined values, `W'bB` (with
/// `x` and `z`) otherwise.
fn sized(bits: &[Logic]) -> String {
    let w = bits.len();
    let defined = bits.iter().all(|b| matches!(b, Logic::Zero | Logic::One));
    if defined && w <= 64 {
        let v = bits.iter().rev().fold(0u64, |acc, b| acc << 1 | (*b == Logic::One) as u64);
        return format!("{w}'d{v}");
    }
    if defined {
        return format!("{w}'h{}", hex(bits, false));
    }
    let digits: String = bits
        .iter()
        .rev()
        .map(|b| match b {
            Logic::Zero => '0',
            Logic::One => '1',
            Logic::Z => 'z',
            _ => 'x',
        })
        .collect();
    format!("{w}'b{digits}")
}

/// Hexadecimal digits of defined bits (least significant first): all of
/// them with `full`, else without leading zeros.
fn hex(bits: &[Logic], full: bool) -> String {
    let digits: String = bits
        .chunks(4)
        .rev()
        .map(|c| {
            let d = c.iter().rev().fold(0, |a, b| a << 1 | (*b == Logic::One) as u32);
            char::from_digit(d, 16).unwrap().to_ascii_uppercase()
        })
        .collect();
    if full {
        return digits;
    }
    let t = digits.trim_start_matches('0');
    if t.is_empty() { "0".into() } else { t.into() }
}

fn logic_vec(v: &LogicVec) -> String {
    sized(&v.bits().collect::<Vec<_>>())
}

/// A memory word: in hexadecimal with all its digits when it is defined
/// and its width a multiple of four (memory contents read as a hex dump).
fn word(v: &LogicVec) -> String {
    let bits: Vec<Logic> = v.bits().collect();
    if bits.len().is_multiple_of(4) && bits.iter().all(|b| matches!(b, Logic::Zero | Logic::One)) {
        format!("{}'h{}", bits.len(), hex(&bits, true))
    } else {
        sized(&bits)
    }
}

fn attr(a: &Attr) -> String {
    match a {
        Attr::Const(v) => logic_vec(v),
        Attr::Str(s) => format!("{s:?}"),
    }
}

/// One bit (or with `idx` None, a whole value) of a named signal, maybe
/// complemented, or a constant bit.
#[derive(Clone, PartialEq)]
enum BitRef {
    Named { base: String, idx: Option<u32>, neg: bool },
    Const(Logic),
}

impl BitRef {
    fn named(base: &str, idx: Option<u32>) -> BitRef {
        BitRef::Named { base: base.to_string(), idx, neg: false }
    }

    /// The complement, when `c`.
    fn not(mut self, c: bool) -> BitRef {
        match &mut self {
            BitRef::Named { neg, .. } => *neg ^= c,
            BitRef::Const(l) if c => {
                *l = match l {
                    Logic::Zero => Logic::One,
                    Logic::One => Logic::Zero,
                    _ => *l,
                }
            }
            BitRef::Const(_) => {}
        }
        self
    }
}

/// Bits, most significant first: one reference, a constant, or a `{…}`
/// concatenation in which runs of consecutive bits of one name become
/// slices (`x[5:4]`) and runs of constant bits become one constant.
fn bits_text(bits: &[BitRef]) -> String {
    enum Part {
        Slice(String, u32, u32),
        Single(String),
        Const(Vec<Logic>),
    }
    let mut parts: Vec<Part> = Vec::new();
    for b in bits {
        match (b, parts.last_mut()) {
            (BitRef::Named { base, idx: Some(i), neg: false }, Some(Part::Slice(pb, _, lo)))
                if pb == base && *lo == i + 1 =>
            {
                *lo = *i
            }
            (BitRef::Named { base, idx: Some(i), neg: false }, _) => {
                parts.push(Part::Slice(base.clone(), *i, *i))
            }
            (BitRef::Named { base, idx, neg }, _) => {
                let mut s = format!("{}{}", if *neg { "~" } else { "" }, name(base));
                if let Some(i) = idx {
                    let _ = write!(s, "[{i}]");
                }
                parts.push(Part::Single(s))
            }
            // Constants gather least significant last: prepend.
            (BitRef::Const(l), Some(Part::Const(v))) => v.insert(0, *l),
            (BitRef::Const(l), _) => parts.push(Part::Const(vec![*l])),
        }
    }
    let parts: Vec<String> = parts
        .into_iter()
        .map(|p| match p {
            Part::Slice(b, hi, lo) if hi == lo => format!("{}[{hi}]", name(&b)),
            Part::Slice(b, hi, lo) => format!("{}[{hi}:{lo}]", name(&b)),
            Part::Single(s) => s,
            Part::Const(v) => sized(&v),
        })
        .collect();
    if parts.len() == 1 {
        parts.into_iter().next().unwrap()
    } else {
        format!("{{{}}}", parts.join(", "))
    }
}

/// `{K: v, …} ` (with its trailing space), or nothing.
fn params_text(params: &[String]) -> String {
    if params.is_empty() { String::new() } else { format!("{{{}}} ", params.join(", ")) }
}

/// A connection of an instance or cell pin: `P: x` for an input, `P -> x`
/// for an output, `P <-> x` for an inout.
fn pin(p: &str, dir: PortDir, value: &str) -> String {
    let arrow = match dir {
        PortDir::Input => ":",
        PortDir::Output => " ->",
        PortDir::Inout => " <->",
    };
    format!("{}{arrow} {value}", name(p))
}

fn port_dir(d: PortDir) -> &'static str {
    match d {
        PortDir::Input => "input",
        PortDir::Output => "output",
        PortDir::Inout => "inout",
    }
}

/// The arguments of a `dff` after its data: clock, enable, resets, the
/// enable's priority over the synchronous reset and the power-on value.
#[allow(clippy::too_many_arguments)]
fn dff_args(
    clk: &str,
    posedge: bool,
    en: Option<String>,
    srst: Option<(String, String)>,
    en_over_srst: bool,
    arst: Option<(String, String)>,
    init: Option<String>,
) -> String {
    let mut a = format!(", clk: {} {clk}", if posedge { "posedge" } else { "negedge" });
    if let Some(e) = en {
        let _ = write!(a, ", en: {e}");
    }
    if let Some((r, v)) = srst {
        let _ = write!(a, ", srst: {r} -> {v}");
        if en_over_srst {
            a.push_str(", en_over_srst");
        }
    }
    if let Some((r, v)) = arst {
        let _ = write!(a, ", arst: {r} -> {v}");
    }
    if let Some(i) = init {
        let _ = write!(a, ", init: {i}");
    }
    a
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

// ----- word-level RTL (.rtl) -----

fn unary_op(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Not => "not",
        UnaryOp::Neg => "neg",
        UnaryOp::ReduceAnd => "reduce_and",
        UnaryOp::ReduceOr => "reduce_or",
        UnaryOp::ReduceXor => "reduce_xor",
        UnaryOp::ReduceXnor => "reduce_xnor",
        UnaryOp::LogicNot => "logic_not",
    }
}

fn binary_op(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        And => "and",
        Or => "or",
        Xor => "xor",
        Xnor => "xnor",
        Add => "add",
        Sub => "sub",
        Mul => "mul",
        Div => "div",
        Mod => "mod",
        Shl => "shl",
        Shr => "shr",
        Sshr => "sshr",
        Eq => "eq",
        Ne => "ne",
        Eqx => "eqx",
        Nex => "nex",
        Lt => "lt",
        Le => "le",
        Gt => "gt",
        Ge => "ge",
        LogicAnd => "logic_and",
        LogicOr => "logic_or",
    }
}

struct Rtl<'m> {
    m: &'m Module,
    names: Vec<String>,
    /// The wires the text refers to, declared before it.
    used: Vec<bool>,
}

impl Rtl<'_> {
    /// A signal: a wire, a slice of one, a constant, or a concatenation.
    fn sig(&mut self, s: &Sig) -> String {
        if s.is_empty() {
            return "{}".into();
        }
        // A whole wire by its name alone.
        if let SigBit::Wire(w, 0) = s.0[0]
            && s.width() as u32 == self.m.wires[w].width
            && s.iter().enumerate().all(|(k, b)| *b == SigBit::Wire(w, k as u32))
        {
            self.used[w.index()] = true;
            return name(&self.names[w.index()]);
        }
        let bits: Vec<BitRef> = s
            .iter()
            .rev()
            .map(|b| match *b {
                SigBit::Const(l) => BitRef::Const(l),
                SigBit::Wire(w, i) => {
                    self.used[w.index()] = true;
                    // A one-bit wire is not indexed.
                    let idx = (self.m.wires[w].width > 1).then_some(i);
                    BitRef::named(&self.names[w.index()], idx)
                }
            })
            .collect();
        bits_text(&bits)
    }

    fn bit(&mut self, b: SigBit) -> String {
        self.sig(&Sig::bit(b))
    }

    /// A control signal: `~x` when it is active low.
    fn control(&mut self, c: Control) -> String {
        format!("{}{}", if c.pol { "" } else { "~" }, self.bit(c.sig))
    }

    fn clock(&mut self, c: Clock) -> String {
        format!("clk: {} {}", if c.posedge { "posedge" } else { "negedge" }, self.bit(c.sig))
    }
}

/// A word-level RTL module in the `.rtl` language.
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
        let _ = writeln!(o, "    connect {l} = {rr}");
    }
    if !m.connections.is_empty() {
        o.push('\n');
    }
    for c in m.live_cells() {
        let cell = &m.cells[c];
        let label = name(&cell.name);
        let line = match &cell.kind {
            CellKind::Unary { op, a, y } => {
                let a = r.sig(a);
                format!("{} = {} {a} @{label}", r.sig(y), unary_op(*op))
            }
            CellKind::Binary { op, signed, a, b, y } => {
                let s = if *signed { ".s" } else { "" };
                let (a, b) = (r.sig(a), r.sig(b));
                format!("{} = {}{s} {a}, {b} @{label}", r.sig(y), binary_op(*op))
            }
            CellKind::Mux { a, b, s, y, .. } => {
                let (s, b, a) = (r.bit(*s), r.sig(b), r.sig(a));
                format!("{} = mux {s} ? {b} : {a} @{label}", r.sig(y))
            }
            CellKind::Pmux { default, cases, s, y } => {
                let s = r.sig(s);
                let d = r.sig(default);
                let cases: Vec<String> = cases.iter().map(|c| r.sig(c)).collect();
                format!(
                    "{} = pmux {s}, default: {d}, cases: [{}] @{label}",
                    r.sig(y),
                    cases.join(", ")
                )
            }
            CellKind::Shiftx { a, b, b_signed, offset, y } => {
                let s = if *b_signed { ".s" } else { "" };
                let (a, b) = (r.sig(a), r.sig(b));
                let off = if *offset != 0 { format!(", offset: {offset}") } else { String::new() };
                format!("{} = shiftx{s} {a}, {b}{off} @{label}", r.sig(y))
            }
            CellKind::Dff(d) => {
                let defined = d.init.bits().any(|b| matches!(b, Logic::Zero | Logic::One));
                let data = r.sig(&d.d);
                let clk = r.bit(d.clk.sig);
                let args = dff_args(
                    &clk,
                    d.clk.posedge,
                    d.en.map(|c| r.control(c)),
                    d.srst.as_ref().map(|(c, v)| (r.control(*c), logic_vec(v))),
                    d.en_over_srst,
                    d.arst.as_ref().map(|(c, v)| (r.control(*c), logic_vec(v))),
                    defined.then(|| logic_vec(&d.init)),
                );
                format!("{} = dff {data}{args} @{label}", r.sig(&d.q))
            }
            CellKind::Latch(l) => {
                let (d, en) = (r.sig(&l.d), r.control(l.en));
                format!("{} = latch {d}, en: {en} @{label}", r.sig(&l.q))
            }
            CellKind::Instance(inst) => {
                let params: Vec<String> =
                    inst.params.iter().map(|(k, v)| format!("{}: {}", name(k), attr(v))).collect();
                let conns: Vec<String> =
                    inst.conns.iter().map(|(k, d, s)| pin(k, *d, &r.sig(s))).collect();
                format!(
                    "inst {label}: {} {}({})",
                    name(&inst.module),
                    params_text(&params),
                    conns.join(", ")
                )
            }
        };
        let _ = writeln!(o, "    {line}");
    }
    for (_, mem) in m.memories.iter_enumerated() {
        if !mem.alive {
            continue;
        }
        let off = if mem.offset != 0 { format!(" offset {}", mem.offset) } else { String::new() };
        let _ =
            writeln!(o, "\n    memory {}: {} x {}{off} {{", name(&mem.name), mem.depth, mem.width);
        // The initial contents: runs of defined words, eight to a line.
        let defined = |w: &LogicVec| w.bits().any(|b| b != Logic::X);
        let mut a = 0;
        while a < mem.init.len() {
            if !defined(&mem.init[a]) {
                a += 1;
                continue;
            }
            let n = (a..mem.init.len().min(a + 8)).take_while(|&k| defined(&mem.init[k])).count();
            let words: Vec<String> = mem.init[a..a + n].iter().map(word).collect();
            let _ = writeln!(o, "        init {}: {}", a as i64 + mem.offset, words.join(", "));
            a += n;
        }
        for wp in &mem.writes {
            let clk = r.clock(wp.clk);
            let (addr, data) = (r.sig(&wp.addr), r.sig(&wp.data));
            let en = if wp.en.iter().all(|b| *b == wp.en.0[0]) {
                r.bit(wp.en.0[0])
            } else {
                r.sig(&wp.en)
            };
            let _ = writeln!(o, "        write {addr}, {data}, en: {en}, {clk}");
        }
        for rp in &mem.reads {
            let (addr, data) = (r.sig(&rp.addr), r.sig(&rp.data));
            match rp.clk {
                Some(c) => {
                    let en = if rp.en == SigBit::ONE {
                        String::new()
                    } else {
                        format!(", en: {}", r.bit(rp.en))
                    };
                    let clk = r.clock(c);
                    let _ = writeln!(o, "        {data} = read {addr}{en}, {clk}");
                }
                None => {
                    let _ = writeln!(o, "        {data} = read {addr}");
                }
            }
        }
        o.push_str("    }\n");
    }

    let mut out = format!("module {} {{\n", name(&m.name));
    for w in m.ports() {
        let wire = &m.wires[w];
        let signed = if wire.signed { " signed" } else { "" };
        let dir = port_dir(wire.port.unwrap().0);
        let _ = writeln!(out, "    {dir} {}: {}{signed}", name(&r.names[w.index()]), wire.width);
    }
    let mut first = true;
    for (w, wire) in m.wires.iter_enumerated() {
        if r.used[w.index()] && wire.port.is_none() {
            if first {
                out.push('\n');
                first = false;
            }
            let signed = if wire.signed { " signed" } else { "" };
            let _ = writeln!(out, "    wire {}: {}{signed}", name(&r.names[w.index()]), wire.width);
        }
    }
    if !body.is_empty() {
        out.push('\n');
    }
    out.push_str(&body);
    out.push_str("}\n");
    cap(out)
}

// ----- gate level (.gates) -----

/// A logic network (AIG, flip-flops, carry chains, instances) in the
/// `.gates` language.
pub fn aig_text(net: &LogicNet) -> String {
    let aig = &net.aig;
    // What each input of the graph is: a port bit, a flip-flop, a carry
    // chain's sum or carry out, an instance's output bit.
    let mut src: Vec<BitRef> = vec![BitRef::Const(Logic::X); aig.inputs.len()];
    let idx = |bits: usize, i: usize| (bits > 1).then_some(i as u32);
    let mut pad: HashMap<u32, BitRef> = HashMap::new();
    for p in &net.ports {
        for (i, b) in p.bits.iter().enumerate() {
            match b {
                Bit::Src(s) => src[*s as usize] = BitRef::named(&p.name, idx(p.bits.len(), i)),
                Bit::Pad(k) => {
                    pad.insert(*k, BitRef::named(&p.name, idx(p.bits.len(), i)));
                }
                Bit::Snk(_) => {}
            }
        }
    }
    // Flip-flops: one bit each, named after the cell they come from and
    // their place in the network.
    let ff_names: Vec<String> =
        net.ffs.iter().enumerate().map(|(i, ff)| format!("{}$ff{i}", ff.name)).collect();
    for (ff, n) in net.ffs.iter().zip(&ff_names) {
        src[ff.q as usize] = BitRef::named(n, None);
    }
    for (ci, c) in net.carries.iter().enumerate() {
        for (i, s) in c.sum.iter().enumerate() {
            src[*s as usize] = BitRef::named(&format!("carry{ci}.s"), Some(i as u32));
        }
        if let Some(co) = c.cout {
            src[co as usize] = BitRef::named(&format!("carry{ci}.cout"), None);
        }
    }
    for inst in &net.instances {
        for (p, _, bits) in &inst.conns {
            for (i, b) in bits.iter().enumerate() {
                if let Bit::Src(s) = b {
                    src[*s as usize] =
                        BitRef::named(&format!("{}.{p}", inst.name), idx(bits.len(), i));
                }
            }
        }
    }
    let lit = |l: Lit| -> BitRef {
        match aig.nodes[l.node() as usize] {
            Node::Const => BitRef::Const(Logic::Zero),
            Node::Input(i) => src[i as usize].clone(),
            Node::And(..) => BitRef::named(&format!("$g{}", l.node()), None),
        }
        .not(l.is_compl())
    };
    let sink = |k: usize| bits_text(&[lit(aig.outputs[k])]);
    let sinks = |ks: &[usize]| {
        bits_text(&ks.iter().rev().map(|&k| lit(aig.outputs[k])).collect::<Vec<_>>())
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

    let mut o = format!("module {} {{\n", name(&net.name));
    for p in &net.ports {
        let _ = writeln!(o, "    {} {}: {}", port_dir(p.dir), name(&p.name), p.bits.len());
    }
    let _ = writeln!(
        o,
        "\n    // {ands} AND gates, {} flip-flops, {} carry chains, {} instances",
        net.ffs.len(),
        net.carries.len(),
        net.instances.len()
    );
    for (i, n) in aig.nodes.iter().enumerate() {
        if let Node::And(a, b) = *n
            && live[i]
        {
            let (a, b) = (bits_text(&[lit(a)]), bits_text(&[lit(b)]));
            let _ = writeln!(o, "    $g{i} = and {a}, {b}");
        }
        if o.len() > MAX_TEXT {
            break;
        }
    }
    let mut outs = String::new();
    for p in &net.ports {
        for (i, b) in p.bits.iter().enumerate() {
            if let Bit::Snk(k) = b {
                let port = bits_text(&[BitRef::named(&p.name, idx(p.bits.len(), i))]);
                let _ = writeln!(outs, "    connect {port} = {}", sink(*k));
            }
        }
    }
    if !outs.is_empty() {
        o.push('\n');
        o.push_str(&outs);
    }
    if !net.ffs.is_empty() {
        o.push('\n');
    }
    let bit = |v: bool| sized(&[if v { Logic::One } else { Logic::Zero }]);
    for (ff, n) in net.ffs.iter().zip(&ff_names) {
        let init = match ff.init {
            Logic::Zero | Logic::One => Some(sized(&[ff.init])),
            _ => None,
        };
        let args = dff_args(
            &sink(ff.clk),
            ff.posedge,
            ff.en.map(sink),
            ff.srst.map(|(k, v)| (sink(k), bit(v))),
            ff.en_over_srst,
            ff.arst.map(|(k, v)| (sink(k), bit(v))),
            init,
        );
        let _ = writeln!(o, "    {} = dff {}{args}", name(n), sink(ff.d));
        if o.len() > MAX_TEXT {
            break;
        }
    }
    if !net.carries.is_empty() {
        o.push('\n');
    }
    for (ci, c) in net.carries.iter().enumerate() {
        let (a, b, cin) = (sinks(&c.a), sinks(&c.b), sink(c.cin));
        let _ = writeln!(o, "    carry{ci} = carry {a}, {b}, cin: {cin}");
    }
    if !net.instances.is_empty() {
        o.push('\n');
    }
    for inst in &net.instances {
        let params: Vec<String> =
            inst.params.iter().map(|(k, v)| format!("{}: {}", name(k), attr(v))).collect();
        let conns: Vec<String> = inst
            .conns
            .iter()
            .map(|(p, dir, bits)| {
                let refs: Vec<BitRef> = bits
                    .iter()
                    .rev()
                    .map(|b| match b {
                        Bit::Src(s) => src[*s as usize].clone(),
                        Bit::Snk(k) => lit(aig.outputs[*k]),
                        Bit::Pad(k) => pad.get(k).cloned().unwrap_or(BitRef::Const(Logic::Z)),
                    })
                    .collect();
                pin(p, *dir, &bits_text(&refs))
            })
            .collect();
        let _ = writeln!(
            o,
            "    inst {}: {} {}({})",
            name(&inst.name),
            name(&inst.module),
            params_text(&params),
            conns.join(", ")
        );
    }
    o.push_str("}\n");
    cap(o)
}

// ----- netlists of primitives (.netlist) -----

/// A parameter: bit vectors as sized hexadecimal constants (all their
/// digits up to 64 bits, without leading zeros beyond), or binary ones when
/// their width is not a multiple of four; integers and strings as they are.
fn param(p: &Param, key: &str) -> String {
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
            let defined = v.iter().all(|b| matches!(b, Logic::Zero | Logic::One));
            if defined && v.len().is_multiple_of(4) {
                format!("{}'h{}", v.len(), hex(&v, v.len() <= 64))
            } else if defined && v.len() <= 64 {
                let digits: String =
                    v.iter().rev().map(|b| if *b == Logic::One { '1' } else { '0' }).collect();
                format!("{}'b{digits}", v.len())
            } else {
                sized(&v)
            }
        }
        Param::Int(v) if key == "LUT_INIT" => format!("16'h{v:04X}"),
        Param::Int(v) => v.to_string(),
        Param::Str(s) => format!("{s:?}"),
    }
}

/// `base[i]` as (`base`, Some(i)); other names as (`name`, None).
fn split_index(s: &str) -> (&str, Option<u32>) {
    if let Some((b, r)) = s.split_once('[')
        && let Some(i) = r.strip_suffix(']').and_then(|i| i.parse().ok())
    {
        return (b, Some(i));
    }
    (s, None)
}

/// A netlist of primitives (mapped or packed) in the `.netlist` language.
pub fn netlist_text(nl: &Netlist) -> String {
    let net_names = unique(nl.nets.iter().map(|n| nl.str(n.name)));
    let net = |n: synth_netlist::NetId| -> String {
        match nl.nets[n].constant {
            Some(Const::Zero) => "1'd0".into(),
            Some(Const::One) => "1'd1".into(),
            Some(Const::Undef) => "1'bx".into(),
            None => name(&net_names[n.index()]),
        }
    };
    let top = nl.top.map_or("top", |t| nl.str(t));
    // Ports by their base name: direction, highest bit, split into bits.
    let mut ports: Vec<(&str, &str, u32, bool)> = Vec::new();
    let mut connects: Vec<(&str, &str, Option<u32>, String)> = Vec::new();
    let mut cells = String::new();
    let mut count = 0;
    for c in nl.live_cells() {
        let cell = &nl.cells[c];
        let kind = nl.cell_kind(c);
        let cname = nl.str(cell.name);
        if matches!(kind, INPUT_PORT | OUTPUT_PORT | INOUT_PORT) {
            let dir = match kind {
                INPUT_PORT => "input",
                OUTPUT_PORT => "output",
                _ => "inout",
            };
            let (base, i) = split_index(cname);
            match ports.iter_mut().find(|p| p.1 == base) {
                Some(p) => p.2 = p.2.max(i.unwrap_or(0)),
                None => ports.push((dir, base, i.unwrap_or(0), i.is_some())),
            }
            if let Some(n) = cell.pins.first().and_then(|p| p.net)
                && net_names[n.index()] != cname
            {
                connects.push((kind, base, i, net(n)));
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
                format!("{}: {}", name(k), param(v, k))
            })
            .collect();
        // Pins `P[i]` grouped into buses, in the order of their first bit.
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
                .insert(i.unwrap_or(0), net(n));
        }
        let pins: Vec<String> = order
            .iter()
            .map(|base| {
                let (dir, bits) = &buses[base];
                let top = bits.keys().max().copied().unwrap_or(0);
                let value = if top == 0 {
                    bits[&0].clone()
                } else {
                    let parts: Vec<&str> =
                        (0..=top).rev().map(|i| bits.get(&i).map_or("_", |n| n.as_str())).collect();
                    format!("{{{}}}", parts.join(", "))
                };
                let dir = match dir {
                    Dir::Input => PortDir::Input,
                    Dir::Output => PortDir::Output,
                    Dir::Inout => PortDir::Inout,
                };
                pin(base, dir, &value)
            })
            .collect();
        let _ = writeln!(
            cells,
            "    cell {}: {} {}({})",
            name(cname),
            name(kind),
            params_text(&params),
            pins.join(", ")
        );
    }
    let mut o = format!("module {} {{\n", name(top));
    for (dir, base, top, _) in &ports {
        let _ = writeln!(o, "    {dir} {}: {}", name(base), top + 1);
    }
    if !ports.is_empty() {
        o.push('\n');
    }
    let _ = writeln!(o, "    // {count} cells");
    if !connects.is_empty() {
        o.push('\n');
    }
    for (kind, base, i, n) in connects {
        let split = ports.iter().find(|p| p.1 == base).is_some_and(|p| p.3);
        let port = bits_text(&[BitRef::named(base, if split { i } else { None })]);
        let _ = match kind {
            INPUT_PORT => writeln!(o, "    connect {n} = {port}"),
            _ => writeln!(o, "    connect {port} = {n}"),
        };
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
    fn names() {
        assert_eq!(name("uart.$n20"), "uart.$n20");
        assert_eq!(name("count[0]$lc"), "\"count[0]$lc\"");
        assert_eq!(name("wire"), "\"wire\"");
        assert_eq!(name("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn constants() {
        use Logic::{One as I, X, Zero as O};
        assert_eq!(sized(&[I, O, I]), "3'd5");
        assert_eq!(sized(&[X, I]), "2'b1x");
        let wide: Vec<Logic> = (0..72).map(|i| if i == 4 { I } else { O }).collect();
        assert_eq!(sized(&wide), "72'h10");
    }

    #[test]
    fn bit_runs() {
        let n = |b: &str, i: u32| BitRef::named(b, Some(i));
        assert_eq!(bits_text(&[n("a", 3), n("a", 2), n("a", 1)]), "a[3:1]");
        assert_eq!(
            bits_text(&[n("a", 3), n("a", 2).not(true), BitRef::named("b", None), n("a", 0)]),
            "{a[3], ~a[2], b, a[0]}"
        );
        assert_eq!(
            bits_text(&[BitRef::Const(Logic::One), BitRef::Const(Logic::Zero), n("x[0]", 1)]),
            "{2'd2, \"x[0]\"[1]}"
        );
    }
}
