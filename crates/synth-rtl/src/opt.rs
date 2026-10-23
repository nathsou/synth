//! RTL clean-up passes: connection resolution and dead-logic removal.

use synth_core::{FxHashMap, FxHashSet};

use crate::{CellId, CellKind, MemId, Module, PortDir, Sig, SigBit};

/// Resolves direct connections: every use of a connected wire bit is
/// replaced by its driver, and the connections are dropped except where an
/// output port must stay driven. Returns the number of rewritten bits.
pub fn resolve_connections(m: &mut Module) -> Result<usize, String> {
    // Map each connected wire bit to its source, following chains.
    let mut map: FxHashMap<SigBit, SigBit> = FxHashMap::default();
    let mut twice: Vec<SigBit> = Vec::new();
    for (lhs, rhs) in &m.connections {
        for (l, r) in lhs.iter().zip(rhs.iter()) {
            if l == r {
                continue;
            }
            if let SigBit::Wire(..) = l
                && let Some(prev) = map.insert(*l, *r)
                && prev != *r
            {
                twice.push(*l);
            }
        }
    }
    // Several drivers are an error only for a live bit, one that reaches an
    // output, an instance or a memory (a loop variable shared by processes
    // is driven by each and feeds at most its own next value).
    if !twice.is_empty() {
        let mut cell_of: FxHashMap<SigBit, usize> = FxHashMap::default();
        for (ci, c) in m.cells.iter().enumerate().filter(|(_, c)| c.alive) {
            for o in c.outputs() {
                for &bit in o.iter() {
                    cell_of.insert(bit, ci);
                }
            }
        }
        let mut conn_of: FxHashMap<SigBit, Vec<SigBit>> = FxHashMap::default();
        for (lhs, rhs) in &m.connections {
            for (l, r) in lhs.iter().zip(rhs.iter()) {
                conn_of.entry(*l).or_default().push(*r);
            }
        }
        let mut work: Vec<SigBit> = Vec::new();
        for c in m.cells.iter().filter(|c| c.alive && matches!(c.kind, CellKind::Instance(_))) {
            c.for_each_input(|bit| work.push(bit));
        }
        for mem in m.memories.iter().filter(|mm| mm.alive) {
            for rd in &mem.reads {
                work.extend(rd.addr.iter().copied());
            }
            for wr in &mem.writes {
                work.extend(wr.addr.iter().chain(wr.data.iter()).chain(wr.en.iter()).copied());
            }
        }
        for w in m.ports() {
            if m.wires[w].port.is_some_and(|(d, _)| d != PortDir::Input) {
                work.extend(m.wire_sig(w).iter().copied());
            }
        }
        let mut live: FxHashSet<SigBit> = FxHashSet::default();
        let mut live_cells: FxHashSet<usize> = FxHashSet::default();
        while let Some(bit) = work.pop() {
            if !live.insert(bit) {
                continue;
            }
            if let Some(rs) = conn_of.get(&bit) {
                work.extend(rs.iter().copied());
            }
            if let Some(&ci) = cell_of.get(&bit)
                && live_cells.insert(ci)
            {
                m.cells[CellId::new(ci)].for_each_input(|x| work.push(x));
            }
        }
        if let Some(b) = twice.iter().find(|b| live.contains(b)) {
            return Err(format!("{} is driven twice", m.bit_name(*b)));
        }
        for b in &twice {
            map.remove(b);
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

/// Folds a constant added or subtracted to a sum or difference with a
/// constant: `(x ± k1) ± k2` (the inner result read by nothing else, the
/// outer as wide or narrower) becomes `x + k`, `k = ±k1 ± k2` modulo the
/// outer width. Expressions such as `i + 3 - C` otherwise take two carry
/// chains in series. Returns the number of operations folded.
pub fn fold_constant_sums(m: &mut Module) -> usize {
    use crate::BinaryOp as B;
    use synth_core::FxHashMap;
    let uses = use_counts(m);
    let mut driver: FxHashMap<SigBit, CellId> = FxHashMap::default();
    for (ci, c) in m.cells.iter_enumerated() {
        if c.alive
            && let CellKind::Binary { op: B::Add | B::Sub, y, .. } = &c.kind
        {
            for (i, &b) in y.iter().enumerate() {
                if i == 0 {
                    driver.insert(b, ci);
                }
            }
        }
    }
    // `x ± k` as (x, k, k negated), with k constant on the right (or on
    // either side of an addition).
    let split = |kind: &CellKind| -> Option<(Sig, synth_core::LogicVec, bool, Sig)> {
        let CellKind::Binary { op, a, b, y, .. } = kind else { return None };
        match (op, a.as_const(), b.as_const()) {
            (B::Add | B::Sub, None, Some(k)) => Some((a.clone(), k, *op == B::Sub, y.clone())),
            (B::Add, Some(k), None) => Some((b.clone(), k, false, y.clone())),
            _ => None,
        }
    };
    let mut n = 0;
    for ci in m.live_cells().collect::<Vec<_>>() {
        let Some((inner_y, k2, neg2, y)) = split(&m.cells[ci].kind) else { continue };
        let w = y.width();
        // The operand is the low bits of another such cell's result, read
        // only here.
        let Some(&inner) = inner_y.iter().next().and_then(|b| driver.get(b)) else { continue };
        if inner == ci || !m.cells[inner].alive {
            continue;
        }
        let Some((x, k1, neg1, iy)) = split(&m.cells[inner].kind) else { continue };
        if inner_y.width() != w || iy.iter().any(|b| uses.get(b).copied().unwrap_or(0) > 1) {
            continue;
        }
        // Either the operand is the inner result's low bits (arithmetic
        // modulo the outer width), or all of it zero-extended, from an
        // addition that cannot overflow its width.
        let wi = iy.width();
        let low = wi >= w && inner_y.0 == iy.0[..w];
        let exact = wi < w
            && inner_y.0[..wi] == iy.0[..]
            && inner_y.0[wi..].iter().all(|b| *b == SigBit::ZERO)
            && !neg1
            && eff_width(&x).max(eff_width(&Sig::from_const(&k1))) < wi;
        if !low && !exact {
            continue;
        }
        let term = |k: &synth_core::LogicVec, neg: bool| {
            let k = k.resize(w, false);
            if neg { synth_core::LogicVec::zeros(w).sub(&k) } else { k }
        };
        let k = term(&k1, neg1).add(&term(&k2, neg2));
        if !k.is_known() {
            continue;
        }
        let x = if low { x.slice(0, w) } else { x.extend(w, false) };
        let sum = m.binary(B::Add, false, x, Sig::from_const(&k));
        m.cells[ci].alive = false;
        m.connect(y, sum);
        n += 1;
    }
    n
}

/// Balances chains of running maxima or minima: `if (best < x[k]) best =
/// x[k], arg = n[k];` over k makes a chain of compare-and-select stages,
/// each comparing the previous stage's result. Comparing pairs, then the
/// winners, gives the same result in logarithmic depth: the earlier of
/// equal values wins (the later for `<=`) on both sides and at the root.
/// Multiplexers on the same selects (the `arg`s) follow. Returns the number
/// of chains rebuilt.
pub fn balance_select_chains(m: &mut Module) -> usize {
    use crate::BinaryOp as B;
    use synth_core::FxHashMap;
    const MIN_STAGES: usize = 3;
    // Narrower values compare in a LUT or two, which LUT mapping merges
    // along the chain better than across a tree (3-bit values: 5 LUT levels
    // chained, 7 balanced; 10-bit: 17 chained, 7.5 balanced).
    const MIN_WIDTH: usize = 6;
    let uses = use_counts(m);
    let mut compares: Vec<CellId> = Vec::new();
    let mut by_select: FxHashMap<SigBit, Vec<CellId>> = FxHashMap::default();
    let mut by_y: FxHashMap<Vec<SigBit>, CellId> = FxHashMap::default();
    for (ci, c) in m.cells.iter_enumerated() {
        if !c.alive {
            continue;
        }
        match &c.kind {
            CellKind::Binary { op: B::Lt | B::Le | B::Gt | B::Ge, .. } => compares.push(ci),
            CellKind::Mux { s, y, .. } => {
                by_select.entry(*s).or_default().push(ci);
                by_y.insert(y.0.clone(), ci);
            }
            _ => {}
        }
    }
    // A stage: a comparison of the running value with a new one (`prev <
    // new`, or `<=`), and the multiplexers on its result: the one taking the
    // new value when it wins (absent where nothing reads the result), and
    // the companions.
    struct Stage {
        prev: Sig,
        new: Sig,
        op: B,
        signed: bool,
        select: SigBit,
        value: Option<CellId>,
    }
    let stage = |m: &Module, cmp: CellId| -> Option<Stage> {
        let CellKind::Binary { op, signed, a: p, b: q, y } = &m.cells[cmp].kind else {
            return None;
        };
        let (prev, new, op) = match op {
            B::Lt | B::Le => (p.clone(), q.clone(), *op),
            B::Gt => (q.clone(), p.clone(), B::Lt),
            B::Ge => (q.clone(), p.clone(), B::Le),
            _ => return None,
        };
        let select = y.0[0];
        let value = by_select.get(&select).into_iter().flatten().copied().find(|&ci| {
            matches!(&m.cells[ci].kind, CellKind::Mux { a, b, .. } if *a == prev && *b == new)
        });
        Some(Stage { prev, new, op, signed: *signed, select, value })
    };
    let all: Vec<Stage> = compares.iter().filter_map(|&c| stage(m, c)).collect();
    // The stage of each value multiplexer.
    let of_value: FxHashMap<CellId, usize> =
        all.iter().enumerate().filter_map(|(i, s)| s.value.map(|v| (v, i))).collect();
    let mut continued = vec![false; all.len()];
    for st in &all {
        if let Some(&v) = by_y.get(&st.prev.0)
            && let Some(&i) = of_value.get(&v)
        {
            continued[i] = true;
        }
    }
    let exclusive = |s: &Sig, n: u32| s.iter().all(|b| uses.get(b).copied().unwrap_or(0) <= n);
    let mut n = 0;
    for end in (0..all.len()).filter(|&i| !continued[i]) {
        // Walk back while each previous value is the value multiplexer of a
        // stage of the same comparison, read only by this stage.
        let mut chain = vec![end];
        loop {
            let last = &all[*chain.last().unwrap()];
            let Some(&v) = by_y.get(&last.prev.0) else { break };
            let Some(&i) = of_value.get(&v) else { break };
            if all[i].op != last.op || all[i].signed != last.signed || !exclusive(&last.prev, 2) {
                break;
            }
            chain.push(i);
        }
        if chain.len() < MIN_STAGES || all[end].prev.width() < MIN_WIDTH {
            continue;
        }
        chain.reverse();
        let stages: Vec<&Stage> = chain.iter().map(|&i| &all[i]).collect();
        // Companions: muxes on each stage's select, chained like the values;
        // a companion continues the previous one when its previous input is
        // made of that one's result bits and constants (narrower stages are
        // widened by constants).
        let companions: Vec<Vec<(CellId, Sig, Sig, Sig)>> = stages
            .iter()
            .map(|st| {
                by_select
                    .get(&st.select)
                    .into_iter()
                    .flatten()
                    .filter(|&&ci| Some(ci) != st.value)
                    .filter_map(|&ci| match &m.cells[ci].kind {
                        CellKind::Mux { a, b, y, .. } => {
                            Some((ci, a.clone(), b.clone(), y.clone()))
                        }
                        _ => None,
                    })
                    .collect()
            })
            .collect();
        let continues = |a: &Sig, prev_y: &Sig| {
            a.iter().any(|b| prev_y.0.contains(b))
                && a.iter().all(|b| prev_y.0.contains(b) || matches!(b, SigBit::Const(_)))
        };
        let mut ok = companions.iter().all(|c| c.len() == companions[0].len());
        let mut chains: Vec<Vec<(CellId, Sig, Sig, Sig)>> = Vec::new();
        for first in &companions[0] {
            let mut list = vec![first.clone()];
            for later in &companions[1..] {
                let prev_y = &list.last().unwrap().3;
                match later.iter().find(|c| continues(&c.1, prev_y)) {
                    Some(c) if exclusive(prev_y, 1) => list.push(c.clone()),
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            chains.push(list);
        }
        if !ok {
            continue;
        }
        // Leaves: the first stage's previous value, then each stage's new.
        // A companion's value from stage k is what the later stages make of
        // it when each keeps its previous input.
        let carry = |list: &[(CellId, Sig, Sig, Sig)], k: usize, mut v: Sig| {
            for j in k + 1..list.len() {
                let (prev_y, a) = (&list[j - 1].3, &list[j].1);
                v = Sig(a
                    .iter()
                    .map(|b| prev_y.0.iter().position(|x| x == b).map_or(*b, |t| v.0[t]))
                    .collect());
            }
            v
        };
        let (op, signed) = (stages[0].op, stages[0].signed);
        let mut leaves: Vec<(Sig, Vec<Sig>)> = vec![(
            stages[0].prev.clone(),
            chains.iter().map(|c| carry(c, 0, c[0].1.clone())).collect(),
        )];
        for (k, st) in stages.iter().enumerate() {
            leaves.push((
                st.new.clone(),
                chains.iter().map(|c| carry(c, k, c[k].2.clone())).collect(),
            ));
        }
        let value_y = stages.last().unwrap().value.map(|v| match &m.cells[v].kind {
            CellKind::Mux { y, .. } => y.clone(),
            _ => unreachable!(),
        });
        let dead: Vec<CellId> = stages
            .iter()
            .filter_map(|st| st.value)
            .chain(chains.iter().flatten().map(|c| c.0))
            .collect();
        let ends: Vec<Sig> = chains.iter().map(|c| c.last().unwrap().3.clone()).collect();
        fn tree(m: &mut Module, op: B, signed: bool, items: &[(Sig, Vec<Sig>)]) -> (Sig, Vec<Sig>) {
            if items.len() == 1 {
                return items[0].clone();
            }
            let (l, r) = items.split_at(items.len() / 2);
            let (lv, lc) = tree(m, op, signed, l);
            let (rv, rc) = tree(m, op, signed, r);
            let s = m.binary(op, signed, lv.clone(), rv.clone()).0[0];
            let v = m.mux(s, lv, rv);
            let c = lc.into_iter().zip(rc).map(|(a, b)| m.mux(s, a, b)).collect();
            (v, c)
        }
        let (v, c) = tree(m, op, signed, &leaves);
        for ci in dead {
            m.cells[ci].alive = false;
        }
        if let Some(y) = value_y {
            m.connect(y, v);
        }
        for (y, cv) in ends.into_iter().zip(c) {
            m.connect(y, cv);
        }
        n += 1;
    }
    n
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

/// Rewrites priority chains of multiplexers selected by comparisons of one
/// narrow signal with constants (`s == 0 ? a : s == 3 ? b : c`, the shape of
/// `case` statements and `?:` chains) into a multiplexer tree indexed by the
/// signal's bits, which maps to far fewer LUTs. Returns the number of chains
/// rewritten.
pub fn chains_to_trees(m: &mut Module) -> usize {
    use crate::BinaryOp as B;
    use synth_core::FxHashMap;
    const MAX_SEL: usize = 6;
    let uses = use_counts(m);
    let mut driver: FxHashMap<SigBit, CellId> = FxHashMap::default();
    for (ci, c) in m.cells.iter_enumerated() {
        if c.alive {
            for o in c.outputs() {
                for &b in o.iter() {
                    driver.insert(b, ci);
                }
            }
        }
    }
    // The (signal, constant) a select bit compares, if it is such a test.
    let test = |m: &Module, s: SigBit| -> Option<(Sig, u64)> {
        let ci = *driver.get(&s)?;
        let CellKind::Binary { op: B::Eq, a, b, .. } = &m.cells[ci].kind else { return None };
        let (sig, c) = match (a.as_const(), b.as_const()) {
            (None, Some(c)) => (a.clone(), c),
            (Some(c), None) => (b.clone(), c),
            _ => return None,
        };
        if sig.width() > MAX_SEL || sig.iter().any(|b| matches!(b, SigBit::Const(_))) {
            return None;
        }
        Some((sig, c.to_u64()?))
    };
    // Chain muxes: Mux cells driving exactly their `y`.
    let mut mux_by_y: FxHashMap<Vec<SigBit>, CellId> = FxHashMap::default();
    for (ci, c) in m.cells.iter_enumerated() {
        if c.alive
            && let CellKind::Mux { y, .. } = &c.kind
        {
            mux_by_y.insert(y.0.clone(), ci);
        }
    }
    let exclusive = |s: &Sig| s.iter().all(|b| uses.get(b).copied() == Some(1));
    // Muxes that continue a chain (they are another chain mux's `a`).
    let mut inner: synth_core::FxHashSet<CellId> = Default::default();
    for c in m.cells.iter().filter(|c| c.alive) {
        if let CellKind::Mux { a, .. } = &c.kind
            && let Some(&ni) = mux_by_y.get(&a.0)
            && exclusive(a)
        {
            inner.insert(ni);
        }
    }
    let heads: Vec<CellId> = mux_by_y.values().copied().filter(|ci| !inner.contains(ci)).collect();
    let mut n = 0;
    for head in heads {
        let mut items: Vec<(u64, Sig)> = Vec::new();
        let mut sel: Option<Sig> = None;
        let mut cur = head;
        let default = loop {
            let CellKind::Mux { a, b, s, .. } = &m.cells[cur].kind else { unreachable!() };
            let Some((sig, c)) = test(m, *s) else {
                // Not a test: the chain ends above this mux.
                break m.cells[cur].outputs()[0].clone();
            };
            if sel.as_ref().is_some_and(|x| *x != sig) {
                break m.cells[cur].outputs()[0].clone();
            }
            sel = Some(sig);
            items.push((c, b.clone()));
            match mux_by_y.get(&a.0) {
                Some(&next) if exclusive(a) && next != head => cur = next,
                _ => break a.clone(),
            }
        };
        let Some(sel) = sel else { continue };
        if items.len() < 3 {
            continue;
        }
        let width = default.width();
        let leaves: Vec<Sig> = (0..1u64 << sel.width())
            .map(|v| {
                items.iter().find(|(c, _)| *c == v).map_or(default.clone(), |(_, x)| x.clone())
            })
            .collect();
        fn tree(m: &mut Module, sel: &[SigBit], leaves: &[Sig], bit: usize) -> Sig {
            if leaves.len() == 1 {
                return leaves[0].clone();
            }
            let even: Vec<Sig> = leaves.iter().step_by(2).cloned().collect();
            let odd: Vec<Sig> = leaves.iter().skip(1).step_by(2).cloned().collect();
            let a = tree(m, sel, &even, bit + 1);
            let b = tree(m, sel, &odd, bit + 1);
            m.mux(sel[bit], a, b)
        }
        let _ = width;
        let y = m.cells[head].outputs()[0].clone();
        let t = tree(m, &sel.0, &leaves, 0);
        m.cells[head].alive = false;
        m.connect(y, t);
        n += 1;
    }
    n
}

/// Effective width of an unsigned value: up to its highest bit that is not
/// constant zero.
fn eff_width(s: &Sig) -> usize {
    s.iter().rposition(|b| *b != SigBit::ZERO).map_or(0, |i| i + 1)
}

/// Width reduction of unsigned arithmetic (after Yosys's `wreduce`): result
/// bits of additions and multiplications above what the operands can reach
/// are constant zero; the cells shrink and those bits become constants.
/// Returns the number of result bits made constant.
pub fn reduce_widths(m: &mut Module) -> usize {
    let mut count = 0;
    for ci in m.live_cells().collect::<Vec<_>>() {
        let CellKind::Binary { op, signed: false, a, b, y } = &m.cells[ci].kind else { continue };
        let (ea, eb) = (eff_width(a), eff_width(b));
        let need = match op {
            crate::BinaryOp::Add => ea.max(eb) + 1,
            crate::BinaryOp::Mul => ea + eb,
            _ => continue,
        };
        let w = y.width();
        if need >= w {
            continue;
        }
        let (op, a, b, y) = (*op, a.clone(), b.clone(), y.clone());
        let narrow = |s: &Sig| Sig(s.0[..need.min(s.width())].to_vec());
        let y_lo =
            m.binary(op, false, narrow(&a).extend(need, false), narrow(&b).extend(need, false));
        m.cells[ci].alive = false;
        m.connect(Sig(y.0[..need].to_vec()), y_lo);
        m.connect(Sig(y.0[need..].to_vec()), Sig::zeros(w - need));
        count += w - need;
    }
    count
}

/// Shifts by a multiple of a constant (`a[c*i +: w]`: the shift amount is
/// `i * c`) become multiplexers over `i`, when `i` is narrow (after Yosys's
/// `peepopt` shiftmul). Returns the number of shifts rewritten.
pub fn shift_by_multiple(m: &mut Module) -> usize {
    let mut mul_of: FxHashMap<SigBit, CellId> = FxHashMap::default();
    for ci in m.live_cells() {
        if let CellKind::Binary { op: crate::BinaryOp::Mul, signed: false, y, .. } =
            &m.cells[ci].kind
        {
            for &bit in y.iter() {
                mul_of.insert(bit, ci);
            }
        }
    }
    let mut count = 0;
    for ci in m.live_cells().collect::<Vec<_>>() {
        let CellKind::Shiftx { a, b, b_signed: false, offset, y } = &m.cells[ci].kind else {
            continue;
        };
        let Some(&mc) = b.0.first().and_then(|bit| mul_of.get(bit)) else { continue };
        let CellKind::Binary { a: p, b: q, y: my, .. } = &m.cells[mc].kind else { continue };
        // b is the product, possibly truncated or zero-extended.
        if !b.iter().zip(my.iter()).all(|(x, z)| x == z)
            || b.iter().skip(my.width()).any(|bit| *bit != SigBit::ZERO)
        {
            continue;
        }
        let (c, x) =
            match (p.as_const().and_then(|v| v.to_u64()), q.as_const().and_then(|v| v.to_u64())) {
                (Some(c), None) => (c, q.clone()),
                (None, Some(c)) => (c, p.clone()),
                _ => continue,
            };
        let ew = eff_width(&x);
        // The amount must not wrap: i * c fits in the shift amount.
        let cbits = 64 - c.leading_zeros() as usize;
        if ew > 12 || c == 0 || ew + cbits > b.width().min(my.width()) {
            continue;
        }
        let (a, y, offset) = (a.clone(), y.clone(), *offset);
        let w = y.width();
        let words: Vec<Sig> = (0..1u64 << ew)
            .map(|k| {
                (0..w)
                    .map(|j| {
                        let pos = offset + (c * k) as i64 + j as i64;
                        if pos >= 0 && (pos as usize) < a.width() {
                            a.0[pos as usize]
                        } else {
                            SigBit::Const(synth_core::Logic::X)
                        }
                    })
                    .collect()
            })
            .collect();
        let value = index_mux(m, &x.0[..ew], &words);
        m.cells[ci].alive = false;
        m.connect(y, value);
        count += 1;
    }
    count
}

/// `words[index]` as a tree of multiplexers (index bits least significant
/// first). Constant tables (ROMs) select on the bits in whichever order
/// gives the smaller decision diagram: the shared subtrees are what LUT
/// mapping can exploit.
fn index_mux(m: &mut Module, index: &[SigBit], words: &[Sig]) -> Sig {
    let constant = words.iter().all(|w| w.as_const().is_some());
    let msb_first =
        constant && bdd_size(index.len(), words, true) < bdd_size(index.len(), words, false);
    index_mux_ordered(m, index, words, msb_first)
}

fn index_mux_ordered(m: &mut Module, index: &[SigBit], words: &[Sig], msb_first: bool) -> Sig {
    if words.len() == 1 || index.is_empty() {
        return words[0].clone();
    }
    let n = index.len();
    let (a, b, s) = if msb_first {
        // Words past the end are undefined: take the low half for them.
        let half = 1usize << (n - 1);
        if words.len() <= half {
            return index_mux_ordered(m, &index[..n - 1], words, msb_first);
        }
        let a = index_mux_ordered(m, &index[..n - 1], &words[..half], msb_first);
        let b = index_mux_ordered(m, &index[..n - 1], &words[half..], msb_first);
        (a, b, index[n - 1])
    } else {
        let even: Vec<Sig> = words.iter().step_by(2).cloned().collect();
        let odd: Vec<Sig> = words.iter().skip(1).step_by(2).cloned().collect();
        let a = index_mux_ordered(m, &index[1..], &even, msb_first);
        let b = index_mux_ordered(m, &index[1..], &odd, msb_first);
        (a, b, index[0])
    };
    m.mux(s, a, b)
}

/// Decision nodes of the reduced diagrams of every bit of a constant table
/// over `n` index bits, splitting on the top bit first or the bottom one.
fn bdd_size(n: usize, words: &[Sig], msb_first: bool) -> usize {
    use synth_core::Logic;
    let w = words[0].width();
    let mut total = 0;
    for j in 0..w {
        let bits: Vec<Logic> = (0..1usize << n)
            .map(|k| match words.get(k) {
                Some(word) => match word.0[j] {
                    SigBit::Const(v) => v,
                    _ => Logic::X,
                },
                None => Logic::X,
            })
            .collect();
        // Index bits in splitting order, as positions in the table index.
        let order: Vec<usize> = if msb_first { (0..n).rev().collect() } else { (0..n).collect() };
        let mut nodes: FxHashSet<Vec<Logic>> = FxHashSet::default();
        // Each level's distinct sub-functions, as tables over the remaining bits.
        let mut level: Vec<Vec<Logic>> = vec![bits];
        for &bit in &order {
            let mut next = Vec::new();
            for f in level {
                let (lo, hi) = split(&f, bit);
                if lo != hi && nodes.insert(f) {
                    next.push(lo);
                    next.push(hi);
                }
            }
            level = next;
        }
        total += nodes.len();
    }
    total
}

/// Cofactors of a table over index bits (positions in the full index; the
/// table keeps its length, entries of the other half are duplicated).
fn split(f: &[synth_core::Logic], bit: usize) -> (Vec<synth_core::Logic>, Vec<synth_core::Logic>) {
    let lo = (0..f.len()).map(|k| f[k & !(1 << bit)]).collect();
    let hi = (0..f.len()).map(|k| f[k | (1 << bit)]).collect();
    (lo, hi)
}
