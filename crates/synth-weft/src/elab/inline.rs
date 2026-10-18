//! Extern modules defined in Verilog: elaborated by the Verilog frontend and
//! inlined into the design.

use std::collections::HashMap;

use synth_core::{Diagnostic, Logic, LogicVec, Span};
use synth_rtl::{Attr, CellKind, Module, PortDir, Sig, SigBit, WireId};

use super::*;

fn string_bits(s: &str) -> LogicVec {
    let bits: Vec<Logic> = s
        .bytes()
        .rev()
        .flat_map(|b| (0..8).map(move |i| Logic::from_bool(b >> i & 1 == 1)))
        .collect();
    LogicVec::from_bits(&bits)
}

pub(crate) fn verilog_instance(
    d: &mut Design<'_>,
    m: &ast::Module,
    iname: &str,
    params: Vec<(String, Attr)>,
    conns: Vec<(String, PortDir, Sig)>,
    span: Span,
) -> R<()> {
    let name = &m.name.name;
    let Some(src) = d.verilog else {
        return d.err(
            Diagnostic::error(span, format!("no Verilog definition for extern module `{name}`"))
                .with_label("instantiated here")
                .with_secondary(m.name.span, "declared `extern` here")
                .with_help("pass the Verilog file that defines it on the command line"),
        );
    };
    if !src.modules.iter().any(|vm| vm.name.name == *name) {
        let names: Vec<&str> = src.modules.iter().map(|vm| vm.name.name.as_str()).collect();
        let mut dg = Diagnostic::error(
            span,
            format!("no Verilog module `{name}` among the Verilog sources"),
        )
        .with_label("instantiated here")
        .with_secondary(m.name.span, "declared `extern` here");
        if let Some(s) = suggest(name, names) {
            dg = dg.with_help(format!("did you mean `{s}`?"));
        } else {
            dg = dg.with_help("pass the Verilog file that defines it on the command line");
        }
        return d.err(dg);
    }
    let top_params = params
        .into_iter()
        .map(|(k, v)| {
            let lv = match v {
                Attr::Const(c) => c,
                Attr::Str(s) => string_bits(&s),
            };
            (k, lv)
        })
        .collect();
    let opts = synth_elab::ElabOptions { top: Some(name.clone()), top_params };
    let (child, diags) = synth_elab::elaborate(src, d.sm, &opts, &synth_elab::NoBlackBoxes);
    for dg in diags.items {
        d.emit(dg);
    }
    let Some(child) = child else {
        return d.err(
            Diagnostic::error(span, format!("the Verilog module `{name}` failed to elaborate"))
                .with_label("instantiated here"),
        );
    };
    // Port wires of the child by name.
    let ports: HashMap<String, WireId> =
        child.ports().into_iter().map(|w| (child.wires[w].name.clone(), w)).collect();
    let mut map: HashMap<SigBit, SigBit> = HashMap::new();
    let mut ok = true;
    for (pname, _, sig) in &conns {
        let Some(&w) = ports.get(pname) else {
            let mut dg = Diagnostic::error(
                m.name.span,
                format!("Verilog module `{name}` has no port `{pname}`"),
            )
            .with_label("declared in the extern");
            if let Some(s) = suggest(pname, ports.keys().map(|s| s.as_str())) {
                dg = dg.with_help(format!("did you mean `{s}`?"));
            }
            d.emit(dg);
            ok = false;
            continue;
        };
        let vw = child.wires[w].width as usize;
        if vw != sig.width() {
            d.emit(
                Diagnostic::error(m.name.span, format!("port `{pname}` of `{name}` is {vw} bits wide in Verilog, but {} in the extern declaration", sig.width()))
                    .with_label("width mismatch")
                    .with_help(format!("declare it as `{}` in the extern", if vw == 1 { "bool".to_string() } else { format!("uint<{vw}>") })),
            );
            ok = false;
            continue;
        }
        for i in 0..vw {
            map.insert(SigBit::Wire(w, i as u32), sig.0[i]);
        }
    }
    for (pname, w) in &ports {
        if !conns.iter().any(|(n, _, _)| n == pname) {
            let dir = child.wires[*w].port.unwrap().0;
            if dir == PortDir::Input {
                d.emit(
                    Diagnostic::error(m.name.span, format!("input `{pname}` of Verilog module `{name}` is missing from the extern declaration"))
                        .with_label("incomplete declaration")
                        .with_help(format!("add `in {pname}: ...` (with a default value if it may stay unconnected)")),
                );
                ok = false;
            }
        }
    }
    if !ok {
        return Err(());
    }
    inline(&mut d.out, &child, iname, &map);
    Ok(())
}

