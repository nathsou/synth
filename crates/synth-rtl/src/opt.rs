//! RTL clean-up passes: connection resolution and dead-logic removal.

use synth_core::{FxHashMap, FxHashSet};

use crate::{CellId, CellKind, MemId, Module, PortDir, Sig, SigBit};

/// Resolves direct connections: every use of a connected wire bit is
/// replaced by its driver, and the connections are dropped except where an
/// output port must stay driven. Returns the number of rewritten bits.
pub fn resolve_connections(m: &mut Module) -> Result<usize, String> {
    // Map each connected wire bit to its source, following chains.
    let mut map: FxHashMap<SigBit, SigBit> = FxHashMap::default();
    for (lhs, rhs) in &m.connections {
        for (l, r) in lhs.iter().zip(rhs.iter()) {
            if l == r {
                continue;
            }
            if let SigBit::Wire(..) = l
                && let Some(prev) = map.insert(*l, *r)
                && prev != *r
            {
                return Err(format!("{} is driven twice", m.bit_name(*l)));
            }
        }
    }
    let resolve = |mut b: SigBit, map: &FxHashMap<SigBit, SigBit>| {
        let mut steps = 0;
        while let Some(&n) = map.get(&b) {
            b = n;
            steps += 1;
            if steps > 100_000 {
                break; // combinational loop through connections
            }
        }
        b
    };
    let mut count = 0;
    for cell in m.cells.iter_mut().filter(|c| c.alive) {
        cell.map_inputs(|b| {
            let r = resolve(b, &map);
            if r != b {
                count += 1;
            }
            r
        });
    }
    for mem in m.memories.iter_mut() {
        let fix = |s: &mut Sig| s.0.iter_mut().for_each(|b| *b = resolve(*b, &map));
        for rd in &mut mem.reads {
            fix(&mut rd.addr);
            let mut en = Sig::bit(rd.en);
            fix(&mut en);
            rd.en = en.0[0];
            if let Some(c) = &mut rd.clk {
                c.sig = resolve(c.sig, &map);
            }
        }
        for wr in &mut mem.writes {
            fix(&mut wr.addr);
            fix(&mut wr.data);
            fix(&mut wr.en);
            wr.clk.sig = resolve(wr.clk.sig, &map);
        }
    }
    // Keep only connections that drive output (or inout) port bits.
    let mut keep = Vec::new();
    for w in m.ports() {
        let wire = &m.wires[w];
        if matches!(wire.port, Some((PortDir::Output, _)) | Some((PortDir::Inout, _))) {
            let lhs = m.wire_sig(w);
            let rhs: Sig = lhs.iter().map(|&b| resolve(b, &map)).collect();
            let pairs: Vec<(SigBit, SigBit)> =
                lhs.iter().zip(rhs.iter()).filter(|(l, r)| l != r).map(|(l, r)| (*l, *r)).collect();
            if !pairs.is_empty() {
                keep.push((
                    pairs.iter().map(|p| p.0).collect(),
                    pairs.iter().map(|p| p.1).collect(),
                ));
            }
        }
    }
    // Remember what named internal wires were connected to.
    let mut aliases = Vec::new();
    for (w, wire) in m.wires.iter_enumerated() {
        if wire.port.is_some() || !wire.public || wire.name.starts_with('$') {
            continue;
        }
        let pairs: Vec<(SigBit, SigBit)> = m
            .wire_sig(w)
            .iter()
            .filter(|b| map.contains_key(b))
            .map(|&b| (b, resolve(b, &map)))
            .filter(|(l, r)| l != r)
            .collect();
        if !pairs.is_empty() {
            aliases
                .push((pairs.iter().map(|p| p.0).collect(), pairs.iter().map(|p| p.1).collect()));
        }
    }
    m.connections = keep;
    m.aliases.extend(aliases);
    Ok(count)
}

