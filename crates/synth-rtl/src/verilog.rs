//! Writes an RTL module as a Verilog-2005 netlist.
//!
//! The output is meant for simulation with standard tools (differential
//! testing of the frontend and of RTL passes), not for readability.

use std::fmt::Write as _;

use synth_core::{Logic, LogicVec};

use crate::{Attr, BinaryOp, CellKind, Module, PortDir, Sig, SigBit, UnaryOp};

/// Escaped Verilog identifier.
fn id(name: &str) -> String {
    if !name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !is_keyword(name)
    {
        name.to_string()
    } else {
        format!("\\{name} ")
    }
}

fn is_keyword(s: &str) -> bool {
    matches!(
        s,
        "module"
            | "endmodule"
            | "input"
            | "output"
            | "inout"
            | "wire"
            | "reg"
            | "assign"
            | "always"
            | "begin"
            | "end"
            | "if"
            | "else"
            | "case"
            | "endcase"
            | "default"
            | "initial"
            | "integer"
            | "for"
            | "posedge"
            | "negedge"
            | "or"
            | "and"
            | "not"
            | "xor"
            | "buf"
            | "nand"
            | "nor"
            | "xnor"
            | "parameter"
            | "localparam"
            | "function"
            | "endfunction"
            | "task"
            | "endtask"
            | "generate"
            | "endgenerate"
            | "genvar"
            | "signed"
            | "logic"
    )
}

fn konst(v: &LogicVec) -> String {
    format!("{}'b{}", v.width(), v.to_bin_string())
}

/// Renders a signal as a Verilog expression (a concatenation, MSB first).
fn sig(m: &Module, s: &Sig) -> String {
    if s.is_empty() {
        return "1'b0".into();
    }
    if let Some(c) = s.as_const() {
        return konst(&c);
    }
    // Group runs of consecutive bits of the same wire.
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    let bits = &s.0;
    while i < bits.len() {
        match bits[i] {
            SigBit::Const(_) => {
                let mut j = i;
                let mut v = Vec::new();
                while j < bits.len() {
                    if let SigBit::Const(l) = bits[j] {
                        v.push(l);
                        j += 1;
                    } else {
                        break;
                    }
                }
                parts.push(konst(&LogicVec::from_bits(&v)));
                i = j;
            }
            SigBit::Wire(w, b) => {
                let mut j = i + 1;
                while j < bits.len() && bits[j] == SigBit::Wire(w, b + (j - i) as u32) {
                    j += 1;
                }
                let n = (j - i) as u32;
                let wire = &m.wires[w];
                let name = id(&wire.name);
                parts.push(if wire.width == 1 || n == wire.width {
                    name
                } else if n == 1 {
                    format!("{name}[{b}]")
                } else {
                    format!("{name}[{}:{b}]", b + n - 1)
                });
                i = j;
            }
        }
    }
    if parts.len() == 1 {
        parts.pop().unwrap()
    } else {
        parts.reverse();
        format!("{{{}}}", parts.join(", "))
    }
}

fn bit(m: &Module, b: SigBit) -> String {
    sig(m, &Sig::bit(b))
}

fn attr(a: &Attr) -> String {
    match a {
        Attr::Const(v) => konst(v),
        Attr::Str(s) => format!("{s:?}"),
    }
}

