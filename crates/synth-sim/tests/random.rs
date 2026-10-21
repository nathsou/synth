//! Random designs: synth-sim against a reference simulator built directly
//! on `synth_rtl::eval`, in both modes, and the C backend against the
//! interpreter.
//!
//! Each design has inputs, a clock with flip-flops on both edges (enables,
//! synchronous and asynchronous resets), memories with asynchronous and
//! synchronous (transparent) read ports, and combinational cells of every
//! kind over random slices and concatenations of what came before.

use synth_core::{Logic, LogicVec, Rng};
use synth_rtl::{
    BinaryOp, CellKind, Clock, Control, Dff, MemId, MemRead, MemWrite, Memory, Module, PortDir,
    Sig, SigBit, UnaryOp, WireId, eval,
};
use synth_sim::{Options, Sim};

struct Gen {
    rng: Rng,
    m: Module,
    /// Signals cells may read.
    avail: Vec<Sig>,
    inputs: Vec<(String, usize)>,
    outputs: Vec<String>,
    clk: SigBit,
}

impl Gen {
    fn below(&mut self, n: usize) -> usize {
        self.rng.below(n)
    }

    fn width(&mut self) -> usize {
        match self.below(10) {
            0 => 1,
            1..=5 => 1 + self.below(16),
            6..=8 => 1 + self.below(64),
            _ => 65 + self.below(70),
        }
    }

    fn constant(&mut self, w: usize) -> LogicVec {
        let bits: Vec<Logic> = (0..w)
            .map(|_| match self.below(20) {
                0 => Logic::X,
                1 => Logic::Z,
                k => Logic::from_bool(k % 2 == 0),
            })
            .collect();
        LogicVec::from_bits(&bits)
    }

    /// A random signal of `w` bits: slices of available signals and
    /// constants.
    fn sig(&mut self, w: usize) -> Sig {
        let mut s = Sig::new();
        while s.width() < w {
            let need = w - s.width();
            if self.avail.is_empty() || self.below(8) == 0 {
                let n = 1 + self.below(need);
                s = s.concat(&Sig::from_const(&self.constant(n)));
                continue;
            }
            let k = self.below(self.avail.len());
            let src = self.avail[k].clone();
            let lo = self.below(src.width());
            let n = (1 + self.below(src.width() - lo)).min(need);
            s = s.concat(&src.slice(lo, n));
        }
        s
    }

    fn bit(&mut self) -> SigBit {
        self.sig(1).0[0]
    }

    fn wire(&mut self, w: usize) -> Sig {
        let s = self.m.new_sig(w);
        // Name it so it can be observed.
        if let SigBit::Wire(id, _) = s.0[0] {
            self.m.wires[id].name = format!("w{}", id.index());
            self.m.wires[id].public = true;
        }
        s
    }

    fn control(&mut self) -> Control {
        Control { sig: self.bit(), pol: self.below(2) == 0 }
    }

