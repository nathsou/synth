//! C source output: a two-state program as a C model, compiled by the
//! user's C compiler for long simulations (ADR 0008).
//!
//! The model is a struct holding the state, with `<p>_init` and `<p>_eval`
//! and an accessor per port, in the manner of a Verilator model.
//!
//! The combinational logic is cut into partitions, in the manner of
//! ESSENT: each value read by exactly one instruction (and nothing else) is
//! computed in a C local by the partition of its reader, so a partition is
//! a tree of instructions whose root alone writes the state. A multiplexer
//! computes only the input it selects, which turns the trees of the
//! processes' assignments back into branches. Each partition has a dirty
//! bit: it runs only after one of its inputs changed, and marks the
//! partitions reading its output when that changes. Inputs, state updates
//! and memory writes mark the partitions reading them.

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

/// Units reading the slot holding `word`.
fn readers(p: &Program, word: u32) -> &[u32] {
    let s = p.slot_of[word as usize] as usize;
    &p.rd[p.rd_start[s] as usize..p.rd_start[s + 1] as usize]
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

/// Slots read by the instructions of clock domains.
fn seq_reads(p: &Program, ins: &Ins, out: &mut Vec<u32>) {
    let s = |w: u32| p.slot_of[w as usize];
    match ins.k {
        K::Copy => out.push(s(ins.a)),
        K::FfEn => out.extend([s(ins.a), s(ins.b), s(ins.c)]),
        K::Ff => {
            let f = &p.aux.flops[ins.x as usize];
            out.extend([s(f.d), s(f.q)]);
            out.extend(f.en.map(|c| s(c.word)));
            out.extend(f.srst.map(|(c, _)| s(c.word)));
            out.extend(f.arst.map(|(c, _)| s(c.word)));
        }
        K::MemWr => {
            let w = &p.aux.writes[ins.x as usize];
            out.extend([w.addr.off, w.data.off, w.en.off]);
        }
        K::MemRdSync => {
            let r = &p.aux.sync_reads[ins.x as usize];
            out.extend([r.addr.off, r.q]);
            out.extend(r.en.map(|c| s(c.word)));
            for &wi in &r.transparent {
                let w = &p.aux.writes[wi as usize];
                out.extend([w.addr.off, w.data.off, w.en.off]);
            }
        }
        _ => {}
    }
}

/// Instructions whose single-word result can live in a C local.
fn simple(k: K) -> bool {
    !matches!(
        k,
        K::Loop
            | K::Decode
            | K::Wide
            | K::Latch
            | K::Arst
            | K::FfEn
            | K::Ff
            | K::MemWr
            | K::MemRdSync
    )
}

/// The partitions of the logic.
struct Plan {
    /// Per word: computed in a C local of its partition.
    local: Vec<bool>,
    /// Partitions in evaluation order: their units, ascending, root last.
    parts: Vec<Vec<u32>>,
    /// Per unit: its partition (`NONE` for members of loops).
    part_of: Vec<u32>,
    /// Per unit: the units whose local results it reads.
    children: Vec<Vec<u32>>,
}

fn plan(p: &Program) -> Plan {
    let n = p.ops.len();
    let words = p.init_val.len();
    let mut member = vec![false; n];
    for (i, ins) in p.ops.iter().enumerate() {
        if ins.k == K::Loop {
            for m in &mut member[i + 1..=i + ins.a as usize] {
                *m = true;
            }
        }
    }
    // What each slot must be: shared state unless a single simple
    // instruction reads it and a simple instruction writes one word of it.
    let mut keep = vec![false; words];
    let mut writer = vec![NONE; words];
    for u in 0..n as u32 {
        if member[u as usize] {
            continue;
        }
        let ins = &p.ops[u as usize];
        for w in unit_writes(p, u) {
            writer[w as usize] = u;
            if !simple(ins.k) || p.out[u as usize].n != 1 {
                keep[w as usize] = true;
            }
        }
    }
    let mut seq = Vec::new();
    for d in &p.domains {
        for ins in d.sample.iter().chain(&d.write).chain(&d.commit).chain(&d.gated) {
            seq_reads(p, ins, &mut seq);
        }
    }
    for s in seq {
        keep[p.slot_of[s as usize] as usize] = true;
    }
    for c in &p.clocks {
        keep[p.slot_of[c.word as usize] as usize] = true;
    }
    for &(si, _, _) in &p.ports {
        for l in &p.signals[si].1 {
            if let Loc::Bit { word, .. } = l {
                keep[p.slot_of[*word as usize] as usize] = true;
            }
        }
    }
    let mut local = vec![false; words];
    let mut reader = vec![NONE; words];
    for s in 0..words {
        if p.slot_of[s] as usize != s || keep[s] || writer[s] == NONE {
            continue;
        }
        if p.word_kind[s] != SlotKind::Comb {
            continue;
        }
        let rs = &p.rd[p.rd_start[s] as usize..p.rd_start[s + 1] as usize];
        if rs.len() != 1 || rs[0] as usize >= n {
            continue;
        }
        let r = rs[0];
        if !simple(p.ops[r as usize].k) || p.ops[r as usize].k == K::Shiftx {
            continue;
        }
        local[s] = true;
        reader[s] = r;
    }
    // Partitions: a unit joins its reader's when its result is local.
    let mut part_of = vec![NONE; n];
    let mut roots = Vec::new();
    for u in (0..n as u32).rev() {
        if member[u as usize] {
            continue;
        }
        let ins = &p.ops[u as usize];
        let joins = ins.k != K::Loop && ins.k != K::Decode && local[ins.d as usize];
        if joins {
            part_of[u as usize] = part_of[reader[ins.d as usize] as usize];
        } else {
            part_of[u as usize] = roots.len() as u32;
            roots.push(u);
        }
    }
    // Number partitions in evaluation order (by root).
    let np = roots.len() as u32;
    let mut parts = vec![Vec::new(); np as usize];
    for u in 0..n as u32 {
        if part_of[u as usize] != NONE {
            part_of[u as usize] = np - 1 - part_of[u as usize];
            parts[part_of[u as usize] as usize].push(u);
        }
    }
    // Reorder them so that partitions marked by the same source sit
    // together (dense groups of dirty bits), keeping a topological order:
    // each partition's sources are the partitions and state words it reads.
    {
        let np = parts.len();
        let mut srcs: Vec<Vec<u64>> = vec![Vec::new(); np];
        let mut succ: Vec<Vec<u32>> = vec![Vec::new(); np];
        let mut preds = vec![0u32; np];
        for q in 0..np {
            for o in unit_writes(p, *parts[q].last().unwrap()) {
                for &r in readers(p, o) {
                    if (r as usize) < n
                        && part_of[r as usize] != NONE
                        && part_of[r as usize] != q as u32
                    {
                        succ[q].push(part_of[r as usize]);
                    }
                }
            }
            succ[q].sort_unstable();
            succ[q].dedup();
            for &t in &succ[q] {
                preds[t as usize] += 1;
                srcs[t as usize].push(q as u64);
            }
        }
        // External sources: state and input words read.
        for w in 0..words {
            if p.slot_of[w] as usize != w
                || matches!(p.word_kind[w], SlotKind::Comb | SlotKind::Const)
            {
                continue;
            }
            for &r in readers(p, w as u32) {
                if (r as usize) < n && part_of[r as usize] != NONE {
                    srcs[part_of[r as usize] as usize].push(1 << 40 | w as u64);
                }
            }
        }
        for s in &mut srcs {
            s.sort_unstable();
            s.dedup();
        }
        let mut ready: Vec<u32> = (0..np as u32).filter(|&q| preds[q as usize] == 0).collect();
        let mut order: Vec<u32> = Vec::with_capacity(np);
        let mut last: Option<u32> = None;
        while !ready.is_empty() {
            // The ready partition sharing most sources with the last one,
            // then the first.
            let pick = match last {
                Some(l) => {
                    let ls = &srcs[l as usize];
                    let shared = |q: u32| {
                        srcs[q as usize].iter().filter(|x| ls.binary_search(x).is_ok()).count()
                    };
                    ready
                        .iter()
                        .copied()
                        .max_by_key(|&q| (shared(q), std::cmp::Reverse(q)))
                        .unwrap()
                }
                None => *ready.iter().min().unwrap(),
            };
            ready.retain(|&q| q != pick);
            order.push(pick);
            last = Some(pick);
            for &t in &succ[pick as usize] {
                preds[t as usize] -= 1;
                if preds[t as usize] == 0 {
                    ready.push(t);
                }
            }
        }
        assert_eq!(order.len(), np);
        let mut new_index = vec![0u32; np];
        for (i, &q) in order.iter().enumerate() {
            new_index[q as usize] = i as u32;
        }
        parts = order.iter().map(|&q| std::mem::take(&mut parts[q as usize])).collect();
        for q in &mut part_of {
            if *q != NONE {
                *q = new_index[*q as usize];
            }
        }
    }
    let mut children = vec![Vec::new(); n];
    for (s, &r) in reader.iter().enumerate() {
        if local[s] {
            children[r as usize].push(writer[s]);
        }
    }
    for c in &mut children {
        c.sort_unstable();
        c.dedup();
    }
    Plan { local, parts, part_of, children }
}

/// Partitions of at most this many instructions run whenever their group
/// of dirty bits is dirty, without a test of their own: a mispredicted
/// branch costs more than running them.
const SMALL: usize = 8;

/// Sizes of the nested groups of dirty bits `settle` tests, outermost
/// first (a word, then within it). Measured best on the NES and the
/// PicoRV32 SoC; an event loop jumping to each dirty partition was slower.
const LEVELS: [u32; 3] = [64, 16, 4];

/// Statements setting the dirty bits of `parts`.
fn marks(parts: impl IntoIterator<Item = u32>, dirty: &str) -> Vec<String> {
    let mut words: std::collections::BTreeMap<u32, u64> = Default::default();
    for q in parts {
        *words.entry(q / 64).or_default() |= 1 << (q % 64);
    }
    words
        .into_iter()
        .map(|(w, m)| match dirty {
            // The locals `settle` keeps the dirty bits in.
            "d" => format!("{dirty}{w} |= {};", hex(m)),
            _ => format!("{dirty}[{w}] |= {};", hex(m)),
        })
        .collect()
}

struct Emitter<'p> {
    p: &'p Program,
    plan: &'p Plan,
    prefix: String,
    out: String,
    indent: usize,
    /// Temporaries for loops.
    tmp: usize,
    /// Set by a memory write when it writes.
    on_write: Vec<String>,
}

