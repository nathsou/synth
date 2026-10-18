//! Whole-design checks on the flattened RTL: combinational cycles and
//! resets that depend on the registers they clear.

use std::collections::{HashMap, HashSet};

use synth_core::Diagnostic;
use synth_rtl::{BinaryOp, CellKind, Module, SigBit, UnaryOp};

use crate::elab::Design;

/// For each signal bit, the bits it depends on combinationally.
fn comb_graph(m: &Module) -> HashMap<SigBit, Vec<SigBit>> {
    let mut g: HashMap<SigBit, Vec<SigBit>> = HashMap::new();
    let wire = |b: &SigBit| matches!(b, SigBit::Wire(..));
    let mut add = |y: SigBit, deps: &mut dyn Iterator<Item = SigBit>| {
        if wire(&y) {
            let e = g.entry(y).or_default();
            e.extend(deps.filter(wire));
        }
    };
    for (l, r) in &m.connections {
        for (a, b) in l.iter().zip(r.iter()) {
            add(*a, &mut std::iter::once(*b));
        }
    }
    for c in m.cells.iter().filter(|c| c.alive) {
        match &c.kind {
            CellKind::Unary { op: UnaryOp::Not, a, y } => {
                for (i, yb) in y.iter().enumerate() {
                    add(*yb, &mut std::iter::once(a.0[i]));
                }
            }
            CellKind::Binary {
                op: BinaryOp::And | BinaryOp::Or | BinaryOp::Xor | BinaryOp::Xnor,
                a,
                b,
                y,
                ..
            } => {
                for (i, yb) in y.iter().enumerate() {
                    add(*yb, &mut [a.0[i], b.0[i]].into_iter());
                }
            }
            CellKind::Mux { a, b, s, y, .. } => {
                for (i, yb) in y.iter().enumerate() {
                    add(*yb, &mut [a.0[i], b.0[i], *s].into_iter());
                }
            }
            CellKind::Pmux { default, cases, s, y } => {
                for (i, yb) in y.iter().enumerate() {
                    let mut deps: Vec<SigBit> = vec![default.0[i]];
                    deps.extend(cases.iter().map(|c| c.0[i]));
                    deps.extend(s.iter().copied());
                    add(*yb, &mut deps.into_iter());
                }
            }
            CellKind::Dff(_) | CellKind::Instance(_) => {}
            other => {
                let mut ins = Vec::new();
                let tmp = synth_rtl::Cell { name: String::new(), kind: other.clone(), alive: true };
                tmp.for_each_input(|b| ins.push(b));
                for o in tmp.outputs() {
                    for yb in o.iter() {
                        add(*yb, &mut ins.iter().copied());
                    }
                }
            }
        }
    }
    for mem in m.memories.iter().filter(|m| m.alive) {
        for r in mem.reads.iter().filter(|r| r.clk.is_none()) {
            for yb in r.data.iter() {
                add(*yb, &mut r.addr.iter().copied());
            }
        }
    }
    g
}

fn bit_name(d: &Design<'_>, b: SigBit) -> Option<(String, synth_core::Span)> {
    let SigBit::Wire(w, i) = b else { return None };
    let (name, span) = d.wire_info.get(&w)?;
    if name.contains('$') {
        return None;
    }
    let width = d.out.wires[w].width;
    Some((if width == 1 { name.clone() } else { format!("{name}[{i}]") }, *span))
}

pub(crate) fn combinational_cycles(d: &mut Design<'_>) {
    let g = comb_graph(&d.out);
    // Iterative depth-first search with colours.
    let mut color: HashMap<SigBit, u8> = HashMap::new();
    let mut nodes: Vec<SigBit> = g.keys().copied().collect();
    nodes.sort();
    for start in nodes {
        if color.contains_key(&start) {
            continue;
        }
        let mut stack: Vec<(SigBit, usize)> = vec![(start, 0)];
        color.insert(start, 1);
        while let Some(&mut (n, ref mut k)) = stack.last_mut() {
            let deps = g.get(&n).map(|v| v.as_slice()).unwrap_or(&[]);
            if *k < deps.len() {
                let m = deps[*k];
                *k += 1;
                match color.get(&m) {
                    None => {
                        color.insert(m, 1);
                        stack.push((m, 0));
                    }
                    Some(1) => {
                        // Cycle: the stack from `m` to the top.
                        let pos = stack.iter().position(|(x, _)| *x == m).unwrap();
                        let cyc: Vec<SigBit> = stack[pos..].iter().map(|(x, _)| *x).collect();
                        report_cycle(d, &cyc);
                        return;
                    }
                    _ => {}
                }
            } else {
                color.insert(n, 2);
                stack.pop();
            }
        }
    }
}

fn report_cycle(d: &mut Design<'_>, cyc: &[SigBit]) {
    let mut names: Vec<(String, synth_core::Span)> = Vec::new();
    // The stack lists dependencies; reverse for the direction of flow.
    for b in cyc.iter().rev() {
        if let Some(n) = bit_name(d, *b)
            && names.last().map(|x| &x.0) != Some(&n.0)
            && !names.iter().any(|x| x.0 == n.0)
        {
            names.push(n);
        }
    }
    if names.is_empty() {
        d.emit(Diagnostic::global_error("combinational cycle between unnamed signals"));
        return;
    }
    let chain: Vec<String> = names
        .iter()
        .map(|(n, _)| format!("`{n}`"))
        .chain(std::iter::once(format!("`{}`", names[0].0)))
        .collect();
    let mut dg =
        Diagnostic::error(names[0].1, format!("combinational cycle: {}", chain.join(" → ")))
            .with_label("depends on itself without a register in between");
    for (n, sp) in names.iter().skip(1).take(4) {
        dg = dg.with_secondary(*sp, format!("`{n}` is part of the cycle"));
    }
    dg = dg.with_help("break the cycle with a register (`reg` and `:=`)");
    d.emit(dg);
}

/// A reset must not depend combinationally on a register it resets.
pub(crate) fn reset_loops(d: &mut Design<'_>) {
    let g = comb_graph(&d.out);
    let mut by_reset: HashMap<SigBit, Vec<SigBit>> = HashMap::new();
    for c in d.out.cells.iter().filter(|c| c.alive) {
        if let CellKind::Dff(f) = &c.kind {
            for (ctl, _) in f.srst.iter().chain(f.arst.iter()) {
                by_reset.entry(ctl.sig).or_default().extend(f.q.iter().copied());
            }
        }
    }
    let mut keys: Vec<SigBit> = by_reset.keys().copied().collect();
    keys.sort();
    for r in keys {
        let regs: HashSet<SigBit> = by_reset[&r].iter().copied().collect();
        let mut seen = HashSet::new();
        let mut todo = vec![r];
        while let Some(b) = todo.pop() {
            if !seen.insert(b) {
                continue;
            }
            if regs.contains(&b) {
                let reg = bit_name(d, b);
                let rst = bit_name(d, r);
                let (Some((rn, rsp)), rst) = (reg, rst) else { break };
                let mut dg = Diagnostic::error(rsp, format!("register `{rn}` controls the reset that clears it"))
                    .with_label("feeds its own domain's reset")
                    .with_help("generate resets from registers outside the domain they reset (for example `power_on_reset()` in the clock's own domain, or a register declared with `init`)");
                if let Some((n, sp)) = rst {
                    dg = dg.with_secondary(sp, format!("reset `{n}`"));
                }
                d.emit(dg);
                break;
            }
            if let Some(deps) = g.get(&b) {
                todo.extend(deps.iter().copied());
            }
        }
    }
}
