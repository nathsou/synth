//! C source output: a two-state program as a C model, compiled by the
//! user's C compiler for long simulations (ADR 0008).
//!
//! The model is a struct holding the state, with `<p>_init` and `<p>_eval`
//! and an accessor per port, in the manner of a Verilator model. Inputs
//! are written through setters, which note which input changed; `eval` then
//! runs the logic each changed input reaches, fires the clock edges that
//! result and runs the logic each fired domain reaches, until no clock
//! changes. Each source (an input, a clock domain, the memories written
//! from outside) has its own function holding, in evaluation order, the
//! instructions it reaches: running several of them in sequence leaves
//! every value as a single evaluation in order would.

use std::fmt::Write as _;

use synth_rtl::{BinaryOp, PortDir, UnaryOp};

use crate::program::*;

/// A C model: a header and its implementation.
#[derive(Clone, Debug)]
pub struct CModel {
    /// The prefix of every name (`<prefix>_t`, `<prefix>_eval`...).
    pub prefix: String,
    pub header: String,
    pub source: String,
}

/// A C identifier from a name.
pub fn ident(name: &str) -> String {
    let mut s: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    if s.is_empty() || s.starts_with(|c: char| c.is_ascii_digit()) {
        s.insert(0, '_');
    }
    s
}

fn hex(x: u64) -> String {
    format!("0x{x:x}ull")
}

/// The C type holding a memory word of `w` bits (wider words use several
/// `uint64_t`).
fn mem_type(w: u32) -> (&'static str, u32) {
    match w {
        0..=8 => ("uint8_t", 1),
        9..=16 => ("uint16_t", 1),
        17..=32 => ("uint32_t", 1),
        _ => ("uint64_t", nwords(w)),
    }
}

/// Instructions reached from each source, in program order.
struct Cones {
    inputs: Vec<(u32, Vec<u32>)>,
    domains: Vec<Vec<u32>>,
    host: Vec<u32>,
}

/// Units reading the slot holding `word`.
fn readers(p: &Program, word: u32) -> &[u32] {
    let s = p.slot_of[word as usize] as usize;
    &p.rd[p.rd_start[s] as usize..p.rd_start[s + 1] as usize]
}

/// The units reached from `start` (units), in order.
fn reach(p: &Program, start: impl IntoIterator<Item = u32>) -> Vec<u32> {
    let n = p.ops.len();
    let mut seen = vec![false; n];
    let mut stack: Vec<u32> = start.into_iter().filter(|&u| (u as usize) < n).collect();
    while let Some(u) = stack.pop() {
        if std::mem::replace(&mut seen[u as usize], true) {
            continue;
        }
        let ins = &p.ops[u as usize];
        let mut push = |w: u32| {
            for &r in readers(p, w) {
                if (r as usize) < n && !seen[r as usize] {
                    stack.push(r);
                }
            }
        };
        match ins.k {
            K::Loop => {
                for &o in &p.aux.lists[ins.b as usize..(ins.b + ins.c) as usize] {
                    push(o);
                }
            }
            K::Decode => {
                for &(_, d, _) in &p.aux.decoders[ins.x as usize].entries {
                    push(d);
                }
            }
            _ => push(ins.d),
        }
    }
    (0..n as u32).filter(|&u| seen[u as usize]).collect()
}

/// The input slots the model can write: whole input ports.
fn input_ports(p: &Program) -> Vec<u32> {
    let mut slots = Vec::new();
    for &(si, dir, _) in &p.ports {
        let locs = &p.signals[si].1;
        if dir != PortDir::Output
            && !locs.is_empty()
            && locs.len() <= 64
            && let Some(Loc::Bit { word, bit: 0 }) = locs.first()
            && p.word_kind[*word as usize] == SlotKind::Input
            && p.slot_of[*word as usize] == *word
            && !slots.contains(word)
        {
            slots.push(*word);
        }
    }
    slots
}

fn cones(p: &Program) -> Cones {
    let inputs =
        input_ports(p).into_iter().map(|s| (s, reach(p, readers(p, s).iter().copied()))).collect();
    let mut domains = Vec::new();
    for d in &p.domains {
        let mut start = Vec::new();
        for ins in d.commit.iter().chain(&d.gated) {
            start.extend_from_slice(readers(p, ins.d));
        }
        for ins in &d.write {
            let mem = p.aux.writes[ins.x as usize].mem as usize;
            start.extend_from_slice(&p.mem_readers[mem]);
        }
        domains.push(reach(p, start));
    }
    let host = reach(p, p.mem_readers.iter().flatten().copied());
    Cones { inputs, domains, host }
}

/// The slots a unit writes.
fn unit_writes(p: &Program, u: u32) -> Vec<u32> {
    let ins = &p.ops[u as usize];
    match ins.k {
        K::Loop => p.aux.lists[ins.b as usize..(ins.b + ins.c) as usize]
            .iter()
            .map(|&w| p.slot_of[w as usize])
            .collect(),
        K::Decode => p.aux.decoders[ins.x as usize].entries.iter().map(|e| e.1).collect(),
        _ => vec![ins.d],
    }
}