    fn comb_cell(&mut self) {
        let w = self.width();
        let y = self.wire(w);
        let kind = match self.below(13) {
            12 => {
                // Bit-level multiplexers sharing a select (as process
                // lowering builds them), for `words::merge_bit_muxes`.
                let n = 1 + self.below(4);
                let sel = if n == 1 { Sig::bit(self.bit()) } else { self.sig(n) };
                for i in 0..w {
                    let bit = y.slice(i, 1);
                    let kind = if n == 1 {
                        CellKind::Mux {
                            a: self.sig(1),
                            b: self.sig(1),
                            s: sel.0[0],
                            y: bit,
                            ternary: false,
                        }
                    } else {
                        CellKind::Pmux {
                            default: self.sig(1),
                            cases: (0..n).map(|_| self.sig(1)).collect(),
                            s: sel.clone(),
                            y: bit,
                        }
                    };
                    let c = self.m.cells.len();
                    self.m.add_cell(format!("c{c}"), kind);
                }
                self.avail.push(y);
                return;
            }
            0 => {
                let op = [
                    UnaryOp::Not,
                    UnaryOp::Neg,
                    UnaryOp::ReduceAnd,
                    UnaryOp::ReduceOr,
                    UnaryOp::ReduceXor,
                    UnaryOp::ReduceXnor,
                    UnaryOp::LogicNot,
                ][self.below(7)];
                let aw = if matches!(op, UnaryOp::Not | UnaryOp::Neg) { w } else { self.width() };
                CellKind::Unary { op, a: self.sig(aw), y: y.clone() }
            }
            1..=5 => {
                let op = [
                    BinaryOp::And,
                    BinaryOp::Or,
                    BinaryOp::Xor,
                    BinaryOp::Xnor,
                    BinaryOp::Add,
                    BinaryOp::Sub,
                    BinaryOp::Mul,
                    BinaryOp::Div,
                    BinaryOp::Mod,
                    BinaryOp::Shl,
                    BinaryOp::Shr,
                    BinaryOp::Sshr,
                    BinaryOp::Eq,
                    BinaryOp::Ne,
                    BinaryOp::Lt,
                    BinaryOp::Le,
                    BinaryOp::Gt,
                    BinaryOp::Ge,
                    BinaryOp::LogicAnd,
                    BinaryOp::LogicOr,
                ][self.below(20)];
                let signed = self.below(2) == 0;
                let (aw, bw) = if op.is_shift() {
                    (w, 1 + self.below(8))
                } else if op.is_compare() {
                    let x = self.width();
                    (x, x)
                } else if op.is_logic() {
                    (self.width(), self.width())
                } else {
                    (w, w)
                };
                // Comparisons with constants, sometimes, to exercise decoders.
                let b = if op == BinaryOp::Eq && self.below(2) == 0 {
                    Sig::from_u64(self.rng.next_u64() % 8, bw)
                } else {
                    self.sig(bw)
                };
                CellKind::Binary { op, signed, a: self.sig(aw), b, y: y.clone() }
            }
            6..=7 => CellKind::Mux {
                a: self.sig(w),
                b: self.sig(w),
                s: self.bit(),
                y: y.clone(),
                ternary: self.below(2) == 0,
            },
            8 => {
                let n = 1 + self.below(5);
                CellKind::Pmux {
                    default: self.sig(w),
                    cases: (0..n).map(|_| self.sig(w)).collect(),
                    s: self.sig(n),
                    y: y.clone(),
                }
            }
            9 => {
                let aw = self.width();
                let bw = 1 + self.below(8);
                CellKind::Shiftx {
                    a: self.sig(aw),
                    b: self.sig(bw),
                    b_signed: self.below(2) == 0,
                    offset: self.below(9) as i64 - 4,
                    y: y.clone(),
                }
            }
            _ => {
                // An extract or a concatenation, through an identity.
                CellKind::Binary {
                    op: BinaryOp::Or,
                    signed: false,
                    a: self.sig(w),
                    b: Sig::zeros(w),
                    y: y.clone(),
                }
            }
        };
        let c = self.m.cells.len();
        self.m.add_cell(format!("c{c}"), kind);
        self.avail.push(y);
    }
}

