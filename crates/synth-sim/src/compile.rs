//! Compiles a [`Module`] into a [`Program`].
//!
//! Every cell output, flip-flop, memory read port and group of undriven
//! bits gets a slot in the state. Cell operands that are not exactly one
//! slot are gathered into a slot of their own (shared between identical
//! operands). Combinational instructions are then ordered by their
//! dependencies, with strongly connected groups repeated to a fixpoint, and
//! each slot records the instructions that read it, so that a change runs
//! only what it reaches.

use synth_core::{FxHashMap, Logic, LogicVec};
use synth_rtl::{BinaryOp, CellKind, Control, Module, PortDir, Sig, SigBit, UnaryOp};

use crate::program::*;

/// The mask of word `j` of a value of `w` bits.
fn word_mask(w: u32, j: usize) -> u64 {
    let lo = j as u32 * 64;
    if w >= lo + 64 { u64::MAX } else { mask(w.saturating_sub(lo)) }
}

/// Simulation options.
#[derive(Clone, Debug)]
pub struct Options {
    /// Four-state (`0 1 x z`) values; otherwise two-state, where unknown
    /// constants, initial values and out-of-range reads are 0.
    pub four_state: bool,
    /// Removes logic that reaches no output port, memory or instance
    /// (`synth_rtl::opt::remove_dead`); such signals then read as unknown.
    pub prune: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { four_state: true, prune: false }
    }
}

struct Node {
    ins: Ins,
    reads: Vec<u32>,
    writes: Vec<u32>,
}

/// `d = (x == key) ^ ne`, waiting to join a decoder.
struct EqConst {
    x: Val,
    key: u64,
    d: u32,
    ne: bool,
    ins: Ins,
}

/// Comparisons of one value with fewer constants than this stay
/// instructions of their own.
const MIN_DECODER: usize = 4;

struct Builder<'m> {
    m: &'m Module,
    four: bool,
    val: Vec<u64>,
    unk: Vec<u64>,
    /// Per word: slot kind, first word of its slot, width of the slot (at
    /// its first word), sources, writing node.
    kind: Vec<SlotKind>,
    slot_of: Vec<u32>,
    width: Vec<u32>,
    writer: Vec<u32>,
    wire_base: Vec<usize>,
    loc: Vec<Option<Loc>>,
    consts: FxHashMap<(u32, Vec<u64>, Vec<u64>), Val>,
    memo: FxHashMap<Vec<Loc>, Val>,
    nodes: Vec<Node>,
    /// Comparisons with constants, grouped into decoders at the end.
    eq_consts: Vec<EqConst>,
    /// Gated flip-flops: (domain, index, slots read), and their outputs.
    gated_reads: Vec<(u32, u32, Vec<u32>)>,
    gated_q: Vec<(u32, u32, u32)>,
    aux: Aux,
    mem_val: Vec<u64>,
    mem_unk: Vec<u64>,
    domains: Vec<Domain>,
    domain_key: Vec<(u32, bool)>,
    clocks: Vec<Clock>,
    /// Asynchronous read nodes of each memory.
    mem_reads: Vec<Vec<usize>>,
    warnings: Vec<String>,
}

/// Compiles `m` (which is cleaned up first, on a copy).
pub fn compile(m: &Module, opts: &Options) -> Result<Program, String> {
    let mut m = m.clone();
    synth_rtl::opt::resolve_connections(&mut m)?;
    if opts.prune {
        synth_rtl::opt::remove_dead(&mut m);
    }
    let mut b = Builder {
        m: &m,
        four: opts.four_state,
        val: Vec::new(),
        unk: Vec::new(),
        kind: Vec::new(),
        slot_of: Vec::new(),
        width: Vec::new(),
        writer: Vec::new(),
        wire_base: Vec::new(),
        loc: Vec::new(),
        consts: FxHashMap::default(),
        memo: FxHashMap::default(),
        nodes: Vec::new(),
        eq_consts: Vec::new(),
        gated_reads: Vec::new(),
        gated_q: Vec::new(),
        aux: Aux::default(),
        mem_val: Vec::new(),
        mem_unk: Vec::new(),
        domains: Vec::new(),
        domain_key: Vec::new(),
        clocks: Vec::new(),
        mem_reads: Vec::new(),
        warnings: Vec::new(),
    };
    b.layout()?;
    b.cells()?;
    b.memories()?;
    b.connections();
    b.decoders();
    Ok(b.finish())
}