/// Reorders `units` (in evaluation order) so that units with the same mask
/// follow each other where dependencies allow, keeping every unit after
/// those it reads.
fn cluster(p: &Program, units: &[u32], mask: &[u64]) -> Vec<u32> {
    let pos: std::collections::HashMap<u32, usize> =
        units.iter().enumerate().map(|(i, &u)| (u, i)).collect();
    let n = units.len();
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut indeg = vec![0u32; n];
    for (i, &u) in units.iter().enumerate() {
        let mut seen = Vec::new();
        for w in unit_writes(p, u) {
            for r in readers(p, w) {
                if let Some(&j) = pos.get(r)
                    && j != i
                    && !seen.contains(&j)
                {
                    seen.push(j);
                    succ[i].push(j);
                    indeg[j] += 1;
                }
            }
        }
    }
    // Ready units by mask, in order.
    let mut ready: std::collections::BTreeMap<u64, std::collections::BTreeSet<usize>> =
        Default::default();
    for i in 0..n {
        if indeg[i] == 0 {
            ready.entry(mask[units[i] as usize]).or_default().insert(i);
        }
    }
    let mut out = Vec::with_capacity(n);
    let mut cur: Option<u64> = None;
    while out.len() < n {
        let mk = match cur {
            Some(m) if ready.get(&m).is_some_and(|s| !s.is_empty()) => m,
            _ => {
                // The mask whose earliest ready unit comes first.
                *ready
                    .iter()
                    .filter(|(_, s)| !s.is_empty())
                    .min_by_key(|(_, s)| *s.iter().next().unwrap())
                    .unwrap()
                    .0
            }
        };
        let set = ready.get_mut(&mk).unwrap();
        let i = *set.iter().next().unwrap();
        set.remove(&i);
        cur = Some(mk);
        out.push(units[i]);
        for &j in &succ[i] {
            indeg[j] -= 1;
            if indeg[j] == 0 {
                ready.entry(mask[units[j] as usize]).or_default().insert(j);
            }
        }
    }
    out
}

/// What a domain's edge changes, in at most 64 groups, and which groups
/// each instruction of its cone depends on. An edge sets the bits of the
/// groups it changed; the cone then runs only what depends on them.
struct Groups {
    /// Group of each commit and gated update, then of each memory write.
    commit: Vec<u32>,
    write: Vec<u32>,
    /// Per unit (instruction index): the groups it depends on.
    mask: Vec<u64>,
}

fn bitset(n: usize, units: &[u32]) -> Vec<u64> {
    let mut b = vec![0u64; n.div_ceil(64)];
    for &u in units {
        b[u as usize / 64] |= 1 << (u % 64);
    }
    b
}

fn groups(p: &Program, d: &Domain) -> Groups {
    let n = p.ops.len();
    // Sources: each state update and each memory write, with what it reaches.
    let mut reach_of: Vec<Vec<u64>> = Vec::new();
    for ins in d.commit.iter().chain(&d.gated) {
        reach_of.push(bitset(n, &reach(p, readers(p, ins.d).iter().copied())));
    }
    for ins in &d.write {
        let mem = p.aux.writes[ins.x as usize].mem as usize;
        reach_of.push(bitset(n, &reach(p, p.mem_readers[mem].iter().copied())));
    }
    // Merge the closest groups (fewest units reached by one but not the
    // other) until at most 64 remain.
    let mut members: Vec<Vec<usize>> = (0..reach_of.len()).map(|i| vec![i]).collect();
    let mut sets = reach_of;
    let dist =
        |a: &[u64], b: &[u64]| -> u32 { a.iter().zip(b).map(|(x, y)| (x ^ y).count_ones()).sum() };
    while sets.len() > 64 {
        let mut best = (u32::MAX, 0, 1);
        for i in 0..sets.len() {
            for j in i + 1..sets.len() {
                let dd = dist(&sets[i], &sets[j]);
                if dd < best.0 {
                    best = (dd, i, j);
                }
            }
        }
        let (_, i, j) = best;
        let sj = sets.swap_remove(j);
        let mj = members.swap_remove(j);
        for (a, b) in sets[i].iter_mut().zip(sj) {
            *a |= b;
        }
        members[i].extend(mj);
    }
    let nsrc = d.commit.len() + d.gated.len() + d.write.len();
    let mut group_of = vec![0u32; nsrc];
    let mut mask = vec![0u64; n];
    for (g, (ms, set)) in members.iter().zip(&sets).enumerate() {
        for &m in ms {
            group_of[m] = g as u32;
        }
        for u in 0..n {
            if set[u / 64] >> (u % 64) & 1 != 0 {
                mask[u] |= 1 << g;
            }
        }
    }
    let nc = d.commit.len() + d.gated.len();
    Groups { commit: group_of[..nc].to_vec(), write: group_of[nc..].to_vec(), mask }
}

struct Emitter<'p> {
    p: &'p Program,
    prefix: String,
    out: String,
    /// Temporaries for loops.
    tmp: usize,
    /// Set by a memory write when it writes: (domain, group).
    on_write: Option<(usize, u32)>,
}

/// `v[i]` for a word.
fn v(i: u32) -> String {
    format!("v[{i}]")
}