/// Builds a random design.
fn design(seed: u64) -> Gen {
    let mut g = Gen {
        rng: Rng::new(seed),
        m: Module::new("top"),
        avail: Vec::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        clk: SigBit::ZERO,
    };
    let clk = g.m.add_wire("clk", 1);
    g.m.wires[clk].port = Some((PortDir::Input, 0));
    g.clk = SigBit::Wire(clk, 0);
    for i in 0..1 + g.below(4) {
        let w = g.width();
        let id = g.m.add_wire(format!("in{i}"), w as u32);
        g.m.wires[id].port = Some((PortDir::Input, 1 + i as u32));
        g.inputs.push((format!("in{i}"), w));
        g.avail.push(g.m.wire_sig(id));
    }
    // State outputs first, so that anything may read them.
    let nflops = 1 + g.below(6);
    let mut flops = Vec::new();
    for _ in 0..nflops {
        let w = g.width();
        let q = g.wire(w);
        g.avail.push(q.clone());
        flops.push(q);
    }
    let nmems = g.below(3);
    let mut mems = Vec::new();
    for mi in 0..nmems {
        let width = 1 + g.below(70) as u32;
        let abits = 1 + g.below(4);
        let depth = (1u32 << abits) - g.below(2) as u32;
        let offset = g.below(3) as i64;
        let init: Vec<LogicVec> = (0..depth).map(|_| g.constant(width as usize)).collect();
        let sync_data = g.wire(width as usize);
        g.avail.push(sync_data.clone());
        mems.push((mi, width, abits, depth, offset, init, sync_data));
    }
    let ncells = 5 + g.below(30);
    let mut async_reads = Vec::new();
    for _ in 0..ncells {
        if !mems.is_empty() && g.below(10) == 0 {
            let k = g.below(mems.len());
            let (width, abits) = (mems[k].1, mems[k].2);
            let data = g.wire(width as usize);
            let addr = g.sig(abits + 1);
            async_reads.push((k, addr, data.clone()));
            g.avail.push(data);
        } else {
            g.comb_cell();
        }
    }
    for q in flops {
        let w = q.width();
        let dff = Dff {
            clk: Clock { sig: g.clk, posedge: g.below(4) != 0 },
            d: g.sig(w),
            q,
            en: (g.below(2) == 0).then(|| g.control()),
            srst: (g.below(3) == 0).then(|| {
                let c = g.control();
                (c, g.constant(w))
            }),
            en_over_srst: g.below(2) == 0,
            // Asynchronous resets come from inputs: one driven through
            // combinational loops is glitchy and order-dependent.
            arst: (g.below(4) == 0).then(|| {
                let k = g.below(g.inputs.len());
                let (name, iw) = g.inputs[k].clone();
                let id = wire_id(&g.m, &name);
                let bit = g.below(iw) as u32;
                let c = Control { sig: SigBit::Wire(id, bit), pol: g.below(2) == 0 };
                (c, g.constant(w))
            }),
            init: g.constant(w),
        };
        let c = g.m.cells.len();
        g.m.add_cell(format!("ff{c}"), CellKind::Dff(Box::new(dff)));
    }
    for (k, (mi, width, abits, depth, offset, init, sync_data)) in mems.into_iter().enumerate() {
        let nw = 1 + g.below(2);
        let writes: Vec<MemWrite> = (0..nw)
            .map(|_| MemWrite {
                clk: Clock { sig: g.clk, posedge: true },
                en: g.sig(width as usize),
                addr: g.sig(abits + 1),
                data: g.sig(width as usize),
            })
            .collect();
        let mut reads: Vec<MemRead> = async_reads
            .iter()
            .filter(|(j, _, _)| *j == k)
            .map(|(_, addr, data)| MemRead {
                clk: None,
                en: SigBit::ONE,
                addr: addr.clone(),
                data: data.clone(),
                transparent: vec![false; nw],
            })
            .collect();
        reads.push(MemRead {
            clk: Some(Clock { sig: g.clk, posedge: true }),
            en: g.bit(),
            addr: g.sig(abits + 1),
            data: sync_data,
            transparent: (0..nw).map(|_| g.below(2) == 0).collect(),
        });
        g.m.memories.push(Memory {
            name: format!("mem{mi}"),
            width,
            depth,
            offset,
            init,
            reads,
            writes,
            alive: true,
        });
    }
    for i in 0..1 + g.below(5) {
        let w = g.width();
        let s = g.sig(w);
        let id = g.m.add_wire(format!("out{i}"), w as u32);
        g.m.wires[id].port = Some((PortDir::Output, 100 + i as u32));
        g.m.connect(g.m.wire_sig(id), s);
        g.outputs.push(format!("out{i}"));
    }
    g
}

/// The reference: evaluates every cell in order with `synth_rtl::eval`.
/// In two-state mode every unknown result bit becomes 0, as synth-sim's
/// two-state mode defines.
struct Ref<'m> {
    m: &'m Module,
    two: bool,
    vals: Vec<LogicVec>,
    mems: Vec<Vec<LogicVec>>,
    prev_clk: Logic,
}