impl Builder<'_> {
    fn alloc(&mut self, w: u32, kind: SlotKind) -> Val {
        let off = self.val.len() as u32;
        let n = nwords(w) as usize;
        for _ in 0..n {
            self.val.push(0);
            self.unk.push(0);
            self.kind.push(kind);
            self.slot_of.push(off);
            self.width.push(0);
            self.writer.push(NONE);
        }
        self.width[off as usize] = w;
        Val { off, w }
    }

    /// Sets the initial value of a slot (unknown bits are 0 in 2-state).
    fn set_init(&mut self, v: Val, x: &LogicVec) {
        let x = x.resize(v.w as usize, false);
        for j in 0..v.words() as usize {
            let (val, unk) = (x.val_words()[j], x.unk_words()[j]);
            let o = v.off as usize + j;
            if self.four {
                self.val[o] = val;
                self.unk[o] = unk;
            } else {
                self.val[o] = val & !unk;
            }
        }
    }

    fn constant(&mut self, x: &LogicVec) -> Val {
        let w = x.width() as u32;
        let (mut val, mut unk) = (x.val_words().to_vec(), x.unk_words().to_vec());
        if !self.four {
            for (v, u) in val.iter_mut().zip(&mut unk) {
                *v &= !*u;
                *u = 0;
            }
        }
        if let Some(&v) = self.consts.get(&(w, val.clone(), unk.clone())) {
            return v;
        }
        let v = self.alloc(w, SlotKind::Const);
        self.set_init(v, x);
        self.consts.insert((w, val, unk), v);
        v
    }

    fn bit_loc(&self, b: SigBit) -> Option<Loc> {
        match b {
            SigBit::Const(l) => Some(Loc::Const(l)),
            SigBit::Wire(w, i) => self.loc[self.wire_base[w.index()] + i as usize],
        }
    }

    fn locs(&self, s: &Sig) -> Vec<Loc> {
        s.iter().map(|&b| self.bit_loc(b).unwrap_or(Loc::Const(Logic::X))).collect()
    }

    /// Gives the bits of `s` the slot `v`, bit `k` at bit `k` of `v`.
    fn place(&mut self, s: &Sig, v: Val) {
        for (k, b) in s.iter().enumerate() {
            if let SigBit::Wire(w, i) = b {
                let idx = self.wire_base[w.index()] + *i as usize;
                self.loc[idx] = Some(Loc::Bit { word: v.off + k as u32 / 64, bit: (k % 64) as u8 });
            }
        }
    }

    /// The slot holding `locs`, gathering it when it is not exactly one slot.
    fn operand_locs(&mut self, locs: &[Loc]) -> Val {
        let w = locs.len() as u32;
        if locs.iter().all(|l| matches!(l, Loc::Const(_))) {
            let bits: Vec<Logic> = locs
                .iter()
                .map(|l| match l {
                    Loc::Const(c) => *c,
                    Loc::Bit { .. } => unreachable!(),
                })
                .collect();
            return self.constant(&LogicVec::from_bits(&bits));
        }
        if let Loc::Bit { word, bit: 0 } = locs[0] {
            let off = self.slot_of[word as usize];
            if off == word
                && self.width[off as usize] == w
                && locs
                    .iter()
                    .enumerate()
                    .all(|(k, l)| *l == Loc::Bit { word: off + k as u32 / 64, bit: (k % 64) as u8 })
            {
                return Val { off, w };
            }
        }
        if let Some(&v) = self.memo.get(locs) {
            return v;
        }
        let d = self.alloc(w, SlotKind::Comb);
        self.gather_into(d, locs);
        self.memo.insert(locs.to_vec(), d);
        d
    }

    fn operand(&mut self, s: &Sig) -> Val {
        let locs = self.locs(s);
        self.operand_locs(&locs)
    }

    /// An operand resized to `w` bits.
    fn operand_w(&mut self, s: &Sig, w: usize, signed: bool) -> Val {
        if s.width() == w {
            return self.operand(s);
        }
        let s = if s.width() > w { s.slice(0, w) } else { s.extend(w, signed) };
        self.operand(&s)
    }

    /// Adds a node computing `d` from the bits `locs`.
    fn gather_into(&mut self, d: Val, locs: &[Loc]) {
        let mut base = Vec::new();
        let mut any_const = false;
        let mut parts = Vec::new();
        let mut reads = Vec::new();
        let mut k = 0;
        while k < locs.len() {
            match locs[k] {
                Loc::Const(c) => {
                    let c = if self.four || c.is_known() { c } else { Logic::Zero };
                    any_const |= c != Logic::Zero;
                    base.push(c);
                    k += 1;
                }
                Loc::Bit { word, bit } => {
                    let mut len = 1;
                    while k + len < locs.len()
                        && (k + len) % 64 != 0
                        && bit as usize + len < 64
                        && locs[k + len] == (Loc::Bit { word, bit: bit + len as u8 })
                    {
                        len += 1;
                    }
                    parts.push(Part {
                        src: word,
                        dst: (k / 64) as u32,
                        lo: bit,
                        pos: (k % 64) as u8,
                        len: len as u8,
                    });
                    let s = self.slot_of[word as usize];
                    if !reads.contains(&s) {
                        reads.push(s);
                    }
                    base.extend(std::iter::repeat_n(Logic::Zero, len));
                    k += len;
                }
            }
        }
        if !any_const && parts.len() == 1 && parts[0].pos == 0 && parts[0].len as u32 == d.w {
            let p = parts[0];
            let mut ins = Ins::new(K::Extract, d.w, d.off, p.src, 0);
            ins.x = p.lo as u64;
            self.node(ins, reads, vec![d.off]);
            return;
        }
        let c = if any_const { self.constant(&LogicVec::from_bits(&base)).off } else { NONE };
        let start = self.aux.parts.len() as u32;
        let n = parts.len() as u32;
        self.aux.parts.extend(parts);
        let mut ins = Ins::new(K::Gather, d.w, d.off, start, n);
        ins.c = c;
        self.node(ins, reads, vec![d.off]);
    }

    fn node(&mut self, ins: Ins, reads: Vec<u32>, writes: Vec<u32>) -> usize {
        let n = self.nodes.len();
        for &w in &writes {
            self.writer[w as usize] = n as u32;
        }
        self.nodes.push(Node { ins, reads, writes });
        n
    }

    /// Allocates the slots of every driven bit, then of undriven ones.
    fn layout(&mut self) -> Result<(), String> {
        let m = self.m;
        let mut n = 0;
        for w in m.wires.iter() {
            self.wire_base.push(n);
            n += w.width as usize;
        }
        self.loc = vec![None; n];
        for c in m.cells.iter().filter(|c| c.alive) {
            match &c.kind {
                CellKind::Unary { y, .. }
                | CellKind::Binary { y, .. }
                | CellKind::Mux { y, .. }
                | CellKind::Pmux { y, .. }
                | CellKind::Shiftx { y, .. } => {
                    let v = self.alloc(y.width() as u32, SlotKind::Comb);
                    self.place(y, v);
                }
                CellKind::Dff(d) => {
                    let v = self.alloc(d.q.width() as u32, SlotKind::State);
                    self.set_init(v, &d.init);
                    self.place(&d.q, v);
                }
                CellKind::Latch(l) => {
                    let v = self.alloc(l.q.width() as u32, SlotKind::State);
                    self.set_init(v, &LogicVec::xs(l.q.width()));
                    self.place(&l.q, v);
                }
                CellKind::Instance(i) => {
                    if i.conns.iter().any(|(_, d, _)| *d != PortDir::Input) {
                        self.warnings.push(format!(
                            "instance `{}` of `{}` is not simulated: its outputs are inputs",
                            c.name, i.module
                        ));
                    }
                }
            }
        }
        for mem in m.memories.iter().filter(|m| m.alive) {
            for r in &mem.reads {
                let kind = if r.clk.is_some() { SlotKind::State } else { SlotKind::Comb };
                let v = self.alloc(r.data.width() as u32, kind);
                self.set_init(v, &LogicVec::xs(r.data.width()));
                self.place(&r.data, v);
            }
        }
        for (lhs, _) in &m.connections {
            let v = self.alloc(lhs.width() as u32, SlotKind::Comb);
            self.place(lhs, v);
        }
        // Aliases name bits that were connected to others: they share
        // their source's location, unless they are driven.
        let mut alias: FxHashMap<usize, SigBit> = FxHashMap::default();
        for (lhs, rhs) in &m.aliases {
            for (l, r) in lhs.iter().zip(rhs.iter()) {
                if let SigBit::Wire(w, i) = l {
                    let idx = self.wire_base[w.index()] + *i as usize;
                    if self.loc[idx].is_none() {
                        alias.insert(idx, *r);
                    }
                }
            }
        }
        // Undriven bits are inputs, one slot per wire.
        for (wi, w) in m.wires.iter_enumerated() {
            let base = self.wire_base[wi.index()];
            let undriven: Vec<usize> = (0..w.width as usize)
                .filter(|&i| self.loc[base + i].is_none() && !alias.contains_key(&(base + i)))
                .collect();
            if undriven.is_empty() {
                continue;
            }
            let v = self.alloc(w.width, SlotKind::Input);
            self.set_init(v, &LogicVec::xs(w.width as usize));
            for i in undriven {
                self.loc[base + i] =
                    Some(Loc::Bit { word: v.off + i as u32 / 64, bit: (i % 64) as u8 });
            }
        }
        // Resolve aliases, following chains.
        let mut changed = true;
        while changed {
            changed = false;
            let pending: Vec<(usize, SigBit)> = alias.iter().map(|(&k, &v)| (k, v)).collect();
            for (idx, r) in pending {
                if let Some(l) = self.bit_loc(r) {
                    self.loc[idx] = Some(l);
                    alias.remove(&idx);
                    changed = true;
                }
            }
        }
        for (idx, _) in alias {
            self.loc[idx] = Some(Loc::Const(Logic::X));
        }
        Ok(())
    }

    fn slot_val(&self, s: &Sig) -> Val {
        match self.bit_loc(s.0[0]) {
            Some(Loc::Bit { word, .. }) => {
                let off = self.slot_of[word as usize];
                Val { off, w: self.width[off as usize] }
            }
            _ => unreachable!("output without a slot"),
        }
    }

    /// A control bit (constants live in a slot of their own).
    fn ctl(&mut self, c: Control) -> Ctl {
        match self.bit_loc(c.sig).unwrap_or(Loc::Const(Logic::X)) {
            Loc::Bit { word, bit } => Ctl { word, bit, pol: c.pol },
            Loc::Const(l) => {
                let v = self.constant(&LogicVec::from_bits(&[l]));
                Ctl { word: v.off, bit: 0, pol: c.pol }
            }
        }
    }

    /// Whether a control is a constant at its active level (`Some(true)`)
    /// or inactive (`Some(false)`).
    fn ctl_const(&self, c: Control) -> Option<bool> {
        match self.bit_loc(c.sig)? {
            Loc::Const(l) => {
                let l = if self.four || l.is_known() { l } else { Logic::Zero };
                Some(l == Logic::from_bool(c.pol))
            }
            Loc::Bit { .. } => None,
        }
    }

    fn clock_domain(&mut self, clk: SigBit, posedge: bool) -> Option<u32> {
        let Some(Loc::Bit { word, bit }) = self.bit_loc(clk) else { return None };
        let ci = match self.clocks.iter().position(|c| c.word == word && c.bit == bit) {
            Some(i) => i,
            None => {
                self.clocks.push(Clock { word, bit, pos: None, neg: None });
                self.clocks.len() - 1
            }
        } as u32;
        if let Some(d) = self.domain_key.iter().position(|&k| k == (ci, posedge)) {
            return Some(d as u32);
        }
        let d = self.domains.len() as u32;
        self.domains.push(Domain::default());
        self.domain_key.push((ci, posedge));
        let c = &mut self.clocks[ci as usize];
        if posedge {
            c.pos = Some(d);
        } else {
            c.neg = Some(d);
        }
        Some(d)
    }

    /// Emits `ins`, whose result has `natural` bits, writing `y`: through a
    /// resizing copy when the widths differ.
    fn emit_to(&mut self, mut ins: Ins, natural: u32, y: Val, reads: Vec<u32>) {
        let one_bit = matches!(
            ins.k,
            K::RedAnd
                | K::RedOr
                | K::RedXor
                | K::RedXnor
                | K::LogicNot
                | K::Eq
                | K::Ne
                | K::Eqx
                | K::Nex
                | K::LtU
                | K::LtS
                | K::LeU
                | K::LeS
                | K::GtU
                | K::GtS
                | K::GeU
                | K::GeS
                | K::LAnd
                | K::LOr
        );
        if natural == y.w || (one_bit && y.w <= 64 && y.w >= 1) {
            ins.d = y.off;
            self.node(ins, reads, vec![y.off]);
            return;
        }
        let t = self.alloc(natural, SlotKind::Comb);
        ins.d = t.off;
        self.node(ins, reads, vec![t.off]);
        let locs: Vec<Loc> = (0..y.w)
            .map(|k| {
                if k < natural {
                    Loc::Bit { word: t.off + k / 64, bit: (k % 64) as u8 }
                } else {
                    Loc::Const(Logic::Zero)
                }
            })
            .collect();
        self.gather_into(y, &locs);
    }

    fn wide(&mut self, op: WideOp, y: Val, a: Val, b: Val, reads: Vec<u32>) {
        let x = self.aux.wide.len() as u64;
        self.aux.wide.push(WideIns { op, d: y, a, b });
        let mut ins = Ins::new(K::Wide, y.w, y.off, a.off, b.off);
        ins.x = x;
        self.node(ins, reads, vec![y.off]);
    }

    fn cells(&mut self) -> Result<(), String> {
        let m = self.m;
        for c in m.cells.iter().filter(|c| c.alive) {
            match &c.kind {
                CellKind::Unary { op, a, y } => {
                    let yv = self.slot_val(y);
                    let av = self.operand(a);
                    let k = match op {
                        UnaryOp::Not => K::Not,
                        UnaryOp::Neg => K::Neg,
                        UnaryOp::ReduceAnd => K::RedAnd,
                        UnaryOp::ReduceOr => K::RedOr,
                        UnaryOp::ReduceXor => K::RedXor,
                        UnaryOp::ReduceXnor => K::RedXnor,
                        UnaryOp::LogicNot => K::LogicNot,
                    };
                    let natural = if matches!(op, UnaryOp::Not | UnaryOp::Neg) { av.w } else { 1 };
                    if av.w > 64 || yv.w > 64 {
                        self.wide(WideOp::Unary(*op), yv, av, av, vec![av.off]);
                    } else {
                        self.emit_to(Ins::new(k, av.w, 0, av.off, 0), natural, yv, vec![av.off]);
                    }
                }
                CellKind::Binary { op, signed, a, b, y } => {
                    let yv = self.slot_val(y);
                    let (av, bv) = if op.is_shift() || op.is_logic() {
                        (self.operand(a), self.operand(b))
                    } else if op.is_compare() {
                        let w = a.width().max(b.width());
                        (self.operand_w(a, w, *signed), self.operand_w(b, w, *signed))
                    } else {
                        let w = a.width();
                        (self.operand(a), self.operand_w(b, w, *signed))
                    };
                    let reads = vec![av.off, bv.off];
                    let k = binary_kind(*op, *signed);
                    let natural = if op.is_compare() || op.is_logic() { 1 } else { av.w };
                    let narrow = av.w <= 64 && bv.w <= 64 && yv.w <= 64;
                    let k = match k {
                        Some(k) if narrow => k,
                        _ => {
                            self.wide(WideOp::Binary(*op, *signed), yv, av, bv, reads);
                            continue;
                        }
                    };
                    let mut ins = Ins::new(k, av.w, 0, av.off, bv.off);
                    ins.c = bv.w;
                    if matches!(k, K::Eq | K::Ne) {
                        let is_const = |v: Val| self.kind[v.off as usize] == SlotKind::Const;
                        let (x, c) = match (is_const(av), is_const(bv)) {
                            (false, true) => (av, Some(bv)),
                            (true, false) => (bv, Some(av)),
                            _ => (av, None),
                        };
                        if let Some(c) = c
                            && (!self.four || self.unk[c.off as usize] == 0)
                        {
                            ins.d = yv.off;
                            self.eq_consts.push(EqConst {
                                x,
                                key: self.val[c.off as usize],
                                d: yv.off,
                                ne: k == K::Ne,
                                ins,
                            });
                            continue;
                        }
                    }
                    self.emit_to(ins, natural, yv, reads);
                }
                CellKind::Mux { a, b, s, y, ternary } => {
                    let yv = self.slot_val(y);
                    let av = self.operand_w(a, y.width(), false);
                    let bv = self.operand_w(b, y.width(), false);
                    let sc = self.ctl(Control { sig: *s, pol: true });
                    let reads = vec![av.off, bv.off, self.slot_of[sc.word as usize]];
                    let mut ins = Ins::new(K::Mux, yv.w, yv.off, av.off, bv.off);
                    ins.c = sc.word;
                    ins.x = sc.bit as u64 | (*ternary as u64) << 8;
                    self.node(ins, reads, vec![yv.off]);
                }
                CellKind::Pmux { default, cases, s, y } => {
                    let yv = self.slot_val(y);
                    let dv = self.operand_w(default, y.width(), false);
                    let sv = self.operand(s);
                    let mut reads = vec![dv.off, sv.off];
                    let start = self.aux.lists.len() as u32;
                    let mut offs = Vec::new();
                    for cs in cases {
                        let cv = self.operand_w(cs, y.width(), false);
                        reads.push(cv.off);
                        offs.push(cv.off);
                    }
                    self.aux.lists.extend(offs);
                    let mut ins = Ins::new(K::Pmux, yv.w, yv.off, dv.off, sv.off);
                    ins.c = sv.w.min(cases.len() as u32);
                    ins.x = start as u64;
                    self.node(ins, reads, vec![yv.off]);
                }
                CellKind::Shiftx { a, b, b_signed, offset, y } => {
                    let yv = self.slot_val(y);
                    let av = self.operand(a);
                    let bv = self.operand(b);
                    let reads = vec![av.off, bv.off];
                    if yv.w > 64 || bv.w > 64 {
                        self.wide(
                            WideOp::Shiftx { b_signed: *b_signed, offset: *offset },
                            yv,
                            av,
                            bv,
                            reads,
                        );
                    } else {
                        let c = self.aux.shiftx.len() as u32;
                        self.aux.shiftx.push(ShiftxInfo {
                            a: av,
                            b: bv,
                            b_signed: *b_signed,
                            offset: *offset,
                        });
                        let mut ins = Ins::new(K::Shiftx, yv.w, yv.off, av.off, bv.off);
                        ins.c = c;
                        self.node(ins, reads, vec![yv.off]);
                    }
                }
                CellKind::Dff(d) => self.flop(d)?,
                CellKind::Latch(l) => {
                    let q = self.slot_val(&l.q);
                    let dv = self.operand_w(&l.d, l.q.width(), false);
                    let en = self.ctl(l.en);
                    let mut ins = Ins::new(K::Latch, q.w, q.off, dv.off, 0);
                    ins.c = en.word;
                    ins.x = en.bit as u64 | (en.pol as u64) << 8;
                    let reads = vec![dv.off, self.slot_of[en.word as usize]];
                    self.node(ins, reads, vec![q.off]);
                }
                CellKind::Instance(_) => {}
            }
        }
        Ok(())
    }

    fn flop(&mut self, d: &synth_rtl::Dff) -> Result<(), String> {
        let q = self.slot_val(&d.q);
        let w = q.w;
        let mut dv = self.operand_w(&d.d, w as usize, false);
        let mut en = d.en.map(|e| (e, self.ctl_const(e)));
        if let Some((_, Some(true))) = en {
            en = None;
        }
        let en_never = matches!(en, Some((_, Some(false))));
        if en_never {
            en = None;
            dv = q;
        }
        let en = en.map(|(e, _)| self.ctl(e));
        let srst = match &d.srst {
            Some((c, _)) if self.ctl_const(*c) == Some(false) => None,
            // Only acts when enabled, which it never is.
            Some(_) if en_never && d.en_over_srst => None,
            Some((c, v)) => {
                let v = self.constant(&v.resize(w as usize, false));
                Some((self.ctl(*c), v.off))
            }
            None => None,
        };
        let arst = match &d.arst {
            Some((c, _)) if self.ctl_const(*c) == Some(false) => None,
            Some((c, v)) => {
                let v = self.constant(&v.resize(w as usize, false));
                Some((self.ctl(*c), v.off))
            }
            None => None,
        };
        if let Some((c, v)) = arst {
            let mut ins = Ins::new(K::Arst, w, q.off, v, 0);
            ins.c = c.word;
            ins.x = c.bit as u64 | (c.pol as u64) << 8;
            let reads = vec![self.slot_of[c.word as usize]];
            self.node(ins, reads, vec![q.off]);
        }
        let Some(dom) = self.clock_domain(d.clk.sig, d.clk.posedge) else {
            return Ok(());
        };
        // Values read at the edge must be sampled before any state changes
        // unless they are combinational (or the flip-flop's own output).
        let mut reads = vec![self.slot_of[dv.off as usize]];
        reads.extend(en.map(|c| self.slot_of[c.word as usize]));
        reads.extend(srst.map(|(c, _)| self.slot_of[c.word as usize]));
        reads.extend(arst.map(|(c, _)| self.slot_of[c.word as usize]));
        let shadow = reads.iter().any(|&r| r != q.off && self.kind[r as usize] == SlotKind::State);
        let dst = if shadow { self.alloc(w, SlotKind::Comb).off } else { q.off };
        let ins = if srst.is_none() && arst.is_none() {
            match en {
                None => Ins::new(K::Copy, w, dst, dv.off, 0),
                Some(c) => {
                    let mut ins = Ins::new(K::FfEn, w, dst, dv.off, q.off);
                    ins.c = c.word;
                    ins.x = c.bit as u64 | (c.pol as u64) << 8;
                    ins
                }
            }
        } else {
            let x = self.aux.flops.len() as u64;
            self.aux.flops.push(Flop {
                dst,
                d: dv.off,
                q: q.off,
                w,
                en,
                srst,
                en_over_srst: d.en_over_srst,
                arst,
            });
            let mut ins = Ins::new(K::Ff, w, dst, dv.off, q.off);
            ins.x = x;
            ins
        };
        let dm = &mut self.domains[dom as usize];
        if shadow {
            dm.sample.push(ins);
            dm.commit.push(Ins::new(K::Copy, w, q.off, dst, 0));
        } else {
            // Its next value depends only on these (its own output aside):
            // it needs updating only after one of them changes.
            let idx = dm.gated.len() as u32;
            dm.gated.push(ins);
            let reads: Vec<u32> = reads
                .into_iter()
                .filter(|&r| r != q.off && self.kind[r as usize] != SlotKind::Const)
                .collect();
            self.gated_reads.push((dom, idx, reads));
            self.gated_q.push((q.off, dom, idx));
        }
        Ok(())
    }

    fn memories(&mut self) -> Result<(), String> {
        let m = self.m;
        for mem in m.memories.iter().filter(|m| m.alive) {
            let mi = self.aux.mems.len() as u32;
            let nw = nwords(mem.width) as usize;
            let base = self.mem_val.len();
            for i in 0..mem.depth as usize {
                let Some(init) = mem.init.get(i) else {
                    // Unspecified: x, or 0 in 2-state.
                    for j in 0..nw {
                        self.mem_val.push(0);
                        if self.four {
                            self.mem_unk.push(word_mask(mem.width, j));
                        }
                    }
                    continue;
                };
                let init = init.resize(mem.width as usize, false);
                for j in 0..nw {
                    let (v, u) = (init.val_words()[j], init.unk_words()[j]);
                    if self.four {
                        self.mem_val.push(v);
                        self.mem_unk.push(u);
                    } else {
                        self.mem_val.push(v & !u);
                    }
                }
            }
            self.aux.mems.push(Mem {
                name: mem.name.clone(),
                width: mem.width,
                depth: mem.depth,
                offset: mem.offset,
                base,
            });
            self.mem_reads.push(Vec::new());
            let mut write_ports = Vec::new();
            for wr in &mem.writes {
                let addr = self.operand(&wr.addr);
                if addr.w > 64 {
                    return Err(format!("memory `{}`: address wider than 64 bits", mem.name));
                }
                let data = self.operand_w(&wr.data, mem.width as usize, false);
                let en = self.operand_w(&wr.en, mem.width as usize, false);
                let wi = self.aux.writes.len() as u32;
                self.aux.writes.push(MemWrite { mem: mi, addr, data, en });
                let dom = self.clock_domain(wr.clk.sig, wr.clk.posedge);
                if let Some(dom) = dom {
                    let mut ins = Ins::new(K::MemWr, mem.width, 0, 0, 0);
                    ins.x = wi as u64;
                    self.domains[dom as usize].write.push(ins);
                }
                write_ports.push((wi, dom));
            }
            for r in &mem.reads {
                let data = self.slot_val(&r.data);
                let addr = self.operand(&r.addr);
                if addr.w > 64 {
                    return Err(format!("memory `{}`: address wider than 64 bits", mem.name));
                }
                match r.clk {
                    None => {
                        let mut ins = Ins::new(K::MemRd, mem.width, data.off, addr.off, addr.w);
                        ins.c = mi;
                        let n = self.node(ins, vec![addr.off], vec![data.off]);
                        self.mem_reads[mi as usize].push(n);
                    }
                    Some(clk) => {
                        let Some(dom) = self.clock_domain(clk.sig, clk.posedge) else { continue };
                        let en = match self.ctl_const(Control { sig: r.en, pol: true }) {
                            Some(true) => None,
                            _ => Some(self.ctl(Control { sig: r.en, pol: true })),
                        };
                        let transparent = write_ports
                            .iter()
                            .enumerate()
                            .filter(|(j, (_, wd))| {
                                r.transparent.get(*j).copied().unwrap_or(false) && *wd == Some(dom)
                            })
                            .map(|(_, (wi, _))| *wi)
                            .collect();
                        let shadow = self.alloc(data.w, SlotKind::Comb);
                        let x = self.aux.sync_reads.len() as u64;
                        self.aux.sync_reads.push(SyncRead {
                            mem: mi,
                            dst: shadow.off,
                            q: data.off,
                            en,
                            addr,
                            transparent,
                        });
                        let mut ins = Ins::new(K::MemRdSync, data.w, shadow.off, addr.off, 0);
                        ins.x = x;
                        let dm = &mut self.domains[dom as usize];
                        dm.sample.push(ins);
                        dm.commit.push(Ins::new(K::Copy, data.w, data.off, shadow.off, 0));
                    }
                }
            }
        }
        Ok(())
    }

    fn connections(&mut self) {
        let m = self.m;
        for (lhs, rhs) in &m.connections {
            let y = self.slot_val(lhs);
            let locs = self.locs(rhs);
            self.gather_into(y, &locs);
        }
    }

    /// Groups comparisons of one value with constants into decoders.
    fn decoders(&mut self) {
        let cands = std::mem::take(&mut self.eq_consts);
        let mut groups: FxHashMap<u32, Vec<usize>> = FxHashMap::default();
        let mut order = Vec::new();
        for (i, c) in cands.iter().enumerate() {
            let g = groups.entry(c.x.off).or_default();
            if g.is_empty() {
                order.push(c.x.off);
            }
            g.push(i);
        }
        for x in order {
            let g = &groups[&x];
            if g.len() < MIN_DECODER {
                for &i in g {
                    let c = &cands[i];
                    self.node(c.ins, vec![c.x.off, c.ins.b, c.ins.a], vec![c.d]);
                }
                continue;
            }
            let mut entries: Vec<(u64, u32, bool)> =
                g.iter().map(|&i| (cands[i].key, cands[i].d, cands[i].ne)).collect();
            entries.sort_unstable();
            let writes = entries.iter().map(|e| e.1).collect();
            let xv = cands[g[0]].x;
            let last = self.alloc(128, SlotKind::Comb);
            let idx = self.aux.decoders.len() as u64;
            self.aux.decoders.push(Decoder { entries });
            let mut ins = Ins::new(K::Decode, xv.w, last.off, xv.off, 0);
            ins.x = idx;
            self.node(ins, vec![xv.off], writes);
        }
    }

    /// Orders the nodes, assigns sources and builds the program.
    fn finish(mut self) -> Program {
        let sccs = self.sccs();
        let mut ops = Vec::with_capacity(self.nodes.len());
        let mut out = Vec::with_capacity(self.nodes.len());
        // The instruction that heads each node's unit (itself, or the
        // `Loop` of its group).
        let mut head = vec![0u32; self.nodes.len()];
        for scc in &sccs {
            let looped = scc.len() > 1 || {
                let n = &self.nodes[scc[0]];
                !matches!(n.ins.k, K::Latch | K::Arst)
                    && n.reads.iter().any(|&r| self.writer[r as usize] == scc[0] as u32)
            };
            if looped {
                let start = self.aux.lists.len() as u32;
                let mut count = 0;
                for &n in scc {
                    for &w in &self.nodes[n].writes {
                        for j in 0..nwords(self.width[w as usize]) {
                            self.aux.lists.push(w + j);
                            count += 1;
                        }
                    }
                }
                let mut ins = Ins::new(K::Loop, 0, 0, scc.len() as u32, start);
                ins.c = count;
                for &n in scc {
                    head[n] = ops.len() as u32;
                }
                ops.push(ins);
                out.push(Out::default());
            }
            for &n in scc {
                if !looped {
                    head[n] = ops.len() as u32;
                }
                let ins = self.nodes[n].ins;
                ops.push(ins);
                out.push(Out { n: nwords(self.width[ins.d as usize]), lo: 0, hi: 0 });
            }
        }
        // Gated flip-flops follow, each domain from a multiple of 64.
        let mut units = (ops.len() as u32).next_multiple_of(64);
        for d in &mut self.domains {
            d.base = units;
            units += (d.gated.len() as u32).next_multiple_of(64);
        }
        // Readers of each slot, by unit.
        let words = self.val.len();
        let mut pairs: Vec<(u32, u32)> = Vec::new();
        for (n, node) in self.nodes.iter().enumerate() {
            for &r in &node.reads {
                if self.kind[r as usize] != SlotKind::Const {
                    pairs.push((r, head[n]));
                }
            }
        }
        for (dom, idx, reads) in &self.gated_reads {
            let unit = self.domains[*dom as usize].base + idx;
            for &r in reads {
                pairs.push((r, unit));
            }
        }
        let mut state_unit = vec![NONE; words];
        for &(q, dom, idx) in &self.gated_q {
            state_unit[q as usize] = self.domains[dom as usize].base + idx;
        }
        pairs.sort_unstable();
        pairs.dedup();
        let mut rd_start = vec![0u32; words + 1];
        for &(s, _) in &pairs {
            rd_start[s as usize + 1] += 1;
        }
        for i in 0..words {
            rd_start[i + 1] += rd_start[i];
        }
        let rd: Vec<u32> = pairs.iter().map(|&(_, u)| u).collect();
        for (o, ins) in out.iter_mut().zip(&ops) {
            if ins.k != K::Loop {
                o.lo = rd_start[ins.d as usize];
                o.hi = rd_start[ins.d as usize + 1];
            }
        }
        let mem_readers: Vec<Vec<u32>> = self
            .mem_reads
            .iter()
            .map(|nodes| {
                let mut h: Vec<u32> = nodes.iter().map(|&n| head[n]).collect();
                h.sort_unstable();
                h.dedup();
                h
            })
            .collect();

        let m = self.m;
        let mut signals = Vec::new();
        let mut ports = Vec::new();
        for (wi, w) in m.wires.iter_enumerated() {
            if let Some((dir, pos)) = w.port {
                ports.push((signals.len(), dir, pos));
            }
            let base = self.wire_base[wi.index()];
            let locs: Vec<Loc> = (0..w.width as usize)
                .map(|i| self.loc[base + i].unwrap_or(Loc::Const(Logic::X)))
                .collect();
            signals.push((w.name.clone(), locs));
        }
        let four = self.four;
        Program {
            four_state: four,
            init_val: self.val,
            init_unk: if four { self.unk } else { Vec::new() },
            mem_init_val: self.mem_val,
            mem_init_unk: if four { self.mem_unk } else { Vec::new() },
            ops,
            out,
            units,
            state_unit,
            rd_start,
            rd,
            mem_readers,
            slot_of: self.slot_of,
            aux: self.aux,
            domains: self.domains,
            clocks: self.clocks,
            word_kind: self.kind,
            signals,
            ports,
            warnings: self.warnings,
        }
    }

    /// Strongly connected components of the node graph, dependencies first
    /// (Tarjan's algorithm, iterative).
    fn sccs(&self) -> Vec<Vec<usize>> {
        let n = self.nodes.len();
        let deps: Vec<Vec<usize>> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| {
                let mut d: Vec<usize> = node
                    .reads
                    .iter()
                    .filter_map(|&r| {
                        let w = self.writer[r as usize];
                        (w != NONE).then_some(w as usize)
                    })
                    .filter(|&w| w != i || !matches!(node.ins.k, K::Latch | K::Arst))
                    .collect();
                d.sort_unstable();
                d.dedup();
                d
            })
            .collect();
        let mut index = vec![u32::MAX; n];
        let mut low = vec![0u32; n];
        let mut on_stack = vec![false; n];
        let mut stack = Vec::new();
        let mut out = Vec::new();
        let mut next = 0u32;
        let mut call: Vec<(usize, usize)> = Vec::new();
        for root in 0..n {
            if index[root] != u32::MAX {
                continue;
            }
            call.push((root, 0));
            index[root] = next;
            low[root] = next;
            next += 1;
            stack.push(root);
            on_stack[root] = true;
            while let Some(&mut (v, ref mut i)) = call.last_mut() {
                if *i < deps[v].len() {
                    let w = deps[v][*i];
                    *i += 1;
                    if index[w] == u32::MAX {
                        index[w] = next;
                        low[w] = next;
                        next += 1;
                        stack.push(w);
                        on_stack[w] = true;
                        call.push((w, 0));
                    } else if on_stack[w] {
                        low[v] = low[v].min(index[w]);
                    }
                } else {
                    call.pop();
                    if let Some(&(p, _)) = call.last() {
                        low[p] = low[p].min(low[v]);
                    }
                    if low[v] == index[v] {
                        let mut scc = Vec::new();
                        loop {
                            let w = stack.pop().unwrap();
                            on_stack[w] = false;
                            scc.push(w);
                            if w == v {
                                break;
                            }
                        }
                        scc.reverse();
                        out.push(scc);
                    }
                }
            }
        }
        out
    }
}

