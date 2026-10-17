//! Lowering of word-level RTL to a [`LogicNet`] ("bit-blasting").

use synth_core::{FxHashMap, Logic};
use synth_rtl::{BinaryOp, CellId, CellKind, Module, PortDir, Sig, SigBit, UnaryOp};

use crate::aig::Lit;
use crate::net::{Bit, Carry, Ff, Instance, LogicNet, Port};

#[derive(Clone, Debug)]
pub struct LowerOptions {
    /// Adders and comparators at least this wide use hard carry chains
    /// ([`Carry`]); `None` builds them from gates.
    pub carry_min_width: Option<usize>,
}

impl Default for LowerOptions {
    fn default() -> Self {
        LowerOptions { carry_min_width: Some(3) }
    }
}

struct Lower<'a> {
    m: &'a Module,
    opts: &'a LowerOptions,
    net: LogicNet,
    lit: FxHashMap<SigBit, Lit>,
    /// Combinational cell driving each bit.
    driver: FxHashMap<SigBit, CellId>,
    /// Connection source of a bit (`lhs <- rhs`).
    alias: FxHashMap<SigBit, SigBit>,
    pads: FxHashMap<SigBit, u32>,
    /// RTL cell that created each AIG node (`SYNTH_DEBUG`).
    origin: Vec<u32>,
    /// Signal bits that something reads (after resolving connections).
    used: synth_core::FxHashSet<SigBit>,
    /// Number of reads of each signal bit.
    uses: FxHashMap<SigBit, u32>,
    /// AND nodes created per cell kind (`SYNTH_DEBUG`).
    profile: Option<FxHashMap<String, usize>>,
}