/// Removes cells and memories whose outputs are never used. Instances are
/// kept (they may have external effects). Returns the number removed.
pub fn remove_dead(m: &mut Module) -> usize {
    // Mark everything that can reach an output port or an instance (which
    // may have external effects), then sweep. Unlike "is any output read",
    // this also removes registers that only feed themselves.
    enum Driver {
        Cell(usize),
        MemRead(usize, usize),
    }
    let mut driver: FxHashMap<SigBit, Driver> = FxHashMap::default();
    for (ci, c) in m.cells.iter().enumerate().filter(|(_, c)| c.alive) {
        for o in c.outputs() {
            for &b in o.iter() {
                driver.insert(b, Driver::Cell(ci));
            }
        }
    }
    for (mi, mem) in m.memories.iter().enumerate().filter(|(_, m)| m.alive) {
        for (ri, rd) in mem.reads.iter().enumerate() {
            for &b in rd.data.iter() {
                driver.insert(b, Driver::MemRead(mi, ri));
            }
        }
    }
    let mut work: Vec<SigBit> = Vec::new();
    for (_, rhs) in &m.connections {
        work.extend(rhs.iter().copied());
    }
    for w in m.ports() {
        if m.wires[w].port.is_some_and(|(d, _)| d != PortDir::Input) {
            work.extend(m.wire_sig(w).iter().copied());
        }
    }
    let mut live_cell = vec![false; m.cells.len()];
    let mut live_read: Vec<Vec<bool>> =
        m.memories.iter().map(|mm| vec![false; mm.reads.len()]).collect();
    let mut live_mem = vec![false; m.memories.len()];
    for (ci, c) in m.cells.iter().enumerate() {
        if c.alive && matches!(c.kind, CellKind::Instance(_)) {
            live_cell[ci] = true;
            c.for_each_input(|b| work.push(b));
        }
    }
    let mut seen: FxHashSet<SigBit> = FxHashSet::default();
    while let Some(b) = work.pop() {
        if !matches!(b, SigBit::Wire(..)) || !seen.insert(b) {
            continue;
        }
        match driver.get(&b) {
            Some(&Driver::Cell(ci)) if !live_cell[ci] => {
                live_cell[ci] = true;
                m.cells[CellId::new(ci)].for_each_input(|b| work.push(b));
            }
            Some(&Driver::MemRead(mi, ri)) if !live_read[mi][ri] => {
                live_read[mi][ri] = true;
                let mem = &m.memories[MemId::new(mi)];
                let rd = &mem.reads[ri];
                work.extend(rd.addr.iter().copied());
                work.push(rd.en);
                if let Some(c) = rd.clk {
                    work.push(c.sig);
                }
                if !live_mem[mi] {
                    live_mem[mi] = true;
                    for wr in &mem.writes {
                        work.extend(
                            wr.addr.iter().chain(wr.data.iter()).chain(wr.en.iter()).copied(),
                        );
                        work.push(wr.clk.sig);
                    }
                }
            }
            _ => {}
        }
    }
    let mut removed = 0;
    for (ci, c) in m.cells.iter_mut().enumerate() {
        if c.alive && !live_cell[ci] {
            c.alive = false;
            removed += 1;
        }
    }
    for (mi, mem) in m.memories.iter_mut().enumerate() {
        if !mem.alive {
            continue;
        }
        let mut ri = 0;
        mem.reads.retain(|_| {
            ri += 1;
            live_read[mi][ri - 1]
        });
        if !live_mem[mi] {
            mem.alive = false;
            removed += 1;
        }
    }
    removed
}

/// Runs the clean-up passes.
pub fn clean(m: &mut Module) -> Result<(), String> {
    resolve_connections(m)?;
    remove_dead(m);
    Ok(())
}