/// Writes `m` as a Verilog module.
pub fn write(m: &Module) -> String {
    let mut o = String::new();
    let ports = m.ports();
    let _ = writeln!(
        o,
        "module {}({});",
        id(&m.name),
        ports.iter().map(|&p| id(&m.wires[p].name)).collect::<Vec<_>>().join(", ")
    );
    for (w, wire) in m.wires.iter_enumerated() {
        let range = if wire.width > 1 { format!("[{}:0] ", wire.width - 1) } else { String::new() };
        let dir = match wire.port {
            Some((PortDir::Input, _)) => "input ",
            Some((PortDir::Output, _)) => "output ",
            Some((PortDir::Inout, _)) => "inout ",
            None => "",
        };
        let _ = writeln!(o, "  {dir}wire {range}{}; // {}", id(&wire.name), w.index());
    }
    for (lhs, rhs) in &m.connections {
        let _ = writeln!(o, "  assign {} = {};", sig(m, lhs), sig(m, rhs));
    }
    for (ci, c) in m.cells.iter_enumerated().filter(|(_, c)| c.alive) {
        match &c.kind {
            CellKind::Unary { op, a, y } => {
                let a = sig(m, a);
                let e = match op {
                    UnaryOp::Not => format!("~{a}"),
                    UnaryOp::Neg => format!("-{a}"),
                    UnaryOp::ReduceAnd => format!("&{a}"),
                    UnaryOp::ReduceOr => format!("|{a}"),
                    UnaryOp::ReduceXor => format!("^{a}"),
                    UnaryOp::ReduceXnor => format!("~^{a}"),
                    UnaryOp::LogicNot => format!("!{a}"),
                };
                let _ = writeln!(o, "  assign {} = {e};", sig(m, y));
            }
            CellKind::Binary { op, signed, a, b, y } => {
                let (sa, sb) = (sig(m, a), sig(m, b));
                let (a, b) = if *signed && !op.is_shift() {
                    (format!("$signed({sa})"), format!("$signed({sb})"))
                } else if *op == BinaryOp::Sshr {
                    (format!("$signed({sa})"), sb)
                } else {
                    (sa, sb)
                };
                let opstr = match op {
                    BinaryOp::And => "&",
                    BinaryOp::Or => "|",
                    BinaryOp::Xor => "^",
                    BinaryOp::Xnor => "~^",
                    BinaryOp::Add => "+",
                    BinaryOp::Sub => "-",
                    BinaryOp::Mul => "*",
                    BinaryOp::Div => "/",
                    BinaryOp::Mod => "%",
                    BinaryOp::Shl => "<<",
                    BinaryOp::Shr => ">>",
                    BinaryOp::Sshr => ">>>",
                    BinaryOp::Eq => "==",
                    BinaryOp::Ne => "!=",
                    BinaryOp::Eqx => "===",
                    BinaryOp::Nex => "!==",
                    BinaryOp::Lt => "<",
                    BinaryOp::Le => "<=",
                    BinaryOp::Gt => ">",
                    BinaryOp::Ge => ">=",
                    BinaryOp::LogicAnd => "&&",
                    BinaryOp::LogicOr => "||",
                };
                let _ = writeln!(o, "  assign {} = {a} {opstr} {b};", sig(m, y));
            }
            CellKind::Mux { a, b, s, y } => {
                // `if` semantics (an unknown select picks `a`) match the
                // procedural code muxes usually come from, which keeps
                // differential simulation free of x-pessimism artefacts.
                let r = format!("\\$m{} ", ci.index());
                let _ = writeln!(o, "  reg [{}:0] {r};", y.width() - 1);
                let _ = writeln!(
                    o,
                    "  always_comb if ({}) {r} = {}; else {r} = {};",
                    bit(m, *s),
                    sig(m, b),
                    sig(m, a)
                );
                let _ = writeln!(o, "  assign {} = {r};", sig(m, y));
            }
            CellKind::Pmux { default, cases, s, y } => {
                let mut e = sig(m, default);
                for (i, c) in cases.iter().enumerate().rev() {
                    e = format!("{} ? {} : ({e})", bit(m, s.0[i]), sig(m, c));
                }
                let _ = writeln!(o, "  assign {} = {e};", sig(m, y));
            }
            CellKind::Shiftx { a, b, b_signed, offset, y } => {
                let tmp = format!("\\$shx{} ", ci.index());
                let aw = a.width();
                let _ = writeln!(o, "  wire [{}:0] {tmp} = {};", aw.max(1) - 1, sig(m, a));
                let idx = if *b_signed { format!("$signed({})", sig(m, b)) } else { sig(m, b) };
                let _ = writeln!(
                    o,
                    "  assign {} = {tmp}[({idx}) + ({offset}) +: {}];",
                    sig(m, y),
                    y.width()
                );
            }
            CellKind::Dff(d) => {
                let r = format!("\\$q{} ", ci.index());
                let w = d.q.width();
                let _ = writeln!(o, "  reg [{}:0] {r} = {};", w - 1, konst(&d.init));
                let _ = writeln!(o, "  assign {} = {r};", sig(m, &d.q));
                let edge = |posedge: bool| if posedge { "posedge" } else { "negedge" };
                let mut sens = format!("{} {}", edge(d.clk.posedge), bit(m, d.clk.sig));
                if let Some((c, _)) = &d.arst {
                    let _ = write!(sens, " or {} {}", edge(c.pol), bit(m, c.sig));
                }
                let _ = writeln!(o, "  always @({sens}) begin");
                let mut prefix = "    ";
                let mut chain = String::new();
                if let Some((c, v)) = &d.arst {
                    let _ = writeln!(
                        chain,
                        "{prefix}if ({} == 1'b{}) {r} <= {};",
                        bit(m, c.sig),
                        c.pol as u8,
                        konst(v)
                    );
                    prefix = "    else ";
                }
                if let Some((c, v)) = &d.srst {
                    let _ = writeln!(
                        chain,
                        "{prefix}if ({} == 1'b{}) {r} <= {};",
                        bit(m, c.sig),
                        c.pol as u8,
                        konst(v)
                    );
                    prefix = "    else ";
                }
                if let Some(c) = &d.en {
                    let _ = writeln!(
                        chain,
                        "{prefix}if ({} == 1'b{}) {r} <= {};",
                        bit(m, c.sig),
                        c.pol as u8,
                        sig(m, &d.d)
                    );
                } else {
                    let _ = writeln!(chain, "{prefix}{r} <= {};", sig(m, &d.d));
                }
                o.push_str(&chain);
                let _ = writeln!(o, "  end");
            }
            CellKind::Latch(l) => {
                let r = format!("\\$l{} ", ci.index());
                let _ = writeln!(o, "  reg [{}:0] {r};", l.q.width() - 1);
                let _ = writeln!(o, "  assign {} = {r};", sig(m, &l.q));
                let _ = writeln!(
                    o,
                    "  always_latch if ({} == 1'b{}) {r} = {};",
                    bit(m, l.en.sig),
                    l.en.pol as u8,
                    sig(m, &l.d)
                );
            }
            CellKind::Instance(inst) => {
                let params = if inst.params.is_empty() {
                    String::new()
                } else {
                    format!(
                        " #({})",
                        inst.params
                            .iter()
                            .map(|(k, v)| format!(".{}({})", id(k), attr(v)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                let conns: Vec<String> = inst
                    .conns
                    .iter()
                    .map(|(p, _, s)| format!(".{}({})", id(p), sig(m, s)))
                    .collect();
                let _ = writeln!(
                    o,
                    "  {}{params} {} ({});",
                    id(&inst.module),
                    id(&c.name),
                    conns.join(", ")
                );
            }
        }
    }
    for (mi, mem) in m.memories.iter_enumerated().filter(|(_, m)| m.alive) {
        let name = format!("\\$mem{}_{} ", mi.index(), mem.name);
        let _ = writeln!(
            o,
            "  reg [{}:0] {name} [{}:{}];",
            mem.width - 1,
            mem.offset,
            mem.offset + mem.depth as i64 - 1
        );
        let mut inits = String::new();
        for (i, v) in mem.init.iter().enumerate() {
            if v.bits().any(|b| b != Logic::X) {
                let _ = writeln!(inits, "    {name}[{}] = {};", mem.offset + i as i64, konst(v));
            }
        }
        if !inits.is_empty() {
            let _ = writeln!(o, "  initial begin\n{inits}  end");
        }
        for (pi, rd) in mem.reads.iter().enumerate() {
            match rd.clk {
                None => {
                    let _ = writeln!(
                        o,
                        "  assign {} = {name}[{}];",
                        sig(m, &rd.data),
                        sig(m, &rd.addr)
                    );
                }
                Some(clk) => {
                    let r = format!("\\$rd{}_{pi} ", mi.index());
                    let _ = writeln!(o, "  reg [{}:0] {r};", mem.width - 1);
                    let _ = writeln!(o, "  assign {} = {r};", sig(m, &rd.data));
                    let _ = writeln!(
                        o,
                        "  always @({} {}) if ({}) {r} <= {name}[{}];",
                        if clk.posedge { "posedge" } else { "negedge" },
                        bit(m, clk.sig),
                        bit(m, rd.en),
                        sig(m, &rd.addr)
                    );
                }
            }
        }
        for wr in &mem.writes {
            let _ = writeln!(
                o,
                "  always @({} {}) begin : \\$wr{}_{} ",
                if wr.clk.posedge { "posedge" } else { "negedge" },
                bit(m, wr.clk.sig),
                mi.index(),
                o.len()
            );
            for i in 0..mem.width as usize {
                let _ = writeln!(
                    o,
                    "    if ({}) {name}[{}][{i}] <= {};",
                    bit(m, wr.en.0[i]),
                    sig(m, &wr.addr),
                    bit(m, wr.data.0[i])
                );
            }
            let _ = writeln!(o, "  end");
        }
    }
    o.push_str("endmodule\n");
    o
}