fn binary_kind(op: BinaryOp, signed: bool) -> Option<K> {
    Some(match op {
        BinaryOp::And => K::And,
        BinaryOp::Or => K::Or,
        BinaryOp::Xor => K::Xor,
        BinaryOp::Xnor => K::Xnor,
        BinaryOp::Add => K::Add,
        BinaryOp::Sub => K::Sub,
        BinaryOp::Mul => K::Mul,
        BinaryOp::Div if signed => K::DivS,
        BinaryOp::Div => K::DivU,
        BinaryOp::Mod if signed => K::ModS,
        BinaryOp::Mod => K::ModU,
        BinaryOp::Shl => K::Shl,
        BinaryOp::Shr => K::Shr,
        BinaryOp::Sshr => K::Sshr,
        BinaryOp::Eq => K::Eq,
        BinaryOp::Ne => K::Ne,
        BinaryOp::Eqx => K::Eqx,
        BinaryOp::Nex => K::Nex,
        BinaryOp::Lt if signed => K::LtS,
        BinaryOp::Lt => K::LtU,
        BinaryOp::Le if signed => K::LeS,
        BinaryOp::Le => K::LeU,
        BinaryOp::Gt if signed => K::GtS,
        BinaryOp::Gt => K::GtU,
        BinaryOp::Ge if signed => K::GeS,
        BinaryOp::Ge => K::GeU,
        BinaryOp::LogicAnd => K::LAnd,
        BinaryOp::LogicOr => K::LOr,
    })
}
