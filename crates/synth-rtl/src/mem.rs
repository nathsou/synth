//! Memory passes: merging registers into read ports, emulating read
//! transparency, and lowering memories to registers.

use synth_core::{FxHashMap, Logic, LogicVec};

use crate::{BinaryOp, CellId, CellKind, Clock, Control, Dff, MemId, Module, PortDir, Sig, SigBit};

/// Uses of every signal bit (cell inputs, memory ports, connections and
/// output ports).
fn use_counts(m: &Module) -> FxHashMap<SigBit, u32> {
    let mut u: FxHashMap<SigBit, u32> = FxHashMap::default();
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

/// Flip-flop bits by their `q` (or `d`) signal bit.
fn ff_bits(m: &Module, by_d: bool) -> FxHashMap<SigBit, (CellId, usize)> {
    let mut map = FxHashMap::default();
    for (ci, c) in m.cells.iter_enumerated() {
        if c.alive
            && let CellKind::Dff(d) = &c.kind
        {
            let s = if by_d { &d.d } else { &d.q };
            for (j, &b) in s.iter().enumerate() {
                map.insert(b, (ci, j));
            }
        }
    }
    map
}

/// Removes bits from a flip-flop cell (splitting it).
fn drop_ff_bits(m: &mut Module, ci: CellId, drop: &[usize]) {
    let CellKind::Dff(d) = &m.cells[ci].kind else { unreachable!() };
    let keep: Vec<usize> = (0..d.q.width()).filter(|j| !drop.contains(j)).collect();
    let pick = |s: &Sig| -> Sig { keep.iter().map(|&j| s.0[j]).collect() };
    let pick_v =
        |v: &LogicVec| LogicVec::from_bits(&keep.iter().map(|&j| v.get(j)).collect::<Vec<_>>());
    let new = Dff {
        clk: d.clk,
        d: pick(&d.d),
        q: pick(&d.q),
        en: d.en,
        srst: d.srst.as_ref().map(|(c, v)| (*c, pick_v(v))),
        en_over_srst: d.en_over_srst,
        arst: d.arst.as_ref().map(|(c, v)| (*c, pick_v(v))),
        init: pick_v(&d.init),
    };
    if keep.is_empty() {
        m.cells[ci].alive = false;
    } else {
        m.cells[ci].kind = CellKind::Dff(Box::new(new));
    }
}

/// Converts asynchronous read ports into synchronous ones by absorbing a
/// register on the data side (read data only feeds flip-flops) or on the
/// address side (the address comes from flip-flops; the port then reads at
/// the address registers' next state, write-first). Returns the number of
/// ports converted.
pub fn merge_read_ffs(m: &mut Module) -> usize {
    let mut n = 0;
    for mi in (0..m.memories.len()).map(MemId::new) {
        if !m.memories[mi].alive {
            continue;
        }
        for ri in 0..m.memories[mi].reads.len() {
            if m.memories[mi].reads[ri].clk.is_some() {
                continue;
            }
            let how = if merge_data_ff(m, mi, ri) {
                "data"
            } else if merge_addr_ff(m, mi, ri) {
                "address"
            } else {
                continue;
            };
            if std::env::var("SYNTH_DEBUG").is_ok() {
                eprintln!(
                    "debug: memory {} read port {ri}: {how} register merged",
                    m.memories[mi].name
                );
            }
            n += 1;
        }
    }
    n
}

fn merge_data_ff(m: &mut Module, mi: MemId, ri: usize) -> bool {
    let uses = use_counts(m);
    let by_d = ff_bits(m, true);
    let rd = &m.memories[mi].reads[ri];
    let mut found: Vec<(CellId, usize)> = Vec::new();
    for b in rd.data.iter() {
        if uses.get(b).copied() != Some(1) {
            return false;
        }
        let Some(&f) = by_d.get(b) else { return false };
        found.push(f);
    }
    let first = match &m.cells[found[0].0].kind {
        CellKind::Dff(d) => d.as_ref().clone(),
        _ => unreachable!(),
    };
    for &(ci, _) in &found {
        let CellKind::Dff(d) = &m.cells[ci].kind else { unreachable!() };
        if d.clk != first.clk || d.en != first.en || d.srst.is_some() || d.arst.is_some() {
            return false;
        }
    }
    let q: Sig = found
        .iter()
        .map(|&(ci, j)| match &m.cells[ci].kind {
            CellKind::Dff(d) => d.q.0[j],
            _ => unreachable!(),
        })
        .collect();
    let en = match first.en {
        None => SigBit::ONE,
        Some(c) if c.pol => c.sig,
        Some(c) => m.not1(c.sig),
    };
    let nw = m.memories[mi].writes.len();
    let rd = &mut m.memories[mi].reads[ri];
    rd.clk = Some(first.clk);
    rd.en = en;
    rd.data = q;
    rd.transparent = vec![false; nw];
    let mut by_cell: FxHashMap<CellId, Vec<usize>> = FxHashMap::default();
    for (ci, j) in found {
        by_cell.entry(ci).or_default().push(j);
    }
    for (ci, js) in by_cell {
        drop_ff_bits(m, ci, &js);
    }
    true
}

fn merge_addr_ff(m: &mut Module, mi: MemId, ri: usize) -> bool {
    let by_q = ff_bits(m, false);
    let addr = m.memories[mi].reads[ri].addr.clone();
    let mut clk: Option<Clock> = None;
    let mut next = Sig::new();
    for &b in addr.iter() {
        if let SigBit::Const(_) = b {
            next.push(b);
            continue;
        }
        let Some(&(ci, j)) = by_q.get(&b) else { return false };
        let CellKind::Dff(d) = &m.cells[ci].kind else { unreachable!() };
        if d.arst.is_some() || clk.is_some_and(|c| c != d.clk) {
            return false;
        }
        clk = Some(d.clk);
        let d = d.as_ref().clone();
        next.push(next_state(m, &d, j));
    }
    let Some(clk) = clk else { return false };
    let mem = &mut m.memories[mi];
    let transparent = mem.writes.iter().map(|w| w.clk == clk).collect();
    let rd = &mut mem.reads[ri];
    rd.clk = Some(clk);
    rd.en = SigBit::ONE;
    rd.addr = next;
    rd.transparent = transparent;
    true
}

/// The value bit `j` of a flip-flop takes at the next clock edge.
fn next_state(m: &mut Module, d: &Dff, j: usize) -> SigBit {
    let (q, data) = (d.q.0[j], d.d.0[j]);
    let ctl = |m: &mut Module, c: Control| if c.pol { c.sig } else { m.not1(c.sig) };
    let rst = d.srst.as_ref().map(|(c, v)| (*c, SigBit::Const(v.get(j))));
    let mut v = data;
    if let (true, Some((c, rv))) = (d.en_over_srst, rst) {
        let s = ctl(m, c);
        v = m.mux(s, Sig::bit(v), Sig::bit(rv)).0[0];
    }
    if let Some(c) = d.en {
        let s = ctl(m, c);
        v = m.mux(s, Sig::bit(q), Sig::bit(v)).0[0];
    }
    if let (false, Some((c, rv))) = (d.en_over_srst, rst) {
        let s = ctl(m, c);
        v = m.mux(s, Sig::bit(v), Sig::bit(rv)).0[0];
    }
    v
}

/// Makes every synchronous read port of `mi` non-transparent, adding bypass
/// logic that forwards same-cycle writes (for RAMs that read old data).
pub fn emulate_transparency(m: &mut Module, mi: MemId) {
    let nr = m.memories[mi].reads.len();
    for ri in 0..nr {
        let rd = m.memories[mi].reads[ri].clone();
        let Some(clk) = rd.clk else { continue };
        if !rd.transparent.iter().any(|&t| t) {
            continue;
        }
        let width = rd.data.width();
        let mut out = m.new_sig(width);
        // The RAM's own (old-data) output.
        let raw = out.clone();
        let mut result: Option<Sig> = None;
        for (wi, wr) in m.memories[mi].writes.clone().iter().enumerate() {
            if !rd.transparent[wi] {
                continue;
            }
            // Registered at the read clock (with the read enable): whether
            // the write hit the read address, per bit, and the written data.
            let aw = wr.addr.width().max(rd.addr.width());
            let (wa, ra) = (wr.addr.extend(aw, false), rd.addr.extend(aw, false));
            let hit = m.binary(BinaryOp::Eq, false, wa, ra).0[0];
            // One registered hit flag per distinct write-enable signal (all
            // bits share one for word-wide writes).
            let mut flags: Vec<(SigBit, SigBit)> = Vec::new();
            let mut sel_q = Sig::new();
            for &e in wr.en.iter() {
                let f = match flags.iter().find(|(x, _)| *x == e) {
                    Some(&(_, f)) => f,
                    None => {
                        let h = m.and1(e, hit);
                        let f = register(m, clk, rd.en, &Sig::bit(h)).0[0];
                        flags.push((e, f));
                        f
                    }
                };
                sel_q.push(f);
            }
            let data_q = register(m, clk, rd.en, &wr.data);
            let base = result.take().unwrap_or_else(|| raw.clone());
            let mut y = Sig::new();
            for b in 0..width {
                y.push(m.mux(sel_q.0[b], Sig::bit(base.0[b]), Sig::bit(data_q.0[b])).0[0]);
            }
            result = Some(y);
        }
        let result = result.expect("a transparent port has a write port");
        m.connect(rd.data.clone(), result);
        let rdp = &mut m.memories[mi].reads[ri];
        rdp.data = std::mem::take(&mut out);
        rdp.transparent.iter_mut().for_each(|t| *t = false);
    }
}

fn register(m: &mut Module, clk: Clock, en: SigBit, d: &Sig) -> Sig {
    let q = m.new_sig(d.width());
    let en = if en == SigBit::ONE { None } else { Some(Control { sig: en, pol: true }) };
    m.add_cell(
        "$mem_bypass",
        CellKind::Dff(Box::new(Dff {
            clk,
            d: d.clone(),
            q: q.clone(),
            en,
            srst: None,
            en_over_srst: false,
            arst: None,
            init: LogicVec::xs(d.width()),
        })),
    );
    q
}

/// Replaces memory `mi` by one register per word, write decoding and read
/// multiplexers. All write ports must share a clock.
pub fn to_registers(m: &mut Module, mi: MemId) -> Result<(), String> {
    let mem = m.memories[mi].clone();
    let width = mem.width as usize;
    let depth = mem.depth as usize;
    if let Some(w0) = mem.writes.first()
        && mem.writes.iter().any(|w| w.clk != w0.clk)
    {
        return Err(format!("memory {}: write ports on different clocks", mem.name));
    }
    // Word registers.
    let words: Vec<Sig> = (0..depth).map(|_| m.new_sig(width)).collect();
    // Next state of each word after all writes (later ports win).
    let mut next: Vec<Sig> = words.clone();
    for wr in &mem.writes {
        for (w, nw) in next.iter_mut().enumerate() {
            let a = LogicVec::from_i64(w as i64 + mem.offset, wr.addr.width());
            let hit = m.eq_const(wr.addr.clone(), &a);
            let mut y = Sig::new();
            for b in 0..width {
                let we = m.and1(hit, wr.en.0[b]);
                y.push(m.mux(we, Sig::bit(nw.0[b]), Sig::bit(wr.data.0[b])).0[0]);
            }
            *nw = y;
        }
    }
    if let Some(w0) = mem.writes.first() {
        for w in 0..depth {
            m.add_cell(
                format!("{}[{}]", mem.name, w as i64 + mem.offset),
                CellKind::Dff(Box::new(Dff {
                    clk: w0.clk,
                    d: next[w].clone(),
                    q: words[w].clone(),
                    en: None,
                    srst: None,
                    en_over_srst: false,
                    arst: None,
                    init: mem.init[w].clone(),
                })),
            );
        }
    } else {
        // A ROM: the words are constants.
        for (word, init) in words.iter().zip(&mem.init) {
            m.connect(word.clone(), Sig::from_const(init));
        }
    }
    for rd in &mem.reads {
        let transparent = rd.transparent.iter().any(|&t| t);
        let src = if transparent { &next } else { &words };
        let idx = if mem.offset == 0 {
            rd.addr.clone()
        } else {
            let aw = rd.addr.width();
            m.binary(
                BinaryOp::Sub,
                false,
                rd.addr.clone(),
                Sig::from_const(&LogicVec::from_i64(mem.offset, aw)),
            )
        };
        let value = mux_tree(m, &idx.0, src, width);
        match rd.clk {
            None => m.connect(rd.data.clone(), value),
            Some(clk) => {
                let q = register(m, clk, rd.en, &value);
                m.connect(rd.data.clone(), q);
            }
        }
    }
    m.memories[mi].alive = false;
    Ok(())
}

/// Selects `words[index]` with a binary tree of multiplexers (index bits
/// least significant first; out-of-range indices read as x).
fn mux_tree(m: &mut Module, index: &[SigBit], words: &[Sig], width: usize) -> Sig {
    fn go(m: &mut Module, index: &[SigBit], words: &[Sig], width: usize, bit: usize) -> Sig {
        match words.len() {
            0 => Sig(vec![SigBit::Const(Logic::X); width]),
            1 => words[0].clone(),
            n => {
                if bit >= index.len() {
                    return words[0].clone();
                }
                // Split by the current bit: even and odd indices.
                let even: Vec<Sig> = words.iter().step_by(2).cloned().collect();
                let odd: Vec<Sig> = words.iter().skip(1).step_by(2).cloned().collect();
                let _ = n;
                let a = go(m, index, &even, width, bit + 1);
                let b = go(m, index, &odd, width, bit + 1);
                m.mux(index[bit], a, b)
            }
        }
    }
    go(m, index, words, width, 0)
}

/// Removes writes of a word's own current value: where a write port's data
/// selects the data of an asynchronous read of the same address (bit for
/// bit), that selection becomes a condition that disables the write
/// instead. The read port often becomes unused. Returns the number of
/// write enable bits changed.
pub fn remove_feedback_writes(m: &mut Module) -> usize {
    let mut mux_of: FxHashMap<SigBit, (CellId, usize)> = FxHashMap::default();
    for (ci, c) in m.cells.iter_enumerated() {
        if c.alive
            && let CellKind::Mux { y, .. } = &c.kind
        {
            for (i, &b) in y.iter().enumerate() {
                mux_of.insert(b, (ci, i));
            }
        }
    }
    let mut changed = 0;
    for mi in (0..m.memories.len()).map(MemId::new) {
        if !m.memories[mi].alive {
            continue;
        }
        let mem = m.memories[mi].clone();
        for wi in 0..mem.writes.len() {
            for r in mem.reads.iter().filter(|r| r.clk.is_none() && r.addr == mem.writes[wi].addr) {
                let w = m.memories[mi].writes[wi].clone();
                let mut new = w.clone();
                let mut memo = FxHashMap::default();
                for i in 0..w.data.width() {
                    let (e, d) =
                        crate::dff::deep_hold(m, &mux_of, r.data.0[i], w.data.0[i], &mut memo, 0);
                    if e != SigBit::ONE {
                        new.en.0[i] = m.and1(w.en.0[i], e);
                        new.data.0[i] = d;
                        changed += 1;
                    }
                }
                m.memories[mi].writes[wi] = new;
            }
        }
    }
    changed
}

/// Merges write ports of the same memory that share a clock and address
/// (e.g. one per byte lane) into one port with per-bit enables; later ports
/// keep priority. Returns the number of ports removed.
pub fn merge_write_ports(m: &mut Module) -> usize {
    let mut removed = 0;
    for mi in (0..m.memories.len()).map(MemId::new) {
        if !m.memories[mi].alive {
            continue;
        }
        let writes = std::mem::take(&mut m.memories[mi].writes);
        let mut out: Vec<crate::MemWrite> = Vec::new();
        for w in writes {
            match out.last_mut() {
                Some(p) if p.clk == w.clk && p.addr == w.addr => {
                    for b in 0..w.en.width() {
                        let (pe, pd) = (p.en.0[b], p.data.0[b]);
                        let (we, wd) = (w.en.0[b], w.data.0[b]);
                        if pe == SigBit::ZERO {
                            p.en.0[b] = we;
                            p.data.0[b] = wd;
                        } else if we != SigBit::ZERO {
                            p.data.0[b] = m.mux(we, Sig::bit(pd), Sig::bit(wd)).0[0];
                            p.en.0[b] = m.or1(pe, we);
                        }
                    }
                    removed += 1;
                }
                _ => out.push(w),
            }
        }
        // Read ports refer to write ports by index (transparency).
        if removed > 0 {
            let n = out.len();
            for rd in &mut m.memories[mi].reads {
                let any = rd.transparent.iter().any(|&t| t);
                rd.transparent = vec![any; n];
            }
        }
        m.memories[mi].writes = out;
    }
    removed
}