/// Rewrites the module so that it simulates with two-valued semantics, as
/// synthesised hardware behaves: unknown constants, initial values and
/// memory contents become 0, and dynamic part-selects read 0 outside the
/// vector. Used to compare synthesis results against their RTL in
/// simulation.
pub fn two_state(m: &mut Module) {
    use synth_core::Logic;
    let fix = |b: &mut SigBit| {
        if let SigBit::Const(Logic::X | Logic::Z) = b {
            *b = SigBit::ZERO;
        }
    };
    let fix_v = |v: &mut synth_core::LogicVec| {
        let bits: Vec<Logic> =
            v.bits().map(|b| if b.is_known() { b } else { Logic::Zero }).collect();
        *v = synth_core::LogicVec::from_bits(&bits);
    };
    let mut shiftx = Vec::new();
    for (ci, c) in m.cells.iter_enumerated() {
        if c.alive && matches!(c.kind, CellKind::Shiftx { .. }) {
            shiftx.push(ci);
        }
    }
    for c in m.cells.iter_mut().filter(|c| c.alive) {
        c.map_inputs(|mut b| {
            fix(&mut b);
            b
        });
        if let CellKind::Dff(d) = &mut c.kind {
            fix_v(&mut d.init);
            if let Some((_, v)) = &mut d.srst {
                fix_v(v);
            }
            if let Some((_, v)) = &mut d.arst {
                fix_v(v);
            }
        }
    }
    for mem in m.memories.iter_mut().filter(|m| m.alive) {
        mem.init.iter_mut().for_each(fix_v);
        for rd in &mut mem.reads {
            rd.addr.0.iter_mut().for_each(fix);
            fix(&mut rd.en);
        }
        for wr in &mut mem.writes {
            wr.addr
                .0
                .iter_mut()
                .chain(wr.data.0.iter_mut())
                .chain(wr.en.0.iter_mut())
                .for_each(fix);
        }
    }
    for (_, rhs) in &mut m.connections {
        rhs.0.iter_mut().for_each(fix);
    }
    // Dynamic part-selects as zero-filling shifts.
    for ci in shiftx {
        let CellKind::Shiftx { a, b, b_signed, offset, y } = m.cells[ci].kind.clone() else {
            unreachable!()
        };
        m.cells[ci].alive = false;
        let mut b = b;
        let neg = if b_signed { b.0.pop() } else { None };
        let base: Sig = if offset >= 0 {
            a.0.iter().skip(offset as usize).copied().collect()
        } else {
            std::iter::repeat_n(SigBit::ZERO, offset.unsigned_abs() as usize)
                .chain(a.0.iter().copied())
                .collect()
        };
        let w = y.width().max(base.width()).max(1);
        let shifted = m.binary(crate::BinaryOp::Shr, false, base.extend(w, false), b);
        let mut v = Sig(shifted.0[..y.width()].to_vec());
        if let Some(n) = neg {
            v = m.mux(n, v, Sig::zeros(y.width()));
        }
        m.connect(y, v);
    }
}

/// Merges identical combinational cells (same operation and inputs; operands
/// of commutative operations in any order). Returns the number removed.
pub fn merge_identical(m: &mut Module) -> usize {
    use synth_core::FxHashMap;
    let mut total = 0;
    loop {
        let mut seen: FxHashMap<String, Sig> = FxHashMap::default();
        let mut removed = 0;
        for i in 0..m.cells.len() {
            let ci = CellId::new(i);
            let c = &m.cells[ci];
            if !c.alive {
                continue;
            }
            let (key, y) = match &c.kind {
                CellKind::Binary { op, signed, a, b, y } => {
                    let (a, b) = if op.is_commutative() && format!("{b:?}") < format!("{a:?}") {
                        (b, a)
                    } else {
                        (a, b)
                    };
                    (format!("B{op:?}{signed}{a:?}{b:?}{}", y.width()), y.clone())
                }
                CellKind::Unary { op, a, y } => (format!("U{op:?}{a:?}{}", y.width()), y.clone()),
                CellKind::Mux { a, b, s, y, ternary } => {
                    (format!("M{a:?}{b:?}{s:?}{ternary}"), y.clone())
                }
                CellKind::Pmux { default, cases, s, y } => {
                    (format!("P{default:?}{cases:?}{s:?}"), y.clone())
                }
                CellKind::Shiftx { a, b, b_signed, offset, y } => {
                    (format!("S{a:?}{b:?}{b_signed}{offset}{}", y.width()), y.clone())
                }
                _ => continue,
            };
            match seen.get(&key) {
                Some(orig) => {
                    let orig = orig.clone();
                    m.cells[ci].alive = false;
                    m.connect(y, orig);
                    removed += 1;
                }
                None => {
                    seen.insert(key, y);
                }
            }
        }
        if removed == 0 {
            return total;
        }
        total += removed;
        // Resolve the new connections so that users of merged cells match.
        let _ = resolve_connections(m);
    }
}

/// Uses of every signal bit by cells, memory ports, connections and output
/// ports.
pub fn use_counts(m: &Module) -> synth_core::FxHashMap<SigBit, u32> {
    let mut u: synth_core::FxHashMap<SigBit, u32> = Default::default();
    let mut add = |b: SigBit| *u.entry(b).or_default() += 1;
    for c in m.cells.iter().filter(|c| c.alive) {
        c.for_each_input(&mut add);
    }
    for mem in m.memories.iter().filter(|m| m.alive) {
        for rd in &mem.reads {
            rd.addr.iter().for_each(|&b| add(b));
            add(rd.en);
            if let Some(c) = rd.clk {
                add(c.sig);
            }
        }
        for wr in &mem.writes {
            wr.addr.iter().chain(wr.data.iter()).chain(wr.en.iter()).for_each(|&b| add(b));
            add(wr.clk.sig);
        }
    }
    for (_, rhs) in &m.connections {
        rhs.iter().for_each(|&b| add(b));
    }
    for w in m.ports() {
        if m.wires[w].port.is_some_and(|(d, _)| d != PortDir::Input) {
            m.wire_sig(w).iter().for_each(|&b| add(b));
        }
    }
    u
}