/// Lowers `m` (memories and latches must have been removed) to a logic
/// network.
pub fn lower(m: &Module, opts: &LowerOptions) -> Result<LogicNet, String> {
    let mut l = Lower {
        m,
        opts,
        net: LogicNet { name: m.name.clone(), ..Default::default() },
        lit: FxHashMap::default(),
        driver: FxHashMap::default(),
        alias: FxHashMap::default(),
        pads: FxHashMap::default(),
        profile: std::env::var("SYNTH_DEBUG").ok().map(|_| FxHashMap::default()),
        used: Default::default(),
        uses: Default::default(),
        origin: Vec::new(),
    };
    if let Some(mem) = m.memories.iter().find(|m| m.alive) {
        return Err(format!("memory {} was not mapped", mem.name));
    }
    for (lhs, rhs) in &m.connections {
        for (a, b) in lhs.iter().zip(rhs.iter()) {
            l.alias.insert(*a, *b);
        }
    }
    let mut used = Vec::new();
    for c in m.cells.iter().filter(|c| c.alive) {
        c.for_each_input(|b| used.push(b));
    }
    for (_, rhs) in &m.connections {
        used.extend(rhs.iter().copied());
    }
    for w in m.ports() {
        if m.wires[w].port.is_some_and(|(d, _)| d != PortDir::Input) {
            used.extend(m.wire_sig(w).iter().copied());
        }
    }
    for &b in &used {
        *l.uses.entry(l.resolve(b)).or_default() += 1;
    }
    l.used = used.into_iter().map(|b| l.resolve(b)).collect();
    // Sources: input ports, flip-flop outputs, instance outputs.
    let mut ports = Vec::new();
    for w in m.ports() {
        let wire = &m.wires[w];
        let (dir, _) = wire.port.unwrap();
        let mut bits = Vec::new();
        for b in m.wire_sig(w).iter() {
            match dir {
                PortDir::Input => {
                    let s = l.net.aig.inputs.len() as u32;
                    let lit = l.net.aig.input();
                    l.lit.insert(*b, lit);
                    bits.push(Bit::Src(s));
                }
                PortDir::Inout => {
                    let p = l.net.pads;
                    l.net.pads += 1;
                    l.pads.insert(*b, p);
                    bits.push(Bit::Pad(p));
                }
                PortDir::Output => bits.push(Bit::Snk(usize::MAX)), // filled below
            }
        }
        ports.push(Port { name: wire.name.clone(), dir, bits });
    }
    for (ci, c) in m.cells.iter_enumerated().filter(|(_, c)| c.alive) {
        match &c.kind {
            CellKind::Dff(d) => {
                for b in d.q.iter() {
                    let lit = l.net.aig.input();
                    l.lit.insert(*b, lit);
                }
            }
            CellKind::Instance(i) => {
                for (_, dir, s) in &i.conns {
                    if *dir == PortDir::Output {
                        for b in s.iter() {
                            if let SigBit::Wire(..) = b {
                                let lit = l.net.aig.input();
                                l.lit.insert(*b, lit);
                            }
                        }
                    }
                }
            }
            CellKind::Latch(_) => return Err(format!("latch {} is not supported", c.name)),
            _ => {
                for o in c.outputs() {
                    for b in o.iter() {
                        l.driver.insert(*b, ci);
                    }
                }
            }
        }
    }
    // Sinks.
    for (p, w) in ports.iter_mut().zip(m.ports()) {
        if p.dir == PortDir::Output {
            let sig = m.wire_sig(w);
            for (k, b) in sig.iter().enumerate() {
                let lit = l.get(*b)?;
                p.bits[k] = Bit::Snk(l.net.aig.output(lit));
            }
        }
    }
    l.net.ports = ports;
    for c in m.cells.iter().filter(|c| c.alive) {
        match &c.kind {
            CellKind::Dff(d) => {
                for j in 0..d.q.width() {
                    let clk = l.get(d.clk.sig)?;
                    let (clk, posedge) = (clk.regular(), d.clk.posedge ^ clk.is_compl());
                    let ctl = |l: &mut Lower, c: synth_rtl::Control| -> Result<usize, String> {
                        let v = l.get(c.sig)?;
                        Ok(l.net.aig.output(v.xor(!c.pol)))
                    };
                    let ff = Ff {
                        name: c.name.clone(),
                        clk: l.net.aig.output(clk),
                        posedge,
                        d: {
                            let v = l.get(d.d.0[j])?;
                            l.net.aig.output(v)
                        },
                        en: d.en.map(|c| ctl(&mut l, c)).transpose()?,
                        srst: match &d.srst {
                            Some((c, v)) => Some((ctl(&mut l, *c)?, v.get(j) == Logic::One)),
                            None => None,
                        },
                        en_over_srst: d.en_over_srst,
                        arst: match &d.arst {
                            Some((c, v)) => Some((ctl(&mut l, *c)?, v.get(j) == Logic::One)),
                            None => None,
                        },
                        init: d.init.get(j),
                        q: l.source(l.lit[&d.q.0[j]]),
                    };
                    l.net.ffs.push(ff);
                }
            }
            CellKind::Instance(i) => {
                let mut conns = Vec::new();
                for (name, dir, s) in &i.conns {
                    let mut bits = Vec::new();
                    for b in s.iter() {
                        bits.push(match dir {
                            PortDir::Input => {
                                let v = l.get(*b)?;
                                Bit::Snk(l.net.aig.output(v))
                            }
                            PortDir::Output => match l.lit.get(b) {
                                Some(&v) => Bit::Src(l.source(v)),
                                None => Bit::Pad(u32::MAX), // unconnected
                            },
                            PortDir::Inout => match l.pads.get(&l.resolve(*b)) {
                                Some(&p) => Bit::Pad(p),
                                None => {
                                    return Err(format!(
                                        "{}: bidirectional pin {name} must connect to an inout port",
                                        c.name
                                    ));
                                }
                            },
                        });
                    }
                    conns.push((name.clone(), *dir, bits));
                }
                l.net.instances.push(Instance {
                    name: c.name.clone(),
                    module: i.module.clone(),
                    params: i.params.clone(),
                    conns,
                });
            }
            _ => {}
        }
    }
    if let Some(p) = &l.profile {
        let mut v: Vec<_> = p.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        eprintln!("debug: AND nodes by cell kind: {v:?}");
        l.critical_path();
    }
    Ok(l.net)
}