impl Emitter<'_> {
    fn line(&mut self, s: &str) {
        self.out.push_str("    ");
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn copy(&mut self, d: u32, a: u32, w: u32) {
        if d == a {
            return;
        }
        for j in 0..nwords(w) {
            self.line(&format!("{} = {};", v(d + j), v(a + j)));
        }
    }

    fn ctl(c_word: u32, x: u64) -> String {
        let bit = x & 63;
        let pol = x >> 8 & 1;
        if pol == 1 {
            format!("({} >> {bit} & 1)", v(c_word))
        } else {
            format!("!({} >> {bit} & 1)", v(c_word))
        }
    }

    fn ctl_of(c: Ctl) -> String {
        Self::ctl(c.word, c.bit as u64 | (c.pol as u64) << 8)
    }

    /// Emits one instruction (not a loop).
    fn ins(&mut self, ins: &Ins) -> Result<(), String> {
        let p = self.p;
        let (d, a, b, w) = (ins.d, ins.a, ins.b, ins.w);
        let m = hex(mask(w));
        let bin = |e: &str| format!("{} = {e};", v(d));
        match ins.k {
            K::Copy => self.copy(d, a, w),
            K::Gather => {
                let n = nwords(w);
                let mut words: Vec<Vec<String>> = vec![Vec::new(); n as usize];
                if ins.c != NONE {
                    for j in 0..n {
                        words[j as usize].push(v(ins.c + j));
                    }
                }
                for part in &p.aux.parts[a as usize..(a + b) as usize] {
                    let mut e = v(part.src);
                    if part.lo != 0 {
                        e = format!("({e} >> {})", part.lo);
                    }
                    if part.lo as u32 + part.len as u32 != 64 {
                        e = format!("({e} & {})", hex(mask(part.len as u32)));
                    }
                    if part.pos != 0 {
                        e = format!("({e} << {})", part.pos);
                    }
                    words[part.dst as usize].push(e);
                }
                for (j, ws) in words.into_iter().enumerate() {
                    let e = if ws.is_empty() { "0".to_string() } else { ws.join(" | ") };
                    self.line(&format!("{} = {e};", v(d + j as u32)));
                }
            }
            K::Extract => {
                let e = if ins.x == 0 {
                    format!("{} & {m}", v(a))
                } else {
                    format!("{} >> {} & {m}", v(a), ins.x)
                };
                self.line(&bin(&e));
            }
            K::Not => self.line(&bin(&format!("~{} & {m}", v(a)))),
            K::Neg => self.line(&bin(&format!("(0 - {}) & {m}", v(a)))),
            K::RedAnd => self.line(&bin(&format!("{} == {m}", v(a)))),
            K::RedOr => self.line(&bin(&format!("{} != 0", v(a)))),
            K::RedXor => self.line(&bin(&format!("(uint64_t)__builtin_parityll({})", v(a)))),
            K::RedXnor => self.line(&bin(&format!("(uint64_t)!__builtin_parityll({})", v(a)))),
            K::LogicNot => self.line(&bin(&format!("{} == 0", v(a)))),
            K::And => self.line(&bin(&format!("{} & {}", v(a), v(b)))),
            K::Or => self.line(&bin(&format!("{} | {}", v(a), v(b)))),
            K::Xor => self.line(&bin(&format!("{} ^ {}", v(a), v(b)))),
            K::Xnor => self.line(&bin(&format!("~({} ^ {}) & {m}", v(a), v(b)))),
            K::Add => self.line(&bin(&format!("({} + {}) & {m}", v(a), v(b)))),
            K::Sub => self.line(&bin(&format!("({} - {}) & {m}", v(a), v(b)))),
            K::Mul => self.line(&bin(&format!("({} * {}) & {m}", v(a), v(b)))),
            K::DivU => self.line(&bin(&format!("{1} ? {0} / {1} : 0", v(a), v(b)))),
            K::ModU => self.line(&bin(&format!("{1} ? {0} % {1} : 0", v(a), v(b)))),
            K::DivS => self.line(&bin(&format!("sim_divs({}, {}, {w})", v(a), v(b)))),
            K::ModS => self.line(&bin(&format!("sim_mods({}, {}, {w})", v(a), v(b)))),
            K::Shl => self.line(&bin(&format!("{1} >= {w} ? 0 : {0} << {1} & {m}", v(a), v(b)))),
            K::Shr => self.line(&bin(&format!("{1} >= {w} ? 0 : {0} >> {1}", v(a), v(b)))),
            K::Sshr => self.line(&bin(&format!(
                "(uint64_t)(sim_sx({}, {w}) >> ({1} < 63 ? {1} : 63)) & {m}",
                v(a),
                v(b)
            ))),
            K::Eq | K::Eqx => self.line(&bin(&format!("{} == {}", v(a), v(b)))),
            K::Ne | K::Nex => self.line(&bin(&format!("{} != {}", v(a), v(b)))),
            K::LtU => self.line(&bin(&format!("{} < {}", v(a), v(b)))),
            K::LeU => self.line(&bin(&format!("{} <= {}", v(a), v(b)))),
            K::GtU => self.line(&bin(&format!("{} > {}", v(a), v(b)))),
            K::GeU => self.line(&bin(&format!("{} >= {}", v(a), v(b)))),
            K::LtS | K::LeS | K::GtS | K::GeS => {
                let op = match ins.k {
                    K::LtS => "<",
                    K::LeS => "<=",
                    K::GtS => ">",
                    _ => ">=",
                };
                self.line(&bin(&format!("sim_sx({}, {w}) {op} sim_sx({}, {w})", v(a), v(b))));
            }
            K::LAnd => self.line(&bin(&format!("{} != 0 && {} != 0", v(a), v(b)))),
            K::LOr => self.line(&bin(&format!("{} != 0 || {} != 0", v(a), v(b)))),
            K::Mux => {
                let s = format!("({} >> {} & 1)", v(ins.c), ins.x & 63);
                if nwords(w) == 1 {
                    self.line(&bin(&format!("{s} ? {} : {}", v(b), v(a))));
                } else {
                    self.line(&format!("if ({s}) {{"));
                    self.copy(d, b, w);
                    self.line("} else {");
                    self.copy(d, a, w);
                    self.line("}");
                }
            }
            K::Pmux => {
                let sw = ins.c;
                let cases: Vec<u32> =
                    p.aux.lists[ins.x as usize..ins.x as usize + sw as usize].to_vec();
                if sw > 64 {
                    return Err("one-hot multiplexers with more than 64 cases".into());
                }
                let n = nwords(w);
                self.line(&format!("{{ uint64_t s = {} & {};", v(b), hex(mask(sw))));
                self.line("if (s == 0) {");
                self.copy(d, a, w);
                self.line("} else if (s & (s - 1)) {");
                for j in 0..n {
                    self.line(&format!("{} = 0;", v(d + j)));
                }
                self.line("} else switch (__builtin_ctzll(s)) {");
                for (i, &c) in cases.iter().enumerate() {
                    self.line(&format!("case {i}:"));
                    self.copy(d, c, w);
                    self.line("break;");
                }
                self.line("} }");
            }
            K::Shiftx => {
                let info = &p.aux.shiftx[ins.c as usize];
                let idx = if info.b_signed {
                    format!("(int64_t)sim_sx({}, {})", v(info.b.off), info.b.w)
                } else {
                    format!("(int64_t){}", v(info.b.off))
                };
                self.line(&bin(&format!(
                    "sim_extract(&{}, {}, {}, {idx} + ({}), {w})",
                    v(info.a.off),
                    info.a.words(),
                    info.a.w,
                    info.offset
                )));
            }
            K::MemRd => self.mem_read(ins.c, a, d, w),
            K::Latch | K::Arst => {
                self.line(&format!("if ({}) {{", Self::ctl(ins.c, ins.x)));
                self.copy(d, a, w);
                self.line("}");
            }
            K::FfEn => {
                self.line(&format!("if ({}) {{", Self::ctl(ins.c, ins.x)));
                self.copy(d, a, w);
                if d != b {
                    self.line("} else {");
                    self.copy(d, b, w);
                }
                self.line("}");
            }
            K::Ff => {
                let f = p.aux.flops[ins.x as usize].clone();
                let en = f.en.map_or("1".to_string(), Self::ctl_of);
                let mut first = true;
                let mut branch = |e: &mut Self, cond: String, src: u32| {
                    let kw = if first { "if" } else { "} else if" };
                    first = false;
                    e.line(&format!("{kw} ({cond}) {{"));
                    e.copy(f.dst, src, f.w);
                };
                if let Some((c, val)) = f.arst {
                    branch(self, Self::ctl_of(c), val);
                }
                if let Some((c, val)) = f.srst {
                    let cond = if f.en_over_srst {
                        format!("{} && {en}", Self::ctl_of(c))
                    } else {
                        Self::ctl_of(c)
                    };
                    branch(self, cond, val);
                }
                branch(self, en, f.d);
                if f.dst != f.q {
                    self.line("} else {");
                    self.copy(f.dst, f.q, f.w);
                }
                self.line("}");
            }
            K::MemWr => {
                let wr = p.aux.writes[ins.x as usize].clone();
                let mem = &p.aux.mems[wr.mem as usize];
                let (ty, nw) = mem_type(mem.width);
                let mi = wr.mem;
                self.line(&format!(
                    "{{ uint64_t i = {} - ({}); if (i < {}) {{",
                    v(wr.addr.off),
                    mem.offset,
                    mem.depth
                ));
                for j in 0..nw {
                    let e = v(wr.en.off + j);
                    let dv = v(wr.data.off + j);
                    let el = if nw == 1 {
                        format!("m->mem{mi}[i]")
                    } else {
                        format!("m->mem{mi}[i * {nw} + {j}]")
                    };
                    self.line(&format!("{el} = ({ty})(({el} & ~{e}) | ({dv} & {e}));"));
                }
                if let Some((dom, g)) = self.on_write {
                    self.line(&format!("m->changed[{dom}] |= {};", hex(1u64 << g)));
                }
                self.line("} }");
            }
            K::MemRdSync => {
                let r = p.aux.sync_reads[ins.x as usize].clone();
                let mem_w = p.aux.mems[r.mem as usize].width;
                if let Some(en) = r.en {
                    self.line(&format!("if ({}) {{", Self::ctl_of(en)));
                } else {
                    self.line("{");
                }
                self.mem_read(r.mem, r.addr.off, r.dst, mem_w);
                for &wi in &r.transparent {
                    let wr = p.aux.writes[wi as usize].clone();
                    self.line(&format!("if ({} == {}) {{", v(wr.addr.off), v(r.addr.off)));
                    for j in 0..nwords(mem_w) {
                        let e = v(wr.en.off + j);
                        self.line(&format!(
                            "{0} = ({0} & ~{e}) | ({1} & {e});",
                            v(r.dst + j),
                            v(wr.data.off + j)
                        ));
                    }
                    self.line("}");
                }
                self.line("} else {");
                self.copy(r.dst, r.q, mem_w);
                self.line("}");
            }
            K::Decode => {
                let dec = &p.aux.decoders[ins.x as usize];
                let x = v(a);
                for &(key, dd, ne) in &dec.entries {
                    let op = if ne { "!=" } else { "==" };
                    self.line(&format!("{} = {x} {op} {};", v(dd), hex(key)));
                }
            }
            K::Wide => self.wide(ins)?,
            K::Loop => unreachable!("loops are emitted as units"),
        }
        Ok(())
    }

    fn mem_read(&mut self, mi: u32, addr: u32, d: u32, w: u32) {
        let mem = &self.p.aux.mems[mi as usize];
        let (_, nw) = mem_type(mem.width);
        self.line(&format!(
            "{{ uint64_t i = {} - ({}); if (i < {}) {{",
            v(addr),
            mem.offset,
            mem.depth
        ));
        if nw == 1 && mem.width <= 64 {
            self.line(&format!("{} = m->mem{mi}[i];", v(d)));
        } else {
            for j in 0..nw {
                self.line(&format!("{} = m->mem{mi}[i * {nw} + {j}];", v(d + j)));
            }
        }
        self.line("} else {");
        for j in 0..nwords(w) {
            self.line(&format!("{} = 0;", v(d + j)));
        }
        self.line("} }");
    }

    /// Operations on wide values, through the helpers of the prelude.
    fn wide(&mut self, ins: &Ins) -> Result<(), String> {
        let wi = self.p.aux.wide[ins.x as usize].clone();
        let (d, a, b) = (wi.d, wi.a, wi.b);
        let args = |f: &str| {
            format!("{f}(&{}, {}, &{}, {}, &{}, {});", v(d.off), d.w, v(a.off), a.w, v(b.off), b.w)
        };
        let call = match &wi.op {
            WideOp::Unary(op) => {
                let f = match op {
                    UnaryOp::Not => "sim_wnot",
                    UnaryOp::Neg => "sim_wneg",
                    UnaryOp::ReduceAnd => "sim_wredand",
                    UnaryOp::ReduceOr => "sim_wredor",
                    UnaryOp::ReduceXor => "sim_wredxor",
                    UnaryOp::ReduceXnor => "sim_wredxnor",
                    UnaryOp::LogicNot => "sim_wlognot",
                };
                args(f)
            }
            WideOp::Binary(op, signed) => {
                let f = match (op, signed) {
                    (BinaryOp::And, _) => "sim_wand",
                    (BinaryOp::Or, _) => "sim_wor",
                    (BinaryOp::Xor, _) => "sim_wxor",
                    (BinaryOp::Xnor, _) => "sim_wxnor",
                    (BinaryOp::Add, _) => "sim_wadd",
                    (BinaryOp::Sub, _) => "sim_wsub",
                    (BinaryOp::Mul, _) => "sim_wmul",
                    (BinaryOp::Div, false) => "sim_wdivu",
                    (BinaryOp::Mod, false) => "sim_wmodu",
                    (BinaryOp::Div, true) => "sim_wdivs",
                    (BinaryOp::Mod, true) => "sim_wmods",
                    (BinaryOp::Shl, _) => "sim_wshl",
                    (BinaryOp::Shr, _) => "sim_wshr",
                    (BinaryOp::Sshr, _) => "sim_wsshr",
                    (BinaryOp::Eq | BinaryOp::Eqx, _) => "sim_weq",
                    (BinaryOp::Ne | BinaryOp::Nex, _) => "sim_wne",
                    (BinaryOp::Lt, false) => "sim_wltu",
                    (BinaryOp::Le, false) => "sim_wleu",
                    (BinaryOp::Gt, false) => "sim_wgtu",
                    (BinaryOp::Ge, false) => "sim_wgeu",
                    (BinaryOp::Lt, true) => "sim_wlts",
                    (BinaryOp::Le, true) => "sim_wles",
                    (BinaryOp::Gt, true) => "sim_wgts",
                    (BinaryOp::Ge, true) => "sim_wges",
                    (BinaryOp::LogicAnd, _) => "sim_wland",
                    (BinaryOp::LogicOr, _) => "sim_wlor",
                };
                args(f)
            }
            WideOp::Shiftx { b_signed, offset } => format!(
                "sim_wshiftx(&{}, {}, &{}, {}, &{}, {}, {}, {offset});",
                v(d.off),
                d.w,
                v(a.off),
                a.w,
                v(b.off),
                b.w,
                *b_signed as u32
            ),
        };
        self.line(&call);
        Ok(())
    }

    /// Emits the units of a domain's cone, each run of units depending on
    /// the same groups under one test of what changed.
    fn gated_units(&mut self, units: &[u32], dom: usize, mask: &[u64]) -> Result<(), String> {
        self.line(&format!("uint64_t c = m->changed[{dom}];"));
        self.line(&format!("m->changed[{dom}] = 0;"));
        let mut i = 0;
        while i < units.len() {
            let mk = mask[units[i] as usize];
            let mut j = i;
            while j < units.len() && mask[units[j] as usize] == mk {
                j += 1;
            }
            self.line(&format!("if (c & {}) {{", hex(mk)));
            self.units(&units[i..j])?;
            self.line("}");
            i = j;
        }
        Ok(())
    }

    /// Emits the units `units` (combinational, in order).
    fn units(&mut self, units: &[u32]) -> Result<(), String> {
        let p = self.p;
        for &u in units {
            let ins = p.ops[u as usize];
            if ins.k != K::Loop {
                self.ins(&ins)?;
                continue;
            }
            let outs: Vec<u32> = p.aux.lists[ins.b as usize..(ins.b + ins.c) as usize].to_vec();
            let t = self.tmp;
            self.tmp += 1;
            self.line(&format!("{{ uint64_t t{t}[{}]; int it{t};", outs.len()));
            self.line(&format!("for (it{t} = 0; it{t} < 256; it{t}++) {{"));
            for (k, &o) in outs.iter().enumerate() {
                self.line(&format!("t{t}[{k}] = {};", v(o)));
            }
            for k in 0..ins.a {
                let body = p.ops[(u + 1 + k) as usize];
                self.ins(&body)?;
            }
            let same: Vec<String> =
                outs.iter().enumerate().map(|(k, &o)| format!("t{t}[{k}] == {}", v(o))).collect();
            self.line(&format!("if ({}) break;", same.join(" && ")));
            self.line("} }");
        }
        Ok(())
    }

    fn function(
        &mut self,
        name: &str,
        body: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<(), String> {
        let pre = self.prefix.clone();
        writeln!(self.out, "static void {name}({pre}_t *m) {{").unwrap();
        self.line("uint64_t *v = m->v; (void)v;");
        body(self)?;
        self.out.push_str("}\n\n");
        Ok(())
    }
}

/// The helpers every model uses.
const PRELUDE: &str = include_str!("prelude.c");

/// Emits a two-state program as a C model named `prefix`.
pub fn emit(p: &Program, prefix: &str) -> Result<CModel, String> {
    if p.four_state {
        return Err("the C backend simulates two-state programs only".into());
    }
    let prefix = ident(prefix);
    let cones = cones(p);
    let pre = &prefix;
    let ninputs = cones.inputs.len();
    let (iw, dw) = (ninputs.div_ceil(64).max(1), p.domains.len().div_ceil(64).max(1));

    // Header: the model struct and its interface.
    let mut h = String::new();
    let guard = format!("{}_H", pre.to_uppercase());
    writeln!(h, "/* Generated by synth (synth-sim C backend). */").unwrap();
    writeln!(h, "#ifndef {guard}\n#define {guard}\n\n#include <stdint.h>\n").unwrap();
    writeln!(h, "typedef struct {pre} {{").unwrap();
    writeln!(h, "    uint64_t v[{}];", p.init_val.len().max(1)).unwrap();
    for (i, mem) in p.aux.mems.iter().enumerate() {
        let (ty, nw) = mem_type(mem.width);
        writeln!(
            h,
            "    {ty} mem{i}[{}]; /* {} */",
            (mem.depth as u64 * nw as u64).max(1),
            mem.name
        )
        .unwrap();
    }
    writeln!(h, "    uint8_t clk[{}];", p.clocks.len().max(1)).unwrap();
    writeln!(h, "    uint64_t changed_inputs[{iw}];\n    uint8_t memories_written;").unwrap();
    writeln!(
        h,
        "    uint64_t changed[{}]; /* groups changed by each domain's last edge */",
        p.domains.len().max(1)
    )
    .unwrap();
    writeln!(h, "}} {pre}_t;\n").unwrap();
    writeln!(h, "/* Sets the initial state and settles it. */\nvoid {pre}_init({pre}_t *m);")
        .unwrap();
    writeln!(
        h,
        "/* Settles after inputs changed and runs the clock edges that result. */\nvoid {pre}_eval({pre}_t *m);"
    )
    .unwrap();
    writeln!(
        h,
        "/* Call after writing memories directly (m->memN), before eval. */\nstatic inline void {pre}_memories_written({pre}_t *m) {{ m->memories_written = 1; }}\n"
    )
    .unwrap();
    for (i, mem) in p.aux.mems.iter().enumerate() {
        writeln!(h, "#define {pre}_MEM_{} mem{i}", ident(&mem.name).to_uppercase()).unwrap();
    }
    let mut ports = p.ports.clone();
    ports.sort_by_key(|x| x.2);
    let input_bit = |slot: u32| cones.inputs.iter().position(|(s, _)| *s == slot);
    let _ = dw;
    for (si, dir, _) in &ports {
        let (name, locs) = &p.signals[*si];
        let id = ident(name);
        let w = locs.len();
        if w == 0 || w > 64 {
            continue;
        }
        // Getter: gather the bits.
        let mut parts: Vec<String> = Vec::new();
        let mut k = 0;
        while k < w {
            match locs[k] {
                Loc::Const(c) => {
                    if c == synth_core::Logic::One {
                        parts.push(format!("(1ull << {k})"));
                    }
                    k += 1;
                }
                Loc::Bit { word, bit } => {
                    let mut len = 1;
                    while k + len < w
                        && bit as usize + len < 64
                        && locs[k + len] == (Loc::Bit { word, bit: bit + len as u8 })
                    {
                        len += 1;
                    }
                    let mut e = format!("m->v[{word}]");
                    if bit != 0 {
                        e = format!("({e} >> {bit})");
                    }
                    e = format!("({e} & {})", hex(mask(len as u32)));
                    if k != 0 {
                        e = format!("({e} << {k})");
                    }
                    parts.push(e);
                    k += len;
                }
            }
        }
        let body = if parts.is_empty() { "0".to_string() } else { parts.join(" | ") };
        writeln!(h, "static inline uint64_t {pre}_get_{id}(const {pre}_t *m) {{ return {body}; }}")
            .unwrap();
        // Setter for inputs that are one whole input slot.
        if *dir != PortDir::Output
            && let Some(Loc::Bit { word, bit: 0 }) = locs.first()
            && p.word_kind[*word as usize] == SlotKind::Input
            && let Some(bit) = input_bit(*word)
        {
            writeln!(
                h,
                "static inline void {pre}_set_{id}({pre}_t *m, uint64_t x) {{ x &= {}; if (m->v[{word}] != x) {{ m->v[{word}] = x; m->changed_inputs[{}] |= {}; }} }}",
                hex(mask(w as u32)),
                bit / 64,
                hex(1u64 << (bit % 64))
            )
            .unwrap();
        }
    }
    writeln!(h, "\n#endif").unwrap();

    // Implementation.
    let mut e = Emitter { p, prefix: prefix.clone(), out: String::new(), tmp: 0, on_write: None };
    writeln!(e.out, "/* Generated by synth (synth-sim C backend). */").unwrap();
    writeln!(e.out, "#include \"{pre}.h\"\n#include <string.h>\n").unwrap();
    e.out.push_str(PRELUDE);
    e.out.push('\n');
    // Initial values.
    let nz: Vec<(usize, u64)> =
        p.init_val.iter().enumerate().filter(|(_, x)| **x != 0).map(|(i, x)| (i, *x)).collect();
    writeln!(e.out, "static const uint32_t init_at[{}] = {{", nz.len().max(1)).unwrap();
    for c in nz.chunks(16) {
        let s: Vec<String> = c.iter().map(|(i, _)| i.to_string()).collect();
        writeln!(e.out, "    {},", s.join(", ")).unwrap();
    }
    if nz.is_empty() {
        e.out.push_str("    0\n");
    }
    writeln!(e.out, "}};\nstatic const uint64_t init_val[{}] = {{", nz.len().max(1)).unwrap();
    for c in nz.chunks(8) {
        let s: Vec<String> = c.iter().map(|(_, x)| hex(*x)).collect();
        writeln!(e.out, "    {},", s.join(", ")).unwrap();
    }
    if nz.is_empty() {
        e.out.push_str("    0\n");
    }
    e.out.push_str("};\n\n");
    for (i, mem) in p.aux.mems.iter().enumerate() {
        let n = nwords(mem.width) as usize;
        let (_, nw) = mem_type(mem.width);
        let words = &p.mem_init_val[mem.base..mem.base + mem.depth as usize * n];
        let nz: Vec<(usize, u64)> =
            words.iter().enumerate().filter(|(_, x)| **x != 0).map(|(i, x)| (i, *x)).collect();
        if nz.is_empty() {
            continue;
        }
        // Words of the memory buffer map to elements one to one when the
        // element is a 64-bit word or narrower.
        let _ = nw;
        writeln!(e.out, "static const uint32_t mem{i}_at[{}] = {{", nz.len()).unwrap();
        for c in nz.chunks(16) {
            let s: Vec<String> = c.iter().map(|(i, _)| i.to_string()).collect();
            writeln!(e.out, "    {},", s.join(", ")).unwrap();
        }
        writeln!(e.out, "}};\nstatic const uint64_t mem{i}_val[{}] = {{", nz.len()).unwrap();
        for c in nz.chunks(8) {
            let s: Vec<String> = c.iter().map(|(_, x)| hex(*x)).collect();
            writeln!(e.out, "    {},", s.join(", ")).unwrap();
        }
        e.out.push_str("};\n\n");
    }

    // Logic.
    let mut member = vec![false; p.ops.len()];
    for (i, ins) in p.ops.iter().enumerate() {
        if ins.k == K::Loop {
            for m in &mut member[i + 1..=i + ins.a as usize] {
                *m = true;
            }
        }
    }
    let all: Vec<u32> = (0..p.ops.len() as u32).filter(|&u| !member[u as usize]).collect();
    e.function("settle_all", |e| e.units(&all))?;
    for (k, (_, units)) in cones.inputs.iter().enumerate() {
        e.function(&format!("settle_input{k}"), |e| e.units(units))?;
    }
    let groups: Vec<Groups> = p.domains.iter().map(|d| groups(p, d)).collect();
    for (k, units) in cones.domains.iter().enumerate() {
        let mask = &groups[k].mask;
        let units = cluster(p, units, mask);
        e.function(&format!("settle_domain{k}"), |e| e.gated_units(&units, k, mask))?;
    }
    e.function("settle_host", |e| e.units(&cones.host))?;
    for (k, d) in p.domains.iter().enumerate() {
        let d = d.clone();
        let g = &groups[k];
        e.function(&format!("sample{k}"), |e| d.sample.iter().try_for_each(|i| e.ins(i)))?;
        e.function(&format!("write{k}"), |e| {
            for (i, ins) in d.write.iter().enumerate() {
                e.on_write = Some((k, g.write[i]));
                e.ins(ins)?;
            }
            e.on_write = None;
            Ok(())
        })?;
        e.function(&format!("commit{k}"), |e| {
            for (i, ins) in d.commit.iter().chain(&d.gated).enumerate() {
                let n = nwords(ins.w);
                e.line("{");
                for j in 0..n {
                    e.line(&format!("uint64_t o{j} = {};", v(ins.d + j)));
                }
                e.ins(ins)?;
                let diff: Vec<String> =
                    (0..n).map(|j| format!("o{j} != {}", v(ins.d + j))).collect();
                e.line(&format!(
                    "if ({}) m->changed[{k}] |= {};",
                    diff.join(" || "),
                    hex(1u64 << g.commit[i])
                ));
                e.line("}");
            }
            Ok(())
        })?;
    }

    // Interface.
    writeln!(e.out, "void {pre}_init({pre}_t *m) {{").unwrap();
    writeln!(e.out, "    unsigned i;\n    memset(m, 0, sizeof *m);").unwrap();
    writeln!(
        e.out,
        "    for (i = 0; i < sizeof init_at / sizeof init_at[0]; i++) m->v[init_at[i]] |= init_val[i];"
    )
    .unwrap();
    for (i, mem) in p.aux.mems.iter().enumerate() {
        let words =
            &p.mem_init_val[mem.base..mem.base + mem.depth as usize * nwords(mem.width) as usize];
        if words.iter().any(|&x| x != 0) {
            let (ty, _) = mem_type(mem.width);
            writeln!(
                e.out,
                "    for (i = 0; i < sizeof mem{i}_at / sizeof mem{i}_at[0]; i++) m->mem{i}[mem{i}_at[i]] = ({ty})mem{i}_val[i];"
            )
            .unwrap();
        }
    }
    writeln!(e.out, "    settle_all(m);").unwrap();
    for (k, c) in p.clocks.iter().enumerate() {
        writeln!(e.out, "    m->clk[{k}] = m->v[{}] >> {} & 1;", c.word, c.bit).unwrap();
    }
    writeln!(e.out, "    memset(m->changed_inputs, 0, sizeof m->changed_inputs);\n    m->memories_written = 0;\n}}\n").unwrap();

    writeln!(e.out, "void {pre}_eval({pre}_t *m) {{").unwrap();
    writeln!(e.out, "    int round;").unwrap();
    writeln!(e.out, "    for (round = 0; round < 10000; round++) {{").unwrap();
    writeln!(e.out, "        uint64_t fired[{dw}] = {{0}}, in[{iw}], any = 0;").unwrap();
    writeln!(e.out, "        memcpy(in, m->changed_inputs, sizeof in);").unwrap();
    writeln!(e.out, "        memset(m->changed_inputs, 0, sizeof in);").unwrap();
    for k in 0..ninputs {
        writeln!(
            e.out,
            "        if (in[{}] & {}) settle_input{k}(m);",
            k / 64,
            hex(1u64 << (k % 64))
        )
        .unwrap();
    }
    writeln!(
        e.out,
        "        if (m->memories_written) {{ m->memories_written = 0; settle_host(m); }}"
    )
    .unwrap();
    for (k, c) in p.clocks.iter().enumerate() {
        writeln!(e.out, "        {{ uint8_t c = m->v[{}] >> {} & 1;", c.word, c.bit).unwrap();
        writeln!(e.out, "          if (c != m->clk[{k}]) {{ m->clk[{k}] = c;").unwrap();
        if let Some(d) = c.pos {
            writeln!(
                e.out,
                "            if (c) {{ fired[{}] |= {}; any = 1; }}",
                d / 64,
                hex(1u64 << (d % 64))
            )
            .unwrap();
        }
        if let Some(d) = c.neg {
            writeln!(
                e.out,
                "            if (!c) {{ fired[{}] |= {}; any = 1; }}",
                d / 64,
                hex(1u64 << (d % 64))
            )
            .unwrap();
        }
        writeln!(e.out, "        }} }}").unwrap();
    }
    writeln!(e.out, "        if (!any) return;").unwrap();
    for phase in ["sample", "write", "commit", "settle_domain"] {
        for k in 0..p.domains.len() {
            writeln!(
                e.out,
                "        if (fired[{}] & {}) {phase}{k}(m);",
                k / 64,
                hex(1u64 << (k % 64))
            )
            .unwrap();
        }
    }
    writeln!(e.out, "    }}\n}}").unwrap();
    Ok(CModel { prefix, header: h, source: e.out })
}