fn known(v: &LogicVec) -> LogicVec {
    let bits: Vec<Logic> =
        v.bits().map(|b| if b == Logic::One { b } else { Logic::Zero }).collect();
    LogicVec::from_bits(&bits)
}

impl<'m> Ref<'m> {
    fn new(m: &'m Module, two: bool) -> Ref<'m> {
        let vals = m
            .wires
            .iter()
            .map(|w| {
                let x = LogicVec::xs(w.width as usize);
                if two { known(&x) } else { x }
            })
            .collect();
        let mems = m
            .memories
            .iter()
            .map(|mm| mm.init.iter().map(|v| if two { known(v) } else { v.clone() }).collect())
            .collect();
        let mut r = Ref { m, two, vals, mems, prev_clk: Logic::X };
        for c in m.cells.iter() {
            if let CellKind::Dff(d) = &c.kind {
                let init = r.fix(&d.init);
                r.set(&d.q, &init);
            }
        }
        r.settle();
        r
    }

    fn fix(&self, v: &LogicVec) -> LogicVec {
        if self.two { known(v) } else { v.clone() }
    }

    fn get(&self, s: &Sig) -> LogicVec {
        let bits: Vec<Logic> = s
            .iter()
            .map(|b| match b {
                SigBit::Const(l) => *l,
                SigBit::Wire(w, i) => self.vals[w.index()].get(*i as usize),
            })
            .collect();
        self.fix(&LogicVec::from_bits(&bits))
    }

    fn set(&mut self, s: &Sig, v: &LogicVec) {
        let v = self.fix(v);
        for (k, b) in s.iter().enumerate() {
            if let SigBit::Wire(w, i) = b {
                self.vals[w.index()].set(*i as usize, v.get(k));
            }
        }
    }

    fn bit(&self, b: SigBit) -> Logic {
        self.get(&Sig::bit(b)).get(0)
    }

    fn active(&self, c: &Control) -> bool {
        self.bit(c.sig) == Logic::from_bool(c.pol)
    }

    fn read(&self, mi: usize, addr: &LogicVec) -> LogicVec {
        let mem = &self.m.memories[MemId::new(mi)];
        match addr.to_u64() {
            Some(a) => {
                let i = a as i64 - mem.offset;
                if i >= 0 && (i as usize) < self.mems[mi].len() {
                    self.mems[mi][i as usize].clone()
                } else {
                    LogicVec::xs(mem.width as usize)
                }
            }
            None => LogicVec::xs(mem.width as usize),
        }
    }

    fn settle(&mut self) {
        loop {
            let before = self.vals.clone();
            // Cells, then asynchronous reads (each reads only what came
            // before it or state), until nothing changes.
            let m = self.m;
            for c in m.cells.iter() {
                let v = match &c.kind {
                    CellKind::Unary { op, a, y } => {
                        eval::unary(*op, &self.get(a)).resize(y.width(), false)
                    }
                    CellKind::Binary { op, signed, a, b, y } => {
                        eval::binary(*op, *signed, &self.get(a), &self.get(b))
                            .resize(y.width(), false)
                    }
                    CellKind::Mux { a, b, s, ternary, .. } => {
                        let s = self.bit(*s);
                        if !s.is_known() && !ternary {
                            self.get(a)
                        } else {
                            eval::mux(&self.get(a), &self.get(b), s)
                        }
                    }
                    CellKind::Pmux { default, cases, s, y } => {
                        let sv = self.get(s);
                        let ones: Vec<usize> =
                            (0..sv.width()).filter(|&i| sv.get(i) == Logic::One).collect();
                        if !sv.is_known() || ones.len() > 1 {
                            LogicVec::xs(y.width())
                        } else if let Some(&i) = ones.first() {
                            self.get(&cases[i])
                        } else {
                            self.get(default)
                        }
                    }
                    CellKind::Shiftx { a, b, b_signed, offset, y } => {
                        eval::shiftx(&self.get(a), &self.get(b), *b_signed, *offset, y.width())
                    }
                    _ => continue,
                };
                let y = c.outputs()[0].clone();
                self.set(&y, &v);
            }
            for (mi, mem) in m.memories.iter().enumerate() {
                for r in mem.reads.iter().filter(|r| r.clk.is_none()) {
                    let v = self.read(mi, &self.get(&r.addr));
                    self.set(&r.data, &v);
                }
            }
            for (l, r) in &m.connections {
                let v = self.get(r);
                self.set(l, &v);
            }
            for c in m.cells.iter() {
                if let CellKind::Dff(d) = &c.kind
                    && let Some((ctl, v)) = &d.arst
                    && self.active(ctl)
                {
                    let v = self.fix(v);
                    self.set(&d.q, &v);
                }
            }
            if self.vals == before {
                return;
            }
        }
    }

    fn edge(&mut self, posedge: bool) {
        let m = self.m;
        let mut next = Vec::new();
        for c in m.cells.iter() {
            let CellKind::Dff(d) = &c.kind else { continue };
            if d.clk.posedge != posedge {
                continue;
            }
            let en = d.en.as_ref().is_none_or(|e| self.active(e));
            let v = if let Some((ctl, v)) = &d.arst
                && self.active(ctl)
            {
                v.clone()
            } else if let Some((ctl, v)) = &d.srst
                && self.active(ctl)
                && (!d.en_over_srst || en)
            {
                v.clone()
            } else if en {
                self.get(&d.d)
            } else {
                self.get(&d.q)
            };
            next.push((d.q.clone(), v));
        }
        let mut writes = Vec::new();
        let mut reads = Vec::new();
        for (mi, mem) in m.memories.iter().enumerate() {
            let ws: Vec<(Option<u64>, LogicVec, LogicVec)> = mem
                .writes
                .iter()
                .filter(|w| w.clk.posedge == posedge)
                .map(|w| (self.get(&w.addr).to_u64(), self.get(&w.en), self.get(&w.data)))
                .collect();
            for r in mem.reads.iter().filter(|r| r.clk.is_some_and(|c| c.posedge == posedge)) {
                if self.bit(r.en) != Logic::One {
                    continue;
                }
                let addr = self.get(&r.addr);
                let mut v = self.read(mi, &addr);
                for (j, (wa, en, data)) in ws.iter().enumerate() {
                    if r.transparent[j] && wa.is_some() && *wa == addr.to_u64() {
                        for b in 0..v.width() {
                            if en.get(b) == Logic::One {
                                v.set(b, data.get(b));
                            }
                        }
                    }
                }
                reads.push((r.data.clone(), v));
            }
            writes.push((mi, ws));
        }
        for (mi, ws) in writes {
            let offset = m.memories[MemId::new(mi)].offset;
            for (a, en, data) in ws {
                let Some(a) = a else { continue };
                let i = a as i64 - offset;
                if i < 0 || i as usize >= self.mems[mi].len() {
                    continue;
                }
                for b in 0..en.width() {
                    if en.get(b) == Logic::One {
                        self.mems[mi][i as usize].set(b, data.get(b));
                    }
                }
            }
        }
        for (q, v) in next {
            self.set(&q, &v);
        }
        for (d, v) in reads {
            self.set(&d, &v);
        }
    }

    /// Sets the clock and settles, firing the edge it makes.
    fn clock(&mut self, v: Logic) {
        let clk = self.m.wires.iter_enumerated().find(|(_, w)| w.name == "clk").unwrap().0;
        self.vals[clk.index()] = LogicVec::from_bits(&[v]);
        self.settle();
        let prev = std::mem::replace(&mut self.prev_clk, v);
        if prev == Logic::Zero && v == Logic::One {
            self.edge(true);
            self.settle();
        } else if prev == Logic::One && v == Logic::Zero {
            self.edge(false);
            self.settle();
        }
    }

    fn poke(&mut self, w: WireId, v: &LogicVec) {
        self.vals[w.index()] = self.fix(v);
    }
}

fn sig_sim(m: &Module, sim: &Sim, s: &Sig) -> LogicVec {
    let bits: Vec<Logic> = s
        .iter()
        .map(|b| match b {
            SigBit::Const(l) => *l,
            SigBit::Wire(w, i) => {
                sim.peek_vec(&sim.signal(&m.wires[*w].name).unwrap()).get(*i as usize)
            }
        })
        .collect();
    LogicVec::from_bits(&bits)
}

/// Prints the first cell (in creation order) whose output differs.
fn diagnose(m: &Module, sim: &Sim, r: &Ref) {
    for c in m.cells.iter() {
        if let CellKind::Dff(d) = &c.kind {
            let SigBit::Wire(id, _) = d.q.0[0] else { continue };
            let s = sim.signal(&m.wires[id].name).unwrap();
            if sim.peek_vec(&s) != r.vals[id.index()] {
                eprintln!(
                    "flop {} differs: sim {:?} ref {:?}\n  {:?}",
                    m.wires[id].name,
                    sim.peek_vec(&s),
                    r.vals[id.index()],
                    d
                );
                eprintln!("  d: sim {:?} ref {:?}", sig_sim(m, sim, &d.d), r.get(&d.d));
                if let Some((c, _)) = &d.arst {
                    eprintln!(
                        "  arst: sim {:?} ref {:?}",
                        sig_sim(m, sim, &Sig::bit(c.sig)),
                        r.get(&Sig::bit(c.sig))
                    );
                }
            }
        }
    }
    for (ci, c) in m.cells.iter().enumerate() {
        let y = c.outputs()[0];
        let SigBit::Wire(id, _) = y.0[0] else { continue };
        let name = &m.wires[id].name;
        let Some(s) = sim.signal(name) else { continue };
        let (got, want) = (sim.peek_vec(&s), r.vals[id.index()].clone());
        if got != want {
            eprintln!("unstable loops: {}", sim.unstable_loops());
            eprintln!("first difference: cell {ci} {:?}\n  sim {got:?}\n  ref {want:?}", c.kind);
            if let CellKind::Pmux { default, cases, s, .. } = &c.kind {
                eprintln!("  s: sim {:?} ref {:?}", sig_sim(m, sim, s), r.get(s));
                eprintln!("  default: sim {:?} ref {:?}", sig_sim(m, sim, default), r.get(default));
                for (k, cs) in cases.iter().enumerate() {
                    eprintln!("  case {k}: sim {:?} ref {:?}", sig_sim(m, sim, cs), r.get(cs));
                }
            }
            let mut ins = Vec::new();
            c.for_each_input(|b| ins.push(b));
            for b in ins {
                if let SigBit::Wire(w, i) = b {
                    let s = sim.signal(&m.wires[w].name).unwrap();
                    eprintln!(
                        "  input {}[{i}]: sim {:?} ref {:?}",
                        m.wires[w].name,
                        sim.peek_vec(&s).get(i as usize),
                        r.vals[w.index()].get(i as usize)
                    );
                }
            }
            return;
        }
    }
    eprintln!("no cell output differs; unstable loops: {}", sim.unstable_loops());
}

fn wire_id(m: &Module, name: &str) -> WireId {
    m.wires.iter_enumerated().find(|(_, w)| w.name == name).unwrap().0
}

/// Simulates design `seed` for `cycles`, against the reference, recording
/// the input values used and the outputs after each half cycle.
fn compare(seed: u64, four: bool, cycles: usize) -> (Gen, Vec<Vec<u64>>, Vec<Vec<u64>>) {
    let g = design(seed);
    let m = &g.m;
    let mut sim = Sim::new(m, &Options { four_state: four, prune: false, optimize: false })
        .unwrap_or_else(|e| panic!("seed {seed}: {e}"));
    let mut r = Ref::new(m, !four);
    let mut rng = Rng::new(seed ^ 0x5eed);
    let clk = sim.signal("clk").unwrap();
    sim.poke(&clk, 0);
    sim.eval_quiet();
    r.clock(Logic::Zero);
    r.prev_clk = Logic::Zero;
    let ins: Vec<_> =
        g.inputs.iter().map(|(n, w)| (sim.signal(n).unwrap(), wire_id(m, n), *w)).collect();
    let outs: Vec<_> =
        g.outputs.iter().map(|n| (n.clone(), sim.signal(n).unwrap(), wire_id(m, n))).collect();
    let mut stimuli = Vec::new();
    let mut results = Vec::new();
    for cycle in 0..cycles {
        let mut stim = Vec::new();
        for (s, id, w) in &ins {
            let bits: Vec<Logic> = (0..*w)
                .map(|_| {
                    if four && rng.below(30) == 0 {
                        Logic::X
                    } else {
                        Logic::from_bool(rng.below(2) == 1)
                    }
                })
                .collect();
            let v = LogicVec::from_bits(&bits);
            stim.push(v.to_u64().unwrap_or(0));
            sim.poke_vec(s, &v);
            r.poke(*id, &v);
        }
        stimuli.push(stim);
        sim.eval();
        r.settle();
        for (id, w) in m.wires.iter_enumerated() {
            let Some(s) = sim.signal(&w.name) else { continue };
            if sim.peek_vec(&s) != r.vals[id.index()] {
                diagnose(m, &sim, &r);
                panic!(
                    "seed {seed}, {}-state, cycle {cycle}, inputs: wire {} sim {:?} ref {:?}",
                    if four { 4 } else { 2 },
                    w.name,
                    sim.peek_vec(&s),
                    r.vals[id.index()]
                );
            }
        }
        for level in [Logic::One, Logic::Zero] {
            // Flip-flop inputs before the edge, for diagnostics.
            let mut pre = Vec::new();
            for c in m.cells.iter() {
                if let CellKind::Dff(d) = &c.kind {
                    let mut sig = d.d.clone();
                    if let Some(e) = d.en {
                        sig = sig.concat(&Sig::bit(e.sig));
                    }
                    if let Some((c, _)) = &d.arst {
                        sig = sig.concat(&Sig::bit(c.sig));
                    }
                    pre.push((d.q.clone(), sig_sim(m, &sim, &sig), r.get(&sig)));
                }
            }
            sim.poke(&clk, (level == Logic::One) as u64);
            sim.eval();
            let bad = sim.unsettled();
            if !bad.is_empty() {
                let p = sim.program();
                for &i in &bad[..bad.len().min(5)] {
                    eprintln!("unsettled {i}: {:?}", p.ops[i]);
                }
                panic!(
                    "seed {seed}, cycle {cycle}, clk {level:?}: {} instructions not settled",
                    bad.len()
                );
            }
            r.clock(level);
            for (id, w) in m.wires.iter_enumerated() {
                let Some(s) = sim.signal(&w.name) else { continue };
                if sim.peek_vec(&s) != r.vals[id.index()] {
                    for (q, a, b) in &pre {
                        if let SigBit::Wire(qw, _) = q.0[0]
                            && qw == id
                        {
                            eprintln!(
                                "pre-edge inputs (d, en, arst) of {}: sim {a:?} ref {b:?}",
                                w.name
                            );
                        }
                    }
                    diagnose(m, &sim, &r);
                    panic!(
                        "seed {seed}, {}-state, cycle {cycle}, clk {level:?}: wire {} sim {:?} ref {:?}",
                        if four { 4 } else { 2 },
                        w.name,
                        sim.peek_vec(&s),
                        r.vals[id.index()]
                    );
                }
            }
            let mut res = Vec::new();
            for (n, s, id) in &outs {
                let got = sim.peek_vec(s);
                let want = r.vals[id.index()].clone();
                if got != want {
                    diagnose(m, &sim, &r);
                }
                assert_eq!(
                    got,
                    want,
                    "seed {seed}, {}-state, cycle {cycle}, clk {level:?}: output {n}",
                    if four { 4 } else { 2 }
                );
                res.push(got.to_u64().unwrap_or(0));
            }
            results.push(res);
        }
    }
    (g, stimuli, results)
}

#[test]
fn random_designs_four_state() {
    for seed in 0..300 {
        compare(seed, true, 20);
    }
}

#[test]
fn random_designs_two_state() {
    for seed in 0..300 {
        compare(seed, false, 20);
    }
}

/// The RTL optimizations (`Options::optimize`) keep the outputs.
#[test]
fn random_designs_optimized() {
    for seed in 0..300 {
        let (g, stimuli, results) = compare(seed, false, 20);
        // The stimuli record inputs up to 64 bits wide.
        if g.inputs.iter().any(|(_, w)| *w > 64) {
            continue;
        }
        let mut sim =
            Sim::new(&g.m, &Options { four_state: false, prune: false, optimize: true }).unwrap();
        let clk = sim.signal("clk").unwrap();
        sim.poke(&clk, 0);
        sim.eval_quiet();
        let ins: Vec<_> = g.inputs.iter().map(|(n, _)| sim.signal(n).unwrap()).collect();
        let outs: Vec<_> = g.outputs.iter().map(|n| sim.signal(n).unwrap()).collect();
        let mut got = Vec::new();
        for stim in &stimuli {
            for (s, &v) in ins.iter().zip(stim) {
                sim.poke(s, v);
            }
            sim.eval();
            for level in [1, 0] {
                sim.poke(&clk, level);
                sim.eval();
                got.push(
                    outs.iter().map(|s| sim.peek_vec(s).to_u64().unwrap_or(0)).collect::<Vec<_>>(),
                );
            }
        }
        assert_eq!(got, results, "seed {seed}: optimized design differs");
    }
}

/// The C backend agrees with the interpreter (when a C compiler is found).
#[test]
fn random_designs_in_c() {
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    if std::process::Command::new(&cc).arg("--version").output().is_err() {
        eprintln!("no C compiler ({cc}): skipped");
        return;
    }
    let dir = std::env::temp_dir().join(format!("synth-sim-c-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for seed in 0..60 {
        let (g, stimuli, results) = compare(seed, false, 12);
        // Inputs wider than 64 bits are driven only through the interpreter.
        if g.inputs.iter().any(|(_, w)| *w > 64)
            || g.outputs.iter().any(|n| g.m.wires.iter().find(|w| &w.name == n).unwrap().width > 64)
        {
            continue;
        }
        // Odd seeds through the RTL optimizations.
        let optimize = seed % 2 == 1;
        let sim = Sim::new(&g.m, &Options { four_state: false, prune: false, optimize }).unwrap();
        let model = synth_sim::emit_c::emit(sim.program(), "top").unwrap();
        std::fs::write(dir.join("top.h"), &model.header).unwrap();
        std::fs::write(dir.join("top.c"), &model.source).unwrap();
        let mut main = String::from(
            "#include <stdio.h>\n#include \"top.h\"\nint main(void) {\n  static top_t m;\n  top_init(&m);\n  top_set_clk(&m, 0); top_eval(&m);\n",
        );
        for stim in &stimuli {
            for ((n, _), v) in g.inputs.iter().zip(stim) {
                main.push_str(&format!("  top_set_{n}(&m, {v}ull);\n"));
            }
            for level in [1, 0] {
                main.push_str(&format!("  top_set_clk(&m, {level}); top_eval(&m);\n"));
                for n in &g.outputs {
                    main.push_str(&format!(
                        "  printf(\"%llu \", (unsigned long long)top_get_{n}(&m));\n"
                    ));
                }
                main.push_str("  printf(\"\\n\");\n");
            }
        }
        main.push_str("  return 0;\n}\n");
        std::fs::write(dir.join("main.c"), main).unwrap();
        let exe = dir.join("sim");
        let out = std::process::Command::new(&cc)
            .args(["-O1", "-w", "-o"])
            .arg(&exe)
            .arg(dir.join("main.c"))
            .arg(dir.join("top.c"))
            .output()
            .unwrap();
        assert!(out.status.success(), "seed {seed}: {}", String::from_utf8_lossy(&out.stderr));
        let run = std::process::Command::new(&exe).output().unwrap();
        let text = String::from_utf8(run.stdout).unwrap();
        let got: Vec<Vec<u64>> = text
            .lines()
            .map(|l| l.split_whitespace().map(|x| x.parse().unwrap()).collect())
            .collect();
        assert_eq!(got, results, "seed {seed}: C model differs from the interpreter");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