impl Lower<'_> {
    /// Prints the RTL cells along the deepest AIG path (debugging aid).
    fn critical_path(&self) {
        let g = &self.net.aig;
        let lv = g.levels();
        let Some(out) = g.outputs.iter().max_by_key(|o| lv[o.node() as usize]) else { return };
        // Names of wires aliased to cell outputs, for readable reports.
        let mut names: FxHashMap<SigBit, String> = FxHashMap::default();
        for (lhs, rhs) in self.m.aliases.iter().chain(&self.m.connections) {
            for (l, r) in lhs.iter().zip(rhs.iter()) {
                names.entry(*r).or_insert_with(|| self.m.bit_name(*l));
            }
        }
        let mut n = out.node();
        let mut cells: Vec<u32> = Vec::new();
        while let Some((a, b)) = g.fanins(n) {
            if let Some(&o) = self.origin.get(n as usize)
                && cells.last() != Some(&o)
            {
                cells.push(o);
            }
            n = if lv[a.node() as usize] >= lv[b.node() as usize] { a.node() } else { b.node() };
        }
        eprintln!(
            "debug: deepest AIG path: {} levels through {} RTL cells",
            lv[out.node() as usize],
            cells.len()
        );
        for &c in cells.iter().rev() {
            if c == u32::MAX {
                continue;
            }
            let cell = &self.m.cells[CellId::new(c as usize)];
            let kind = match &cell.kind {
                CellKind::Binary { op, .. } => format!("{op:?}"),
                CellKind::Unary { op, .. } => format!("{op:?}"),
                CellKind::Mux { .. } => "Mux".into(),
                CellKind::Pmux { .. } => "Pmux".into(),
                CellKind::Shiftx { .. } => "Shiftx".into(),
                _ => "?".into(),
            };
            let label =
                cell.outputs().first().and_then(|y| y.iter().find_map(|b| names.get(b).cloned()));
            eprintln!("debug:   {kind} {}", label.unwrap_or_default());
        }
    }

    /// The input index of a source literal.
    fn source(&self, l: Lit) -> u32 {
        match self.net.aig.nodes[l.node() as usize] {
            crate::aig::Node::Input(k) => k,
            _ => unreachable!("not a source"),
        }
    }

    fn resolve(&self, mut b: SigBit) -> SigBit {
        let mut n = 0;
        while let Some(&r) = self.alias.get(&b) {
            b = r;
            n += 1;
            if n > 1_000_000 {
                break;
            }
        }
        b
    }

    /// The literal of a signal bit, lowering its driving logic on demand
    /// (iteratively, so deep logic does not exhaust the stack).
    fn get(&mut self, b: SigBit) -> Result<Lit, String> {
        let b = self.resolve(b);
        if let Some(&l) = self.lit.get(&b) {
            return Ok(l);
        }
        match b {
            SigBit::Const(Logic::One) => return Ok(Lit::TRUE),
            SigBit::Const(_) => return Ok(Lit::FALSE),
            SigBit::Wire(..) => {}
        }
        let Some(&root) = self.driver.get(&b) else {
            // Undriven: don't care.
            self.lit.insert(b, Lit::FALSE);
            return Ok(Lit::FALSE);
        };
        let mut stack: Vec<(CellId, bool)> = vec![(root, false)];
        let mut on_stack: FxHashMap<CellId, ()> = FxHashMap::default();
        while let Some((ci, expanded)) = stack.pop() {
            let cell = &self.m.cells[ci];
            if cell.outputs().iter().all(|o| o.iter().all(|x| self.lit.contains_key(x))) {
                continue;
            }
            if expanded {
                self.lower_cell(ci)?;
                on_stack.remove(&ci);
                continue;
            }
            if on_stack.insert(ci, ()).is_some() {
                return Err(format!("combinational loop through {}", cell.name));
            }
            stack.push((ci, true));
            let mut deps = Vec::new();
            cell.for_each_input(|x| deps.push(x));
            for x in deps {
                let x = self.resolve(x);
                if self.lit.contains_key(&x) {
                    continue;
                }
                if let Some(&d) = self.driver.get(&x) {
                    if on_stack.contains_key(&d) {
                        return Err(format!("combinational loop through {}", self.m.cells[d].name));
                    }
                    stack.push((d, false));
                }
            }
        }
        Ok(self.lit.get(&b).copied().unwrap_or(Lit::FALSE))
    }

    fn sig(&mut self, s: &Sig) -> Result<Vec<Lit>, String> {
        s.iter().map(|&b| self.get(b)).collect()
    }

    fn set(&mut self, y: &Sig, v: Vec<Lit>) {
        debug_assert_eq!(y.width(), v.len());
        for (b, l) in y.iter().zip(v) {
            self.lit.insert(*b, l);
        }
    }

    fn lower_cell(&mut self, ci: CellId) -> Result<(), String> {
        let before = self.net.aig.nodes.len();
        self.lower_cell_inner(ci)?;
        if self.profile.is_some() {
            let n = self.net.aig.nodes.len();
            self.origin.resize(before, u32::MAX);
            self.origin.resize(n, ci.index() as u32);
        }
        if let Some(p) = self.profile.as_mut() {
            let c = &self.m.cells[ci];
            let kind = match &c.kind {
                CellKind::Binary { op, .. } => format!("{op:?}"),
                CellKind::Unary { op, .. } => format!("{op:?}"),
                CellKind::Mux { .. } => "Mux".into(),
                CellKind::Pmux { .. } => "Pmux".into(),
                CellKind::Shiftx { .. } => "Shiftx".into(),
                _ => "other".into(),
            };
            *p.entry(kind).or_default() += self.net.aig.nodes.len() - before;
        }
        Ok(())
    }

    fn lower_cell_inner(&mut self, ci: CellId) -> Result<(), String> {
        let cell = &self.m.cells[ci];
        match &cell.kind {
            CellKind::Unary { op, a, y } => {
                let a = self.sig(a)?;
                let g = &mut self.net.aig;
                let v = match op {
                    UnaryOp::Not => a.iter().map(|&x| !x).collect(),
                    UnaryOp::Neg => {
                        let na: Vec<Lit> = a.iter().map(|&x| !x).collect();
                        let zero = vec![Lit::FALSE; a.len()];
                        self.add(&na, &zero, Lit::TRUE).0
                    }
                    UnaryOp::ReduceAnd => vec![g.and_all(&a)],
                    UnaryOp::ReduceOr => vec![g.or_all(&a)],
                    UnaryOp::ReduceXor => vec![g.xor_all(&a)],
                    UnaryOp::ReduceXnor => vec![!g.xor_all(&a)],
                    UnaryOp::LogicNot => vec![!g.or_all(&a)],
                };
                let v = fit(v, y.width());
                self.set(y, v);
            }
            CellKind::Binary { op, signed, a, b, y } => {
                if *op == BinaryOp::Add
                    && let Some((ia, ib, c)) = self.carry_in_operand(a, b, y)
                {
                    // `(x + y) + {0, c}`: one chain with carry-in `c`.
                    let yw = y.iter().rposition(|b| self.used.contains(b)).map_or(0, |i| i + 1);
                    let (av, bv, cin) = (self.sig(&ia)?, self.sig(&ib)?, self.get(c)?);
                    let v = self.add(&fit(av, yw), &fit(bv, yw), cin).0;
                    self.set(y, fit(v, y.width()));
                    return Ok(());
                }
                let (mut av, mut bv) = (self.sig(a)?, self.sig(b)?);
                let mut yw = y.width();
                // Arithmetic only up to the highest result bit anyone reads.
                if matches!(op, BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul) {
                    yw = y.iter().rposition(|b| self.used.contains(b)).map_or(0, |i| i + 1);
                    av.truncate(yw);
                    bv.truncate(yw);
                }
                let v = self.binary(*op, *signed, av, bv, yw)?;
                let v = fit(v, y.width());
                self.set(y, v);
            }
            CellKind::Mux { a, b, s, y, .. } => {
                let (av, bv, sv) = (self.sig(a)?, self.sig(b)?, self.get(*s)?);
                let g = &mut self.net.aig;
                let v = av.iter().zip(&bv).map(|(&x, &t)| g.mux(sv, t, x)).collect();
                self.set(y, v);
            }
            CellKind::Pmux { default, cases, s, y } => {
                let mut v = self.sig(default)?;
                let sv = self.sig(s)?;
                for (i, c) in cases.iter().enumerate().rev() {
                    let cv = self.sig(c)?;
                    let g = &mut self.net.aig;
                    v = v.iter().zip(&cv).map(|(&e, &t)| g.mux(sv[i], t, e)).collect();
                }
                self.set(y, v);
            }
            CellKind::Shiftx { a, b, b_signed, offset, y } => {
                let av = self.sig(a)?;
                let mut bv = self.sig(b)?;
                let w = y.width();
                // Base vector starting at index `offset`.
                let base: Vec<Lit> = if *offset >= 0 {
                    av.iter().skip(*offset as usize).copied().collect()
                } else {
                    std::iter::repeat_n(Lit::FALSE, offset.unsigned_abs() as usize)
                        .chain(av.iter().copied())
                        .collect()
                };
                let neg = if *b_signed { bv.pop() } else { None };
                let mut v = self.shift_right(&base, &bv, Lit::FALSE, w);
                if let Some(n) = neg {
                    let g = &mut self.net.aig;
                    v = v.into_iter().map(|x| g.and(x, !n)).collect();
                }
                self.set(y, v);
            }
            _ => unreachable!("not a combinational cell"),
        }
        Ok(())
    }

    /// For `a + b` (result `y`) where one operand is a single zero-extended
    /// bit `c` and the other is the whole result of another addition read
    /// nowhere else, returns that addition's operands and `c`.
    fn carry_in_operand(&self, a: &Sig, b: &Sig, y: &Sig) -> Option<(Sig, Sig, SigBit)> {
        let w = y.width();
        for (x, z) in [(a, b), (b, a)] {
            let c = self.resolve(*z.0.first()?);
            if !matches!(c, SigBit::Wire(..))
                || z.iter()
                    .skip(1)
                    .take(w.saturating_sub(1))
                    .any(|&b| !matches!(self.resolve(b), SigBit::Const(Logic::Zero)))
            {
                continue;
            }
            let Some(&inner) = self.driver.get(&self.resolve(*x.0.first()?)) else { continue };
            let CellKind::Binary { op: BinaryOp::Add, a: ia, b: ib, y: iy, .. } =
                &self.m.cells[inner].kind
            else {
                continue;
            };
            if iy.width() < w.min(x.width())
                || x.width() < w
                || (0..w).any(|i| self.resolve(x.0[i]) != iy.0[i])
                || iy.iter().any(|b| self.uses.get(b).copied().unwrap_or(0) > 1)
            {
                continue;
            }
            return Some((ia.clone(), ib.clone(), c));
        }
        None
    }

    fn binary(
        &mut self,
        op: BinaryOp,
        signed: bool,
        a: Vec<Lit>,
        b: Vec<Lit>,
        yw: usize,
    ) -> Result<Vec<Lit>, String> {
        let g = &mut self.net.aig;
        Ok(match op {
            BinaryOp::And => a.iter().zip(&b).map(|(&x, &y)| g.and(x, y)).collect(),
            BinaryOp::Or => a.iter().zip(&b).map(|(&x, &y)| g.or(x, y)).collect(),
            BinaryOp::Xor => a.iter().zip(&b).map(|(&x, &y)| g.xor(x, y)).collect(),
            BinaryOp::Xnor => a.iter().zip(&b).map(|(&x, &y)| !g.xor(x, y)).collect(),
            BinaryOp::LogicAnd => {
                let (x, y) = (g.or_all(&a), g.or_all(&b));
                vec![g.and(x, y)]
            }
            BinaryOp::LogicOr => {
                let (x, y) = (g.or_all(&a), g.or_all(&b));
                vec![g.or(x, y)]
            }
            BinaryOp::Eq | BinaryOp::Eqx | BinaryOp::Ne | BinaryOp::Nex => {
                let (a, b) = same_width(a, b, signed);
                let eqs: Vec<Lit> = a.iter().zip(&b).map(|(&x, &y)| !g.xor(x, y)).collect();
                let e = g.and_all(&eqs);
                vec![if matches!(op, BinaryOp::Eq | BinaryOp::Eqx) { e } else { !e }]
            }
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
                let (mut a, mut b) = same_width(a, b, signed);
                if signed && !a.is_empty() {
                    let n = a.len() - 1;
                    a[n] = !a[n];
                    b[n] = !b[n];
                }
                // `x >= y` is the carry out of `x + ~y + 1`.
                let (x, y, invert) = match op {
                    BinaryOp::Lt => (&a, &b, true),
                    BinaryOp::Ge => (&a, &b, false),
                    BinaryOp::Gt => (&b, &a, true),
                    _ => (&b, &a, false), // Le: b >= a
                };
                let ny: Vec<Lit> = y.iter().map(|&l| !l).collect();
                let ge = self.carry_out(x, &ny, Lit::TRUE);
                vec![ge.xor(invert)]
            }
            BinaryOp::Add => {
                let (a, b) = (fit(a, yw), fit(b, yw));
                self.add(&a, &b, Lit::FALSE).0
            }
            BinaryOp::Sub => {
                let (a, b) = (fit(a, yw), fit(b, yw));
                let nb: Vec<Lit> = b.iter().map(|&l| !l).collect();
                self.add(&a, &nb, Lit::TRUE).0
            }
            BinaryOp::Mul => {
                let (a, b) = (fit(a, yw), fit(b, yw));
                self.mul(a, b)
            }
            BinaryOp::Div | BinaryOp::Mod => {
                // Unsigned division by a power of two only.
                let k = b.iter().position(|&l| l == Lit::TRUE);
                let pow2 =
                    !signed && k.is_some() && b.iter().filter(|&&l| l != Lit::FALSE).count() == 1;
                if !pow2 {
                    return Err("division is only supported by constant powers of two".into());
                }
                let k = k.unwrap();
                if op == BinaryOp::Div {
                    a.iter()
                        .skip(k)
                        .copied()
                        .chain(std::iter::repeat(Lit::FALSE))
                        .take(yw)
                        .collect()
                } else {
                    a.iter()
                        .take(k)
                        .copied()
                        .chain(std::iter::repeat(Lit::FALSE))
                        .take(yw)
                        .collect()
                }
            }
            BinaryOp::Shl => {
                // Shift left == reverse, shift right, reverse.
                let ra: Vec<Lit> = a.iter().rev().copied().collect();
                let w = a.len();
                let r = self.shift_right(&ra, &b, Lit::FALSE, w);
                r.into_iter().rev().collect()
            }
            BinaryOp::Shr => {
                let w = a.len();
                self.shift_right(&a, &b, Lit::FALSE, w)
            }
            BinaryOp::Sshr => {
                let w = a.len();
                let fill = a.last().copied().unwrap_or(Lit::FALSE);
                self.shift_right(&a, &b, fill, w)
            }
        })
    }

    /// `y[i] = a[i + amount]` (`fill` beyond `a`), `w` result bits.
    fn shift_right(&mut self, a: &[Lit], amount: &[Lit], fill: Lit, w: usize) -> Vec<Lit> {
        let g = &mut self.net.aig;
        let at = |v: &[Lit], i: usize| v.get(i).copied().unwrap_or(fill);
        let mut cur: Vec<Lit> = (0..a.len().max(w)).map(|i| at(a, i)).collect();
        let n = cur.len();
        for (k, &s) in amount.iter().enumerate() {
            if k >= 63 || (1usize << k) >= n {
                // Any higher set bit shifts everything out.
                cur = cur.iter().map(|&x| g.mux(s, fill, x)).collect();
                continue;
            }
            let d = 1usize << k;
            cur = (0..n).map(|i| g.mux(s, at(&cur, i + d), cur[i])).collect();
        }
        cur.truncate(w);
        cur
    }

    /// `a + b + cin`, both `a.len()` bits; returns the sum and carry out.
    fn add(&mut self, a: &[Lit], b: &[Lit], cin: Lit) -> (Vec<Lit>, Lit) {
        let w = a.len();
        // Low bits where `b` equals the (constant) carry pass `a` through
        // unchanged and keep the carry: `x + 0 + 0` and `x + 1 + 1`.
        let mut k = 0;
        while k < w && cin.is_const() && b[k] == cin {
            k += 1;
        }
        if k > 0 {
            let (hs, c) = self.add(&a[k..], &b[k..], cin);
            let mut s = a[..k].to_vec();
            s.extend(hs);
            return (s, c);
        }
        if self.opts.carry_min_width.is_some_and(|m| w >= m) {
            return self.carry_chain(a, b, cin, true);
        }
        let g = &mut self.net.aig;
        let mut c = cin;
        let mut s = Vec::with_capacity(w);
        for i in 0..w {
            let x = g.xor(a[i], b[i]);
            s.push(g.xor(x, c));
            c = g.maj(a[i], b[i], c);
        }
        (s, c)
    }

    fn carry_out(&mut self, a: &[Lit], b: &[Lit], cin: Lit) -> Lit {
        let konst = |v: &[Lit]| v.iter().all(|l| l.is_const());
        if self.opts.carry_min_width.is_some_and(|m| a.len() >= m + 2) && !konst(a) && !konst(b) {
            return self.carry_chain(a, b, cin, false).1;
        }
        let g = &mut self.net.aig;
        let mut c = cin;
        for i in 0..a.len() {
            c = g.maj(a[i], b[i], c);
        }
        c
    }

    fn carry_chain(&mut self, a: &[Lit], b: &[Lit], cin: Lit, want_sum: bool) -> (Vec<Lit>, Lit) {
        let g = &mut self.net.aig;
        let mut sa = Vec::new();
        let mut sb = Vec::new();
        for i in 0..a.len() {
            sa.push(g.output(a[i]));
            sb.push(g.output(b[i]));
        }
        let cin_s = g.output(cin);
        let mut sum = Vec::new();
        let mut lits = Vec::new();
        if want_sum {
            for _ in 0..a.len() {
                sum.push(g.inputs.len() as u32);
                lits.push(g.input());
            }
        }
        let cout = g.inputs.len() as u32;
        let cl = g.input();
        self.net.carries.push(Carry { a: sa, b: sb, cin: cin_s, sum, cout: Some(cout) });
        (lits, cl)
    }

    /// Shift-and-add multiplier, truncated to the operand width.
    fn mul(&mut self, a: Vec<Lit>, b: Vec<Lit>) -> Vec<Lit> {
        let w = a.len();
        // Iterate over the operand with fewer non-constant bits.
        let var = |v: &[Lit]| v.iter().filter(|l| !l.is_const()).count();
        let (a, b) = if var(&b) <= var(&a) { (a, b) } else { (b, a) };
        let mut acc = vec![Lit::FALSE; w];
        let mut started = false;
        for i in 0..w {
            if b[i] == Lit::FALSE {
                continue;
            }
            let g = &mut self.net.aig;
            let pp: Vec<Lit> = (0..w - i).map(|k| g.and(a[k], b[i])).collect();
            if !started {
                acc[i..].copy_from_slice(&pp);
                started = true;
                continue;
            }
            let hi = acc[i..].to_vec();
            let (s, _) = self.add(&hi, &pp, Lit::FALSE);
            acc[i..].copy_from_slice(&s);
        }
        acc
    }
}

/// Zero-extends or truncates to `w` bits.
fn fit(mut v: Vec<Lit>, w: usize) -> Vec<Lit> {
    v.resize(w, Lit::FALSE);
    v
}

/// Extends the narrower operand (sign-extending when `signed`).
fn same_width(mut a: Vec<Lit>, mut b: Vec<Lit>, signed: bool) -> (Vec<Lit>, Vec<Lit>) {
    let w = a.len().max(b.len());
    let ext = |v: &mut Vec<Lit>| {
        let f = if signed { v.last().copied().unwrap_or(Lit::FALSE) } else { Lit::FALSE };
        v.resize(w, f);
    };
    ext(&mut a);
    ext(&mut b);
    (a, b)
}