impl Emitter<'_> {
    fn line(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    /// A word of the state, or the local holding it. (Fields of some
    /// instructions that are not words are named too, and never used.)
    fn w(&self, i: u32) -> String {
        if self.plan.local.get(i as usize).copied().unwrap_or(false) {
            format!("t{i}")
        } else {
            format!("v[{i}]")
        }
    }

    /// Partitions reading the slot holding `word`.
    fn reader_parts(&self, word: u32) -> Vec<u32> {
        let n = self.p.ops.len();
        readers(self.p, word)
            .iter()
            .filter(|&&r| (r as usize) < n)
            .map(|&r| self.plan.part_of[r as usize])
            .filter(|&q| q != NONE)
            .collect()
    }

    /// Statements marking the partitions reading the slot holding `word`
    /// (except `skip`) in dirty words `dirty`.
    fn mark_readers(&self, word: u32, dirty: &str, skip: u32) -> Vec<String> {
        marks(self.reader_parts(word).into_iter().filter(|&q| q != skip), dirty)
    }

    fn copy(&mut self, d: u32, a: u32, w: u32) {
        if d == a {
            return;
        }
        for j in 0..nwords(w) {
            let line = format!("{} = {};", self.w(d + j), self.w(a + j));
            self.line(&line);
        }
    }

    fn ctl(&self, c_word: u32, x: u64) -> String {
        let bit = x & 63;
        if x >> 8 & 1 == 1 {
            format!("({} >> {bit} & 1)", self.w(c_word))
        } else {
            format!("!({} >> {bit} & 1)", self.w(c_word))
        }
    }

    fn ctl_of(&self, c: Ctl) -> String {
        self.ctl(c.word, c.bit as u64 | (c.pol as u64) << 8)
    }

    /// Emits one instruction (not a loop).
    fn ins(&mut self, ins: &Ins) -> Result<(), String> {
        let p = self.p;
        let (d, a, b, w) = (ins.d, ins.a, ins.b, ins.w);
        let m = hex(mask(w));
        let (vd, va, vb) = (self.w(d), self.w(a), self.w(b));
        let bin = |e: &str| format!("{vd} = {e};");
        match ins.k {
            K::Copy => self.copy(d, a, w),
            K::Gather => {
                let n = nwords(w);
                let mut words: Vec<Vec<String>> = vec![Vec::new(); n as usize];
                if ins.c != NONE {
                    for j in 0..n {
                        words[j as usize].push(self.w(ins.c + j));
                    }
                }
                let parts = &p.aux.parts[a as usize..(a + b) as usize];
                let mut i = 0;
                while i < parts.len() {
                    let part = &parts[i];
                    // A bit repeated (a sign or mask extension): one negation.
                    let mut n = 1;
                    while part.len == 1
                        && parts.get(i + n).is_some_and(|q| {
                            (q.src, q.dst, q.lo, q.len) == (part.src, part.dst, part.lo, 1)
                                && q.pos as usize == part.pos as usize + n
                        })
                    {
                        n += 1;
                    }
                    i += n;
                    if n > 1 {
                        let mut e = format!("(0 - ({} >> {} & 1))", self.w(part.src), part.lo);
                        if part.pos as usize + n != 64 {
                            e = format!("({e} & {})", hex(mask(n as u32)));
                        }
                        if part.pos != 0 {
                            e = format!("({e} << {})", part.pos);
                        }
                        words[part.dst as usize].push(e);
                        continue;
                    }
                    let mut e = self.w(part.src);
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
                    let line = format!("{} = {e};", self.w(d + j as u32));
                    self.line(&line);
                }
            }
            K::Extract => {
                let e = if ins.x == 0 {
                    format!("{va} & {m}")
                } else {
                    format!("{va} >> {} & {m}", ins.x)
                };
                self.line(&bin(&e));
            }
            K::Not => self.line(&bin(&format!("~{va} & {m}"))),
            K::Neg => self.line(&bin(&format!("(0 - {va}) & {m}"))),
            K::RedAnd => self.line(&bin(&format!("{va} == {m}"))),
            K::RedOr => self.line(&bin(&format!("{va} != 0"))),
            K::RedXor => self.line(&bin(&format!("(uint64_t)__builtin_parityll({va})"))),
            K::RedXnor => self.line(&bin(&format!("(uint64_t)!__builtin_parityll({va})"))),
            K::LogicNot => self.line(&bin(&format!("{va} == 0"))),
            K::And => self.line(&bin(&format!("{va} & {vb}"))),
            K::Or => self.line(&bin(&format!("{va} | {vb}"))),
            K::Xor => self.line(&bin(&format!("{va} ^ {vb}"))),
            K::Xnor => self.line(&bin(&format!("~({va} ^ {vb}) & {m}"))),
            K::Add => self.line(&bin(&format!("({va} + {vb}) & {m}"))),
            K::Sub => self.line(&bin(&format!("({va} - {vb}) & {m}"))),
            K::Mul => self.line(&bin(&format!("({va} * {vb}) & {m}"))),
            K::DivU => self.line(&bin(&format!("{vb} ? {va} / {vb} : 0"))),
            K::ModU => self.line(&bin(&format!("{vb} ? {va} % {vb} : 0"))),
            K::DivS => self.line(&bin(&format!("sim_divs({va}, {vb}, {w})"))),
            K::ModS => self.line(&bin(&format!("sim_mods({va}, {vb}, {w})"))),
            K::Shl => self.line(&bin(&format!("{vb} >= {w} ? 0 : {va} << {vb} & {m}"))),
            K::Shr => self.line(&bin(&format!("{vb} >= {w} ? 0 : {va} >> {vb}"))),
            K::Sshr => self.line(&bin(&format!(
                "(uint64_t)(sim_sx({va}, {w}) >> ({vb} < 63 ? {vb} : 63)) & {m}"
            ))),
            K::Eq | K::Eqx => self.line(&bin(&format!("{va} == {vb}"))),
            K::Ne | K::Nex => self.line(&bin(&format!("{va} != {vb}"))),
            K::LtU => self.line(&bin(&format!("{va} < {vb}"))),
            K::LeU => self.line(&bin(&format!("{va} <= {vb}"))),
            K::GtU => self.line(&bin(&format!("{va} > {vb}"))),
            K::GeU => self.line(&bin(&format!("{va} >= {vb}"))),
            K::LtS | K::LeS | K::GtS | K::GeS => {
                let op = match ins.k {
                    K::LtS => "<",
                    K::LeS => "<=",
                    K::GtS => ">",
                    _ => ">=",
                };
                self.line(&bin(&format!("sim_sx({va}, {w}) {op} sim_sx({vb}, {w})")));
            }
            K::LAnd => self.line(&bin(&format!("{va} != 0 && {vb} != 0"))),
            K::LOr => self.line(&bin(&format!("{va} != 0 || {vb} != 0"))),
            K::Mux => {
                let s = format!("({} >> {} & 1)", self.w(ins.c), ins.x & 63);
                if nwords(w) == 1 {
                    self.line(&bin(&format!("{s} ? {vb} : {va}")));
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
                if sw > 64 {
                    return Err("one-hot multiplexers with more than 64 cases".into());
                }
                let cases: Vec<u32> =
                    p.aux.lists[ins.x as usize..ins.x as usize + sw as usize].to_vec();
                let n = nwords(w);
                self.line(&format!("{{ uint64_t s = {vb} & {};", hex(mask(sw))));
                self.line("if (s == 0) {");
                self.copy(d, a, w);
                self.line("} else if (s & (s - 1)) {");
                for j in 0..n {
                    let line = format!("{} = 0;", self.w(d + j));
                    self.line(&line);
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
                    format!("(int64_t)sim_sx({}, {})", self.w(info.b.off), info.b.w)
                } else {
                    format!("(int64_t){}", self.w(info.b.off))
                };
                let e = format!(
                    "sim_extract(&v[{}], {}, {}, {idx} + ({}), {w})",
                    info.a.off,
                    info.a.words(),
                    info.a.w,
                    info.offset
                );
                self.line(&bin(&e));
            }
            K::MemRd => self.mem_read(ins.c, a, d, w),
            K::Latch | K::Arst => {
                let c = self.ctl(ins.c, ins.x);
                self.line(&format!("if ({c}) {{"));
                self.copy(d, a, w);
                self.line("}");
            }
            K::FfEn => {
                let c = self.ctl(ins.c, ins.x);
                self.line(&format!("if ({c}) {{"));
                self.copy(d, a, w);
                if d != b {
                    self.line("} else {");
                    self.copy(d, b, w);
                }
                self.line("}");
            }
            K::Ff => {
                let f = p.aux.flops[ins.x as usize].clone();
                let en = f.en.map_or("1".to_string(), |c| self.ctl_of(c));
                let mut first = true;
                let mut branch = |e: &mut Self, cond: String, src: u32| {
                    let kw = if first { "if" } else { "} else if" };
                    first = false;
                    e.line(&format!("{kw} ({cond}) {{"));
                    e.copy(f.dst, src, f.w);
                };
                if let Some((c, val)) = f.arst {
                    let cond = self.ctl_of(c);
                    branch(self, cond, val);
                }
                if let Some((c, val)) = f.srst {
                    let cond = if f.en_over_srst {
                        format!("{} && {en}", self.ctl_of(c))
                    } else {
                        self.ctl_of(c)
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
                    self.w(wr.addr.off),
                    mem.offset,
                    mem.depth
                ));
                for j in 0..nw {
                    let e = self.w(wr.en.off + j);
                    let dv = self.w(wr.data.off + j);
                    let el = if nw == 1 {
                        format!("m->mem{mi}[i]")
                    } else {
                        format!("m->mem{mi}[i * {nw} + {j}]")
                    };
                    self.line(&format!("{el} = ({ty})(({el} & ~{e}) | ({dv} & {e}));"));
                }
                for l in self.on_write.clone() {
                    self.line(&l);
                }
                self.line("} }");
            }
            K::MemRdSync => {
                let r = p.aux.sync_reads[ins.x as usize].clone();
                let mem_w = p.aux.mems[r.mem as usize].width;
                match r.en {
                    Some(en) => {
                        let c = self.ctl_of(en);
                        self.line(&format!("if ({c}) {{"));
                    }
                    None => self.line("{"),
                }
                self.mem_read(r.mem, r.addr.off, r.dst, mem_w);
                for &wi in &r.transparent {
                    let wr = p.aux.writes[wi as usize].clone();
                    self.line(&format!(
                        "if ({} == {}) {{",
                        self.w(wr.addr.off),
                        self.w(r.addr.off)
                    ));
                    for j in 0..nwords(mem_w) {
                        let e = self.w(wr.en.off + j);
                        let line = format!(
                            "{0} = ({0} & ~{e}) | ({1} & {e});",
                            self.w(r.dst + j),
                            self.w(wr.data.off + j)
                        );
                        self.line(&line);
                    }
                    self.line("}");
                }
                if r.en.is_some() {
                    self.line("} else {");
                    self.copy(r.dst, r.q, mem_w);
                }
                self.line("}");
            }
            K::Decode => {
                let dec = &p.aux.decoders[ins.x as usize];
                for &(key, dd, ne) in &dec.entries {
                    let op = if ne { "!=" } else { "==" };
                    let line = format!("{} = {va} {op} {};", self.w(dd), hex(key));
                    self.line(&line);
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
            self.w(addr),
            mem.offset,
            mem.depth
        ));
        if nw == 1 && mem.width <= 64 {
            let line = format!("{} = m->mem{mi}[i];", self.w(d));
            self.line(&line);
        } else {
            for j in 0..nw {
                let line = format!("{} = m->mem{mi}[i * {nw} + {j}];", self.w(d + j));
                self.line(&line);
            }
        }
        self.line("} else {");
        for j in 0..nwords(w) {
            let line = format!("{} = 0;", self.w(d + j));
            self.line(&line);
        }
        self.line("} }");
    }

    /// Operations on wide values, through the helpers of the prelude.
    fn wide(&mut self, ins: &Ins) -> Result<(), String> {
        let wi = self.p.aux.wide[ins.x as usize].clone();
        let (d, a, b) = (wi.d, wi.a, wi.b);
        let args = |f: &str| {
            format!("{f}(&v[{}], {}, &v[{}], {}, &v[{}], {});", d.off, d.w, a.off, a.w, b.off, b.w)
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
                "sim_wshiftx(&v[{}], {}, &v[{}], {}, &v[{}], {}, {}, {offset});",
                d.off, d.w, a.off, a.w, b.off, b.w, *b_signed as u32
            ),
        };
        self.line(&call);
        Ok(())
    }

    /// Emits unit `u` of a partition after the units computing its local
    /// operands; a multiplexer computes only the operand it selects.
    fn tree(&mut self, u: u32) -> Result<(), String> {
        let p = self.p;
        let ins = p.ops[u as usize];
        let children = self.plan.children[u as usize].clone();
        if ins.k == K::Mux && nwords(ins.w) == 1 && ins.a != ins.b {
            let (sel, a, b) = (p.slot_of[ins.c as usize], ins.a, ins.b);
            let of = |slot: u32| {
                children
                    .iter()
                    .copied()
                    .filter(|&c| p.ops[c as usize].d == slot)
                    .collect::<Vec<_>>()
            };
            let (cs, ca, cb) = (of(sel), of(a), of(b));
            if !ca.is_empty() || !cb.is_empty() {
                for c in cs {
                    self.tree(c)?;
                }
                let s = format!("({} >> {} & 1)", self.w(ins.c), ins.x & 63);
                let (vd, va, vb) = (self.w(ins.d), self.w(a), self.w(b));
                self.line(&format!("if ({s}) {{"));
                self.indent += 1;
                for c in cb {
                    self.tree(c)?;
                }
                self.line(&format!("{vd} = {vb};"));
                self.indent -= 1;
                self.line("} else {");
                self.indent += 1;
                for c in ca {
                    self.tree(c)?;
                }
                self.line(&format!("{vd} = {va};"));
                self.indent -= 1;
                self.line("}");
                return Ok(());
            }
        }
        if ins.k == K::Pmux && nwords(ins.w) == 1 && ins.c <= 64 {
            // A one-hot multiplexer: each input's cone only in its case.
            let cases: Vec<u32> =
                p.aux.lists[ins.x as usize..ins.x as usize + ins.c as usize].to_vec();
            let of = |slot: u32| {
                children
                    .iter()
                    .copied()
                    .filter(|&c| p.ops[c as usize].d == slot)
                    .collect::<Vec<_>>()
            };
            if cases.iter().chain([&ins.a]).any(|&c| !of(c).is_empty()) {
                for c in of(p.slot_of[ins.b as usize]) {
                    self.tree(c)?;
                }
                let vd = self.w(ins.d);
                let arm = |e: &mut Self, slot: u32| -> Result<(), String> {
                    e.indent += 1;
                    for c in of(slot) {
                        e.tree(c)?;
                    }
                    let line = format!("{vd} = {};", e.w(slot));
                    e.line(&line);
                    e.indent -= 1;
                    Ok(())
                };
                self.line(&format!("{{ uint64_t s = {} & {};", self.w(ins.b), hex(mask(ins.c))));
                self.line("if (s == 0) {");
                arm(self, ins.a)?;
                self.line("} else if (s & (s - 1)) {");
                self.line(&format!("    {vd} = 0;"));
                self.line("} else switch (__builtin_ctzll(s)) {");
                for (i, &c) in cases.iter().enumerate() {
                    self.line(&format!("case {i}: {{"));
                    arm(self, c)?;
                    self.line("    break; }");
                }
                self.line("} }");
                return Ok(());
            }
        }
        for c in children {
            self.tree(c)?;
        }
        self.ins(&ins)
    }

    /// Emits the decoder of partition `k` (rooted at `u`): when its input
    /// changes, only the comparisons with the old and the new value change
    /// (all of them the first time), and only those mark their readers.
    fn decode(&mut self, u: u32, k: u32) -> Result<(), String> {
        let p = self.p;
        let ins = p.ops[u as usize];
        for c in self.plan.children[u as usize].clone() {
            self.tree(c)?;
        }
        let dec = &p.aux.decoders[ins.x as usize];
        let (last, valid) = (ins.d, ins.d + 1);
        self.line(&format!("{{ uint64_t nv = {}, ov = v[{last}];", self.w(ins.a)));
        self.line(&format!("if (!v[{valid}]) {{"));
        self.indent += 1;
        self.line(&format!("v[{valid}] = 1; v[{last}] = nv;"));
        for &(key, dd, ne) in &dec.entries {
            let op = if ne { "!=" } else { "==" };
            let ms = self.mark_readers(dd, "d", k);
            self.line(&format!("v[{dd}] = nv {op} {}; {}", hex(key), ms.join(" ")));
        }
        self.indent -= 1;
        self.line("} else if (nv != ov) {");
        self.indent += 1;
        self.line(&format!("v[{last}] = nv;"));
        // Entries by key: those of the old value now differ, those of the
        // new one match.
        let mut keys: Vec<u64> = dec.entries.iter().map(|e| e.0).collect();
        keys.dedup();
        for (value, matches) in [("ov", false), ("nv", true)] {
            self.line(&format!("switch ({value}) {{"));
            for &key in &keys {
                self.line(&format!("case {}: {{", hex(key)));
                for &(_, dd, ne) in dec.entries.iter().filter(|e| e.0 == key) {
                    let ms = self.mark_readers(dd, "d", k);
                    self.line(&format!("    v[{dd}] = {}; {}", (matches ^ ne) as u8, ms.join(" ")));
                }
                self.line("    break; }");
            }
            self.line("}");
        }
        self.indent -= 1;
        self.line("} }");
        Ok(())
    }

    /// Emits a loop unit: its instructions repeated until they settle.
    fn looped(&mut self, u: u32) -> Result<(), String> {
        let p = self.p;
        let ins = p.ops[u as usize];
        let outs: Vec<u32> = p.aux.lists[ins.b as usize..(ins.b + ins.c) as usize].to_vec();
        let t = self.tmp;
        self.tmp += 1;
        self.line(&format!("{{ uint64_t l{t}[{}]; int it{t};", outs.len()));
        self.line(&format!("for (it{t} = 0; it{t} < 256; it{t}++) {{"));
        for (k, &o) in outs.iter().enumerate() {
            let line = format!("l{t}[{k}] = {};", self.w(o));
            self.line(&line);
        }
        for k in 0..ins.a {
            let body = p.ops[(u + 1 + k) as usize];
            self.ins(&body)?;
        }
        let same: Vec<String> =
            outs.iter().enumerate().map(|(k, &o)| format!("l{t}[{k}] == {}", self.w(o))).collect();
        self.line(&format!("if ({}) break;", same.join(" && ")));
        self.line("} }");
        Ok(())
    }

    /// Emits `body(i)` for each `i` in `0..n` whose dirty bit (bit `i % 64`
    /// of local `{var}{i / 64}`) is set, under nested tests that skip runs of
    /// clean ones: a word, then groups within it (see [`LEVELS`]).
    fn under_dirty(
        &mut self,
        var: &str,
        n: u32,
        own_test: impl Fn(u32) -> bool,
        mut body: impl FnMut(&mut Self, u32) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut open = 0;
        for k in 0..n {
            for (l, &size) in LEVELS.iter().enumerate() {
                if !k.is_multiple_of(size) {
                    continue;
                }
                // Close this level and those inside it.
                while open > l {
                    self.indent -= 1;
                    self.line("}");
                    open -= 1;
                }
                let c = size.min(n - k);
                let bits = if c == 64 { u64::MAX } else { ((1u64 << c) - 1) << (k % 64) };
                if size == 64 {
                    self.line(&format!("if ({var}{}) {{", k / 64));
                } else if c > 1 {
                    self.line(&format!("if ({var}{} & {}) {{", k / 64, hex(bits)));
                } else {
                    // A single one: its own test suffices.
                    self.line("{");
                }
                self.indent += 1;
                open += 1;
            }
            if own_test(k) {
                self.line(&format!("if ({var}{} & {}) {{", k / 64, hex(1u64 << (k % 64))));
            } else {
                // Cheaper to run than to test (a mispredicted branch costs
                // more): run whenever its group is dirty.
                self.line("{");
            }
            self.indent += 1;
            body(self, k)?;
            self.indent -= 1;
            self.line("}");
        }
        for _ in 0..open {
            self.indent -= 1;
            self.line("}");
        }
        Ok(())
    }

    /// Emits the partitions: each runs when its bit is set and marks the
    /// partitions reading what it changed.
    fn settle(&mut self) -> Result<(), String> {
        let p = self.p;
        let plan = self.plan;
        let pre = self.prefix.clone();
        writeln!(self.out, "static void settle({pre}_t *m) {{").unwrap();
        self.indent = 1;
        self.line("uint64_t *v = m->v, *D = m->dirty; (void)v;");
        // The dirty bits live in locals (registers) while partitions run in
        // order: a partition only marks later ones, so none needs clearing
        // once run, and all are clear at the end.
        let np = plan.parts.len() as u32;
        for w in 0..np.div_ceil(64) {
            self.line(&format!("uint64_t d{w} = D[{w}]; D[{w}] = 0;"));
        }
        let own_test = |k: u32| {
            let units = &plan.parts[k as usize];
            units.len() > SMALL || p.ops[*units.last().unwrap() as usize].k == K::Loop
        };
        self.under_dirty("d", np, own_test, |e, k| {
            let units = &plan.parts[k as usize];
            let root = *units.last().unwrap();
            let ins = p.ops[root as usize];
            // Locals of the partition.
            let locals: Vec<String> = units
                .iter()
                .filter(|&&u| {
                    p.ops[u as usize].k != K::Loop && plan.local[p.ops[u as usize].d as usize]
                })
                .map(|&u| format!("t{}", p.ops[u as usize].d))
                .collect();
            if !locals.is_empty() {
                e.line(&format!("uint64_t {};", locals.join(", ")));
            }
            if ins.k == K::Decode {
                return e.decode(root, k);
            }
            // What it writes to the state and is read, before.
            let outs: Vec<(u32, Vec<String>)> = match ins.k {
                K::Loop => p.aux.lists[ins.b as usize..(ins.b + ins.c) as usize].to_vec(),
                K::Decode => p.aux.decoders[ins.x as usize].entries.iter().map(|e| e.1).collect(),
                _ => (0..p.out[root as usize].n).map(|j| ins.d + j).collect(),
            }
            .into_iter()
            .map(|o| (o, e.mark_readers(o, "d", k)))
            .filter(|(_, ms)| !ms.is_empty())
            .collect();
            for (j, (o, _)) in outs.iter().enumerate() {
                e.line(&format!("uint64_t o{j} = v[{o}];"));
            }
            if ins.k == K::Loop {
                e.looped(root)?;
            } else {
                e.tree(root)?;
            }
            for (j, (o, ms)) in outs.iter().enumerate() {
                e.line(&format!("if (o{j} != v[{o}]) {{ {} }}", ms.join(" ")));
            }
            Ok(())
        })?;
        self.indent = 0;
        self.out.push_str("}\n\n");
        Ok(())
    }

    fn function(
        &mut self,
        name: &str,
        body: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<(), String> {
        let pre = self.prefix.clone();
        writeln!(self.out, "static void {name}({pre}_t *m) {{").unwrap();
        self.indent = 1;
        self.line("uint64_t *v = m->v, *D = m->dirty; (void)v; (void)D;");
        body(self)?;
        self.indent = 0;
        self.out.push_str("}\n\n");
        Ok(())
    }
}

/// The helpers every model uses.
const PRELUDE: &str = include_str!("prelude.c");

fn table(out: &mut String, name: &str, ty: &str, values: &[String]) {
    writeln!(out, "static const {ty} {name}[{}] = {{", values.len().max(1)).unwrap();
    for c in values.chunks(12) {
        writeln!(out, "    {},", c.join(", ")).unwrap();
    }
    if values.is_empty() {
        out.push_str("    0\n");
    }
    out.push_str("};\n");
}

/// Emits a two-state program as a C model named `prefix`.
pub fn emit(p: &Program, prefix: &str) -> Result<CModel, String> {
    if p.four_state {
        return Err("the C backend simulates two-state programs only".into());
    }
    let prefix = ident(prefix);
    let pre = &prefix;
    let plan = plan(p);
    let np = plan.parts.len();
    let dirty_words = np.div_ceil(64).max(1);
    let e = Emitter {
        p,
        plan: &plan,
        prefix: prefix.clone(),
        out: String::new(),
        indent: 0,
        tmp: 0,
        on_write: Vec::new(),
    };

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
    writeln!(h, "    /* Partitions of the logic to run. */\n    uint64_t dirty[{dirty_words}];")
        .unwrap();
    writeln!(h, "}} {pre}_t;\n").unwrap();
    writeln!(h, "/* Sets the initial state and settles it. */\nvoid {pre}_init({pre}_t *m);")
        .unwrap();
    writeln!(
        h,
        "/* Settles after inputs changed and runs the clock edges that result. */\nvoid {pre}_eval({pre}_t *m);"
    )
    .unwrap();
    let host = marks(
        p.mem_readers.iter().flatten().map(|&u| plan.part_of[u as usize]).filter(|&q| q != NONE),
        "m->dirty",
    );
    writeln!(
        h,
        "/* Call after writing memories directly (m->memN), before eval. */\nstatic inline void {pre}_memories_written({pre}_t *m) {{ {} }}\n",
        host.join(" ")
    )
    .unwrap();
    for (i, mem) in p.aux.mems.iter().enumerate() {
        writeln!(h, "#define {pre}_MEM_{} mem{i}", ident(&mem.name).to_uppercase()).unwrap();
    }
    let mut ports = p.ports.clone();
    ports.sort_by_key(|x| x.2);
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
            && p.slot_of[*word as usize] == *word
        {
            let ms = e.mark_readers(*word, "m->dirty", NONE);
            writeln!(
                h,
                "static inline void {pre}_set_{id}({pre}_t *m, uint64_t x) {{ x &= {}; if (m->v[{word}] != x) {{ m->v[{word}] = x; {} }} }}",
                hex(mask(w as u32)),
                ms.join(" ")
            )
            .unwrap();
        }
    }
    writeln!(h, "\n#endif").unwrap();

    // Implementation.
    let mut e = e;
    writeln!(e.out, "/* Generated by synth (synth-sim C backend). */").unwrap();
    writeln!(e.out, "#include \"{pre}.h\"\n#include <string.h>\n").unwrap();
    e.out.push_str(PRELUDE);
    e.out.push('\n');
    // Initial values.
    let nz: Vec<(usize, u64)> =
        p.init_val.iter().enumerate().filter(|(_, x)| **x != 0).map(|(i, x)| (i, *x)).collect();
    table(
        &mut e.out,
        "init_at",
        "uint32_t",
        &nz.iter().map(|(i, _)| i.to_string()).collect::<Vec<_>>(),
    );
    table(&mut e.out, "init_val", "uint64_t", &nz.iter().map(|(_, x)| hex(*x)).collect::<Vec<_>>());
    for (i, mem) in p.aux.mems.iter().enumerate() {
        let words =
            &p.mem_init_val[mem.base..mem.base + mem.depth as usize * nwords(mem.width) as usize];
        let nz: Vec<(usize, u64)> =
            words.iter().enumerate().filter(|(_, x)| **x != 0).map(|(i, x)| (i, *x)).collect();
        if nz.is_empty() {
            continue;
        }
        table(
            &mut e.out,
            &format!("mem{i}_at"),
            "uint32_t",
            &nz.iter().map(|(i, _)| i.to_string()).collect::<Vec<_>>(),
        );
        table(
            &mut e.out,
            &format!("mem{i}_val"),
            "uint64_t",
            &nz.iter().map(|(_, x)| hex(*x)).collect::<Vec<_>>(),
        );
    }
    e.out.push('\n');

    // Logic.
    e.settle()?;
    for (k, d) in p.domains.iter().enumerate() {
        let d = d.clone();
        e.function(&format!("sample{k}"), |e| d.sample.iter().try_for_each(|i| e.ins(i)))?;
        e.function(&format!("write{k}"), |e| {
            for ins in &d.write {
                let mem = p.aux.writes[ins.x as usize].mem as usize;
                e.on_write = marks(
                    p.mem_readers[mem]
                        .iter()
                        .map(|&u| plan.part_of[u as usize])
                        .filter(|&q| q != NONE),
                    "D",
                );
                e.ins(ins)?;
            }
            e.on_write.clear();
            Ok(())
        })?;
        e.function(&format!("commit{k}"), |e| {
            // Updates `ins`, marking the readers of its result if it changed.
            let update = |e: &mut Emitter, ins: &Ins| -> Result<(), String> {
                let n = nwords(ins.w);
                let ms = e.mark_readers(ins.d, "D", NONE);
                if ms.is_empty() {
                    return e.ins(ins);
                }
                e.line("{");
                for j in 0..n {
                    e.line(&format!("uint64_t o{j} = v[{}];", ins.d + j));
                }
                e.ins(ins)?;
                let diff: Vec<String> =
                    (0..n).map(|j| format!("o{j} != v[{}]", ins.d + j)).collect();
                e.line(&format!("if ({}) {{ {} }}", diff.join(" || "), ms.join(" ")));
                e.line("}");
                Ok(())
            };
            // (Gated flip-flops too: most have a changed input at every
            // edge, and their dirty bits would cost every value they read a
            // comparison.)
            d.commit.iter().chain(&d.gated).try_for_each(|ins| update(e, ins))
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
    writeln!(e.out, "    memset(m->dirty, 0xff, sizeof m->dirty);\n    settle(m);").unwrap();
    for (k, c) in p.clocks.iter().enumerate() {
        writeln!(e.out, "    m->clk[{k}] = m->v[{}] >> {} & 1;", c.word, c.bit).unwrap();
    }
    writeln!(e.out, "}}\n").unwrap();

    let dw = p.domains.len().div_ceil(64).max(1);
    writeln!(e.out, "void {pre}_eval({pre}_t *m) {{").unwrap();
    writeln!(e.out, "    int round;").unwrap();
    writeln!(e.out, "    for (round = 0; round < 10000; round++) {{").unwrap();
    writeln!(e.out, "        uint64_t fired[{dw}] = {{0}}, any = 0;").unwrap();
    writeln!(e.out, "        settle(m);").unwrap();
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
    for phase in ["sample", "write", "commit"] {
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