/// Shares arithmetic between the inputs of a multiplexer:
/// `s ? x op z : x op w` becomes `x op (s ? w : z)` (for `+ - & | ^`), and
/// `s ? x - z : x + z` one adder, `x + (z ^ s) + s`. Only when the operations
/// feed nothing else. Returns the number of multiplexers rewritten.
pub fn share_arith(m: &mut Module) -> usize {
    use crate::BinaryOp as B;
    use synth_core::FxHashMap;
    let uses = use_counts(m);
    // Cell driving exactly the whole signal `y`.
    let mut by_y: FxHashMap<Vec<SigBit>, CellId> = FxHashMap::default();
    for (ci, c) in m.cells.iter_enumerated() {
        if c.alive
            && let CellKind::Binary { y, .. } = &c.kind
        {
            by_y.insert(y.0.clone(), ci);
        }
    }
    let exclusive = |s: &Sig| s.iter().all(|b| uses.get(b).copied() == Some(1));
    let mut n = 0;
    for i in 0..m.cells.len() {
        let mi = CellId::new(i);
        let CellKind::Mux { a, b, s, y, .. } = &m.cells[mi].kind else { continue };
        if !m.cells[mi].alive {
            continue;
        }
        let (Some(&ca), Some(&cb)) = (by_y.get(&a.0), by_y.get(&b.0)) else { continue };
        if ca == cb || !m.cells[ca].alive || !m.cells[cb].alive || !exclusive(a) || !exclusive(b) {
            continue;
        }
        let (
            CellKind::Binary { op: oa, signed: sa, a: xa, b: za, .. },
            CellKind::Binary { op: ob, signed: sb, a: xb, b: zb, .. },
        ) = (&m.cells[ca].kind, &m.cells[cb].kind)
        else {
            continue;
        };
        if sa != sb
            || xa.width() != xb.width()
            || za.width() != zb.width()
            || xa.width() != y.width()
        {
            continue;
        }
        let (s, y, sa) = (*s, y.clone(), *sa);
        let (oa, ob, xa, za, xb, zb) = (*oa, *ob, xa.clone(), za.clone(), xb.clone(), zb.clone());
        let new = match (oa, ob) {
            (B::Add, B::Sub) | (B::Sub, B::Add) if xa == xb && za == zb => {
                // Subtract when the select picks the Sub input.
                let sub_sel = if ob == B::Sub { s } else { m.not1(s) };
                let w = y.width();
                let flip = Sig(vec![sub_sel; w]);
                let zz = m.binary(B::Xor, false, za, flip);
                let mut x1 = Sig::bit(SigBit::ONE);
                x1.0.extend_from_slice(&xa.0);
                let mut z1 = Sig::bit(sub_sel);
                z1.0.extend_from_slice(&zz.0);
                let sum = m.binary(B::Add, false, x1, z1);
                Sig(sum.0[1..].to_vec())
            }
            (op1, op2)
                if op1 == op2 && matches!(op1, B::Add | B::Sub | B::And | B::Or | B::Xor) =>
            {
                let commutative = op1 != B::Sub;
                let (shared, p, q) = if xa == xb {
                    (xa, za, zb)
                } else if za == zb && za.width() == xa.width() {
                    // `p op z : q op z`; for Sub the shared operand is the subtrahend.
                    if commutative {
                        (za, xa, xb)
                    } else {
                        let sel = m.mux(s, xa, xb);
                        let r = m.binary(op1, sa, sel, za);
                        m.connect(y, r);
                        m.cells[mi].alive = false;
                        m.cells[ca].alive = false;
                        m.cells[cb].alive = false;
                        n += 1;
                        continue;
                    }
                } else if commutative && xa == zb && za.width() == xa.width() {
                    (xa, za, xb)
                } else if commutative && za == xb && za.width() == xa.width() {
                    (za, xa, zb)
                } else {
                    continue;
                };
                let sel = m.mux(s, p, q);
                m.binary(op1, sa, shared, sel)
            }
            _ => continue,
        };
        m.connect(y, new);
        m.cells[mi].alive = false;
        m.cells[ca].alive = false;
        m.cells[cb].alive = false;
        n += 1;
    }
    n
}