/// Copies `child` into `out`, with its non-port wires renamed under `prefix`
/// and port bits replaced through `map`.
pub(crate) fn inline(
    out: &mut Module,
    child: &Module,
    prefix: &str,
    ports: &HashMap<SigBit, SigBit>,
) {
    let mut wmap: HashMap<WireId, WireId> = HashMap::new();
    for (wi, w) in child.wires.iter_enumerated() {
        if w.port.is_some() {
            continue;
        }
        let nw = out.add_wire(format!("{prefix}.{}", w.name), w.width);
        out.wires[nw].public = w.public;
        out.wires[nw].signed = w.signed;
        out.wires[nw].attrs = w.attrs.clone();
        wmap.insert(wi, nw);
    }
    let f = |b: SigBit| -> SigBit {
        match b {
            SigBit::Const(_) => b,
            SigBit::Wire(w, i) => match wmap.get(&w) {
                Some(&nw) => SigBit::Wire(nw, i),
                // An unconnected output port.
                None => ports.get(&b).copied().unwrap_or(SigBit::X),
            },
        }
    };
    let sig = |s: &Sig| -> Sig { s.iter().map(|&b| f(b)).collect() };
    for c in child.cells.iter().filter(|c| c.alive) {
        let mut kind = c.kind.clone();
        remap(&mut kind, &sig, &f);
        out.add_cell(format!("{prefix}.{}", c.name), kind);
    }
    for mem in child.memories.iter().filter(|m| m.alive) {
        let mut nm = mem.clone();
        nm.name = format!("{prefix}.{}", mem.name);
        for r in &mut nm.reads {
            if let Some(c) = &mut r.clk {
                c.sig = f(c.sig);
            }
            r.en = f(r.en);
            r.addr = sig(&r.addr);
            r.data = sig(&r.data);
        }
        for w in &mut nm.writes {
            w.clk.sig = f(w.clk.sig);
            w.en = sig(&w.en);
            w.addr = sig(&w.addr);
            w.data = sig(&w.data);
        }
        out.memories.push(nm);
    }
    for (l, r) in &child.connections {
        out.connect(sig(l), sig(r));
    }
}

fn remap(k: &mut CellKind, sig: &impl Fn(&Sig) -> Sig, f: &impl Fn(SigBit) -> SigBit) {
    match k {
        CellKind::Unary { a, y, .. } => {
            *a = sig(a);
            *y = sig(y);
        }
        CellKind::Binary { a, b, y, .. } => {
            *a = sig(a);
            *b = sig(b);
            *y = sig(y);
        }
        CellKind::Mux { a, b, s, y, .. } => {
            *a = sig(a);
            *b = sig(b);
            *s = f(*s);
            *y = sig(y);
        }
        CellKind::Pmux { default, cases, s, y } => {
            *default = sig(default);
            for c in cases.iter_mut() {
                *c = sig(c);
            }
            *s = sig(s);
            *y = sig(y);
        }
        CellKind::Shiftx { a, b, y, .. } => {
            *a = sig(a);
            *b = sig(b);
            *y = sig(y);
        }
        CellKind::Dff(dff) => {
            dff.clk.sig = f(dff.clk.sig);
            dff.d = sig(&dff.d);
            dff.q = sig(&dff.q);
            if let Some(e) = &mut dff.en {
                e.sig = f(e.sig);
            }
            if let Some((c, _)) = &mut dff.srst {
                c.sig = f(c.sig);
            }
            if let Some((c, _)) = &mut dff.arst {
                c.sig = f(c.sig);
            }
        }
        CellKind::Latch(l) => {
            l.en.sig = f(l.en.sig);
            l.d = sig(&l.d);
            l.q = sig(&l.q);
        }
        CellKind::Instance(i) => {
            for (_, _, s) in &mut i.conns {
                *s = sig(s);
            }
        }
    }
}
