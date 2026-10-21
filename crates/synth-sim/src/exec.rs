//! The interpreter: runs instructions on the state.
//!
//! One function serves both modes: `F` (four-state) is a constant, so the
//! two-state version carries no unknown-plane code. Four-state operations
//! take a fast path when their operands are known; otherwise they evaluate
//! through [`synth_rtl::eval`], which defines the semantics.

use synth_core::LogicVec;
use synth_rtl::{BinaryOp, UnaryOp, eval};

use crate::program::*;

/// The values of a running program.
#[derive(Clone, Debug)]
pub struct State {
    pub val: Vec<u64>,
    pub unk: Vec<u64>,
    pub mval: Vec<u64>,
    pub munk: Vec<u64>,
    /// Scratch space for loop fixpoints and change detection.
    scratch: Vec<u64>,
    snap: Vec<u64>,
    /// Loops that did not settle.
    pub unstable: u64,
    /// Units run by settles.
    pub units_run: u64,
}

impl State {
    pub fn new(p: &Program) -> State {
        State {
            val: p.init_val.clone(),
            unk: p.init_unk.clone(),
            mval: p.mem_init_val.clone(),
            munk: p.mem_init_unk.clone(),
            scratch: Vec::new(),
            snap: Vec::new(),
            unstable: 0,
            units_run: 0,
        }
    }
}

/// Marks the units reading slot `slot` (except `except`).
#[inline(always)]
pub fn mark(p: &Program, dirty: &mut [u64], slot: u32, except: u32) {
    let (a, b) = (p.rd_start[slot as usize] as usize, p.rd_start[slot as usize + 1] as usize);
    for &r in &p.rd[a..b] {
        if r != except {
            dirty[(r >> 6) as usize] |= 1 << (r & 63);
        }
    }
}

#[inline(always)]
fn mark_range(p: &Program, dirty: &mut [u64], lo: u32, hi: u32) {
    for &r in &p.rd[lo as usize..hi as usize] {
        dirty[(r >> 6) as usize] |= 1 << (r & 63);
    }
}

/// Runs the instruction at `i` and marks the readers of its output if it
/// changed.
#[inline(always)]
fn step_marked<const F: bool>(p: &Program, i: usize, ins: &Ins, st: &mut State, dirty: &mut [u64]) {
    let d = ins.d as usize;
    let o = p.out[i];
    if o.n == 1 {
        let ov = st.val[d];
        let ou = if F { st.unk[d] } else { 0 };
        step::<F>(&p.aux, ins, st);
        if st.val[d] != ov || F && st.unk[d] != ou {
            mark_range(p, dirty, o.lo, o.hi);
        }
    } else {
        changed_words::<F>(p, d, o.n as usize, st, dirty, |st| step::<F>(&p.aux, ins, st));
    }
}

/// Runs a decoder: updates the comparisons whose result can have changed
/// and marks the readers of those that did.
fn decode<const F: bool>(p: &Program, ins: &Ins, st: &mut State, dirty: &mut [u64]) {
    let dec = &p.aux.decoders[ins.x as usize];
    let (x, last) = (ins.a as usize, ins.d as usize);
    let m = mask(ins.w);
    let nv = st.val[x];
    let nu = if F { st.unk[x] } else { 0 };
    let valid = st.val[last + 1] != 0;
    let ov = st.val[last];
    let ou = if F { st.unk[last] } else { 0 };
    if valid && nv == ov && nu == ou {
        return;
    }
    let mut set = |st: &mut State, d: u32, ne: bool, key: u64| {
        let d = d as usize;
        let (v, u) = if F && nu != 0 {
            if (nv ^ key) & !nu & m != 0 { (ne as u64, 0) } else { (0, 1) }
        } else {
            ((key == nv) as u64 ^ ne as u64, 0)
        };
        if st.val[d] != v || F && st.unk[d] != u {
            st.val[d] = v;
            if F {
                st.unk[d] = u;
            }
            mark(p, dirty, d as u32, u32::MAX);
        }
    };
    if !valid || F && (nu != 0 || ou != 0) {
        for &(key, d, ne) in &dec.entries {
            set(st, d, ne, key);
        }
    } else {
        for k in [ov, nv] {
            let mut j = dec.entries.partition_point(|e| e.0 < k);
            while j < dec.entries.len() && dec.entries[j].0 == k {
                let (key, d, ne) = dec.entries[j];
                set(st, d, ne, key);
                j += 1;
            }
        }
    }
    st.val[last] = nv;
    if F {
        st.unk[last] = nu;
    }
    st.val[last + 1] = 1;
}

/// Runs `f`, marking the readers of the `n` words at `d` if they change.
fn changed_words<const F: bool>(
    p: &Program,
    d: usize,
    n: usize,
    st: &mut State,
    dirty: &mut [u64],
    f: impl FnOnce(&mut State),
) {
    let mut snap = std::mem::take(&mut st.snap);
    snap.clear();
    snap.extend_from_slice(&st.val[d..d + n]);
    if F {
        snap.extend_from_slice(&st.unk[d..d + n]);
    }
    f(st);
    let changed = st.val[d..d + n] != snap[..n] || F && st.unk[d..d + n] != snap[n..];
    st.snap = snap;
    if changed {
        mark(p, dirty, d as u32, u32::MAX);
    }
}

/// Runs the units marked in `dirty`, in order, marking the readers of
/// every output that changes, until none is left.
pub fn settle<const F: bool>(p: &Program, st: &mut State, dirty: &mut [u64]) {
    let mut wi = 0;
    let end = p.ops.len().div_ceil(64);
    while wi < end {
        let bits = dirty[wi];
        if bits == 0 {
            wi += 1;
            continue;
        }
        dirty[wi] = bits & (bits - 1);
        st.units_run += 1;
        let i = wi * 64 + bits.trailing_zeros() as usize;
        let ins = &p.ops[i];
        if ins.k != K::Loop {
            if ins.k == K::Decode {
                decode::<F>(p, ins, st, dirty);
            } else {
                step_marked::<F>(p, i, ins, st, dirty);
            }
            continue;
        }
        let outs = &p.aux.lists[ins.b as usize..(ins.b + ins.c) as usize];
        let mut snap = std::mem::take(&mut st.snap);
        snap.clear();
        for &o in outs {
            snap.push(st.val[o as usize]);
            if F {
                snap.push(st.unk[o as usize]);
            }
        }
        let n = ins.a as usize;
        run_loop::<F>(&p.aux, ins, &p.ops[i + 1..i + 1 + n], st);
        let k = if F { 2 } else { 1 };
        for (j, &o) in outs.iter().enumerate() {
            let o = o as usize;
            if snap[k * j] != st.val[o] || F && snap[k * j + 1] != st.unk[o] {
                mark(p, dirty, p.slot_of[o], i as u32);
            }
        }
        st.snap = snap;
    }
}

/// Runs state updates, marking the readers of every state that changes.
pub fn commit<const F: bool>(p: &Program, ops: &[Ins], st: &mut State, dirty: &mut [u64]) {
    for ins in ops {
        let d = ins.d as usize;
        let n = nwords(ins.w) as usize;
        if n == 1 {
            let ov = st.val[d];
            let ou = if F { st.unk[d] } else { 0 };
            step::<F>(&p.aux, ins, st);
            if st.val[d] != ov || F && st.unk[d] != ou {
                mark(p, dirty, ins.d, u32::MAX);
            }
        } else {
            changed_words::<F>(p, d, n, st, dirty, |st| step::<F>(&p.aux, ins, st));
        }
    }
}

/// Runs the gated updates of a domain marked in `dirty`.
pub fn commit_gated<const F: bool>(p: &Program, dom: &Domain, st: &mut State, dirty: &mut [u64]) {
    let base = dom.base as usize;
    for wi in base / 64..(base + dom.gated.len()).div_ceil(64) {
        let mut bits = std::mem::take(&mut dirty[wi]);
        while bits != 0 {
            let i = wi * 64 + bits.trailing_zeros() as usize - base;
            bits &= bits - 1;
            let ins = &dom.gated[i];
            let d = ins.d as usize;
            let n = nwords(ins.w) as usize;
            if n == 1 {
                let ov = st.val[d];
                let ou = if F { st.unk[d] } else { 0 };
                step::<F>(&p.aux, ins, st);
                if st.val[d] != ov || F && st.unk[d] != ou {
                    mark(p, dirty, ins.d, u32::MAX);
                }
            } else {
                changed_words::<F>(p, d, n, st, dirty, |st| step::<F>(&p.aux, ins, st));
            }
        }
    }
}

/// Runs memory writes, marking the asynchronous readers of written memories.
pub fn write<const F: bool>(p: &Program, ops: &[Ins], st: &mut State, dirty: &mut [u64]) {
    for ins in ops {
        if mem_write::<F>(&p.aux, ins.x as u32, st) {
            let mem = p.aux.writes[ins.x as usize].mem as usize;
            for &r in &p.mem_readers[mem] {
                dirty[(r >> 6) as usize] |= 1 << (r & 63);
            }
        }
    }
}

/// Runs a decoder outside a settle: every comparison.
fn decode_all<const F: bool>(aux: &Aux, ins: &Ins, st: &mut State) {
    let dec = &aux.decoders[ins.x as usize];
    let x = ins.a as usize;
    let m = mask(ins.w);
    let nv = st.val[x];
    let nu = if F { st.unk[x] } else { 0 };
    for &(key, d, ne) in &dec.entries {
        let d = d as usize;
        if F && nu != 0 {
            let known_diff = (nv ^ key) & !nu & m != 0;
            st.val[d] = if known_diff { ne as u64 } else { 0 };
            st.unk[d] = !known_diff as u64;
        } else {
            st.val[d] = (key == nv) as u64 ^ ne as u64;
            if F {
                st.unk[d] = 0;
            }
        }
    }
    st.val[ins.d as usize] = nv;
    if F {
        st.unk[ins.d as usize] = nu;
    }
    st.val[ins.d as usize + 1] = 1;
}

/// Iterations before a combinational loop is declared unstable.
const LOOP_LIMIT: usize = 256;

/// Runs `ops` in order.
pub fn run<const F: bool>(aux: &Aux, ops: &[Ins], st: &mut State) {
    let mut i = 0;
    while i < ops.len() {
        let ins = &ops[i];
        i += 1;
        if ins.k == K::Loop {
            let n = ins.a as usize;
            run_loop::<F>(aux, ins, &ops[i..i + n], st);
            i += n;
        } else {
            step::<F>(aux, ins, st);
        }
    }
}

fn run_loop<const F: bool>(aux: &Aux, head: &Ins, body: &[Ins], st: &mut State) {
    let outs = &aux.lists[head.b as usize..(head.b + head.c) as usize];
    let mut scratch = std::mem::take(&mut st.scratch);
    for _ in 0..LOOP_LIMIT {
        scratch.clear();
        for &o in outs {
            scratch.push(st.val[o as usize]);
            if F {
                scratch.push(st.unk[o as usize]);
            }
        }
        for ins in body {
            step::<F>(aux, ins, st);
        }
        let stable = if F {
            outs.iter().enumerate().all(|(k, &o)| {
                scratch[2 * k] == st.val[o as usize] && scratch[2 * k + 1] == st.unk[o as usize]
            })
        } else {
            outs.iter().enumerate().all(|(k, &o)| scratch[k] == st.val[o as usize])
        };
        if stable {
            st.scratch = scratch;
            return;
        }
    }
    st.unstable += 1;
    st.scratch = scratch;
}

/// Sign-extends the low `w` bits.
#[inline(always)]
fn sext(x: u64, w: u32) -> i64 {
    if w == 0 { 0 } else { ((x << (64 - w)) as i64) >> (64 - w) }
}

/// The mask of word `j` of a value of `w` bits.
#[inline(always)]
fn word_mask(w: u32, j: usize) -> u64 {
    let lo = j as u32 * 64;
    if w >= lo + 64 { u64::MAX } else { mask(w.saturating_sub(lo)) }
}

/// Bits `pos..pos + w` (`w` at most 64) of the value in `words` of `aw`
/// bits, and the mask of the result bits that lie inside it.
#[inline]
pub fn extract(words: &[u64], aw: u32, pos: i128, w: u32) -> (u64, u64) {
    let awi = aw as i128;
    if w == 0 || pos >= awi || pos + w as i128 <= 0 {
        return (0, 0);
    }
    if pos >= 0 {
        let p = pos as usize;
        let (wi, sh) = (p / 64, p % 64);
        let mut r = words[wi] >> sh;
        if sh != 0 && wi + 1 < words.len() {
            r |= words[wi + 1] << (64 - sh);
        }
        let inr = mask((awi - pos).min(w as i128) as u32);
        (r & inr, inr)
    } else {
        let s = (-pos) as u32;
        let r = words[0] << s;
        let inr = mask((awi + s as i128).min(w as i128) as u32) & !mask(s);
        (r & inr, inr)
    }
}

#[inline(always)]
fn active<const F: bool>(st: &State, word: u32, bit: u64, pol: bool) -> bool {
    let w = word as usize;
    (st.val[w] >> bit & 1 != 0) == pol && (!F || st.unk[w] >> bit & 1 == 0)
}

#[inline(always)]
fn copy<const F: bool>(st: &mut State, d: usize, a: usize, n: usize) {
    if d == a {
        return;
    }
    if n == 1 {
        st.val[d] = st.val[a];
        if F {
            st.unk[d] = st.unk[a];
        }
    } else {
        st.val.copy_within(a..a + n, d);
        if F {
            st.unk.copy_within(a..a + n, d);
        }
    }
}

/// Sets `n` words of `d` to `x` for a value of `w` bits.
fn set_x<const F: bool>(st: &mut State, d: usize, w: u32) {
    for j in 0..nwords(w) as usize {
        st.val[d + j] = 0;
        if F {
            st.unk[d + j] = word_mask(w, j);
        }
    }
}

fn lv<const F: bool>(st: &State, off: u32, w: u32) -> LogicVec {
    let (o, n) = (off as usize, nwords(w) as usize);
    let unk: &[u64] = if F { &st.unk[o..o + n] } else { &[] };
    LogicVec::from_planes(w as usize, &st.val[o..o + n], unk)
}

fn store<const F: bool>(st: &mut State, d: Val, r: &LogicVec) {
    let r = r.resize(d.w as usize, false);
    for j in 0..d.words() as usize {
        let (v, u) = (r.val_words()[j], r.unk_words()[j]);
        if F {
            st.val[d.off as usize + j] = v;
            st.unk[d.off as usize + j] = u;
        } else {
            st.val[d.off as usize + j] = v & !u;
        }
    }
}

fn unary_op(k: K) -> UnaryOp {
    match k {
        K::Not => UnaryOp::Not,
        K::Neg => UnaryOp::Neg,
        K::RedAnd => UnaryOp::ReduceAnd,
        K::RedOr => UnaryOp::ReduceOr,
        K::RedXor => UnaryOp::ReduceXor,
        K::RedXnor => UnaryOp::ReduceXnor,
        _ => UnaryOp::LogicNot,
    }
}

fn binary_op(k: K) -> (BinaryOp, bool) {
    match k {
        K::And => (BinaryOp::And, false),
        K::Or => (BinaryOp::Or, false),
        K::Xor => (BinaryOp::Xor, false),
        K::Xnor => (BinaryOp::Xnor, false),
        K::Add => (BinaryOp::Add, false),
        K::Sub => (BinaryOp::Sub, false),
        K::Mul => (BinaryOp::Mul, false),
        K::DivU => (BinaryOp::Div, false),
        K::DivS => (BinaryOp::Div, true),
        K::ModU => (BinaryOp::Mod, false),
        K::ModS => (BinaryOp::Mod, true),
        K::Shl => (BinaryOp::Shl, false),
        K::Shr => (BinaryOp::Shr, false),
        K::Sshr => (BinaryOp::Sshr, false),
        K::Eq => (BinaryOp::Eq, false),
        K::Ne => (BinaryOp::Ne, false),
        K::Eqx => (BinaryOp::Eqx, false),
        K::Nex => (BinaryOp::Nex, false),
        K::LtU => (BinaryOp::Lt, false),
        K::LtS => (BinaryOp::Lt, true),
        K::LeU => (BinaryOp::Le, false),
        K::LeS => (BinaryOp::Le, true),
        K::GtU => (BinaryOp::Gt, false),
        K::GtS => (BinaryOp::Gt, true),
        K::GeU => (BinaryOp::Ge, false),
        K::GeS => (BinaryOp::Ge, true),
        K::LAnd => (BinaryOp::LogicAnd, false),
        _ => (BinaryOp::LogicOr, false),
    }
}

/// Evaluates a narrow instruction through `synth_rtl::eval`.
#[cold]
fn slow<const F: bool>(ins: &Ins, st: &mut State) {
    let a = lv::<F>(st, ins.a, ins.w);
    let r = match ins.k {
        K::Not | K::Neg | K::RedAnd | K::RedOr | K::RedXor | K::RedXnor | K::LogicNot => {
            eval::unary(unary_op(ins.k), &a)
        }
        k => {
            let bw = match k {
                K::Shl | K::Shr | K::Sshr | K::LAnd | K::LOr => ins.c,
                _ => ins.w,
            };
            let b = lv::<F>(st, ins.b, bw);
            let (op, signed) = binary_op(k);
            eval::binary(op, signed, &a, &b)
        }
    };
    let d = ins.d as usize;
    if F {
        st.val[d] = r.val_words()[0];
        st.unk[d] = r.unk_words()[0];
    } else {
        st.val[d] = r.val_words()[0] & !r.unk_words()[0];
    }
}

#[cold]
fn wide<const F: bool>(aux: &Aux, ins: &Ins, st: &mut State) {
    let wi = &aux.wide[ins.x as usize];
    let a = lv::<F>(st, wi.a.off, wi.a.w);
    let r = match &wi.op {
        WideOp::Unary(op) => eval::unary(*op, &a),
        WideOp::Binary(op, signed) => {
            let b = lv::<F>(st, wi.b.off, wi.b.w);
            eval::binary(*op, *signed, &a, &b)
        }
        WideOp::Shiftx { b_signed, offset } => {
            let b = lv::<F>(st, wi.b.off, wi.b.w);
            eval::shiftx(&a, &b, *b_signed, *offset, wi.d.w as usize)
        }
    };
    store::<F>(st, wi.d, &r);
}

fn gather<const F: bool>(aux: &Aux, ins: &Ins, st: &mut State) {
    let n = nwords(ins.w) as usize;
    let d = ins.d as usize;
    if ins.c != NONE {
        copy::<F>(st, d, ins.c as usize, n);
    } else {
        for j in 0..n {
            st.val[d + j] = 0;
            if F {
                st.unk[d + j] = 0;
            }
        }
    }
    for p in &aux.parts[ins.a as usize..(ins.a + ins.b) as usize] {
        let m = mask(p.len as u32);
        let o = d + p.dst as usize;
        st.val[o] |= (st.val[p.src as usize] >> p.lo & m) << p.pos;
        if F {
            st.unk[o] |= (st.unk[p.src as usize] >> p.lo & m) << p.pos;
        }
    }
}

fn pmux<const F: bool>(aux: &Aux, ins: &Ins, st: &mut State) {
    let (d, a, b) = (ins.d as usize, ins.a as usize, ins.b as usize);
    let n = nwords(ins.w) as usize;
    let sw = ins.c;
    let cases = &aux.lists[ins.x as usize..ins.x as usize + sw as usize];
    let src = if sw <= 64 {
        if F && st.unk[b] & mask(sw) != 0 {
            None
        } else {
            let s = st.val[b] & mask(sw);
            if s == 0 {
                Some(a)
            } else if s & (s - 1) == 0 {
                Some(cases[s.trailing_zeros() as usize] as usize)
            } else {
                None
            }
        }
    } else {
        let mut found = Some(a);
        let mut count = 0;
        for j in 0..nwords(sw) as usize {
            let m = word_mask(sw, j);
            if F && st.unk[b + j] & m != 0 {
                count = 2;
                break;
            }
            let s = st.val[b + j] & m;
            count += s.count_ones();
            if s != 0 {
                found = Some(cases[j * 64 + s.trailing_zeros() as usize] as usize);
            }
        }
        if count > 1 { None } else { found }
    };
    match src {
        Some(s) => copy::<F>(st, d, s, n),
        None => set_x::<F>(st, d, ins.w),
    }
}

fn shiftx<const F: bool>(aux: &Aux, ins: &Ins, st: &mut State) {
    let info = &aux.shiftx[ins.c as usize];
    let (d, b, w) = (ins.d as usize, info.b.off as usize, ins.w);
    if F && st.unk[b] != 0 {
        set_x::<F>(st, d, w);
        return;
    }
    let bv = st.val[b];
    let idx = if info.b_signed { sext(bv, info.b.w) as i128 } else { bv as i128 };
    let pos = idx + info.offset as i128;
    let (a, na) = (info.a.off as usize, info.a.words() as usize);
    let (r, inr) = extract(&st.val[a..a + na], info.a.w, pos, w);
    st.val[d] = r;
    if F {
        let (ru, _) = extract(&st.unk[a..a + na], info.a.w, pos, w);
        st.unk[d] = ru | (!inr & mask(w));
    }
}

fn mem_read<const F: bool>(aux: &Aux, mi: u32, addr: u32, d: usize, w: u32, st: &mut State) {
    let mem = &aux.mems[mi as usize];
    let n = nwords(w) as usize;
    let a = addr as usize;
    if F && st.unk[a] != 0 {
        set_x::<F>(st, d, w);
        return;
    }
    let idx = st.val[a] as i128 - mem.offset as i128;
    if idx < 0 || idx >= mem.depth as i128 {
        set_x::<F>(st, d, w);
        return;
    }
    let o = mem.base + idx as usize * n;
    for j in 0..n {
        st.val[d + j] = st.mval[o + j];
        if F {
            st.unk[d + j] = st.munk[o + j];
        }
    }
}

/// Returns whether anything was written.
fn mem_write<const F: bool>(aux: &Aux, wi: u32, st: &mut State) -> bool {
    let wr = &aux.writes[wi as usize];
    let mem = &aux.mems[wr.mem as usize];
    let a = wr.addr.off as usize;
    if F && st.unk[a] != 0 {
        return false;
    }
    let idx = st.val[a] as i128 - mem.offset as i128;
    if idx < 0 || idx >= mem.depth as i128 {
        return false;
    }
    let mut wrote = false;
    let n = nwords(mem.width) as usize;
    let o = mem.base + idx as usize * n;
    let (en, data) = (wr.en.off as usize, wr.data.off as usize);
    for j in 0..n {
        let e = if F { st.val[en + j] & !st.unk[en + j] } else { st.val[en + j] };
        if e == 0 {
            continue;
        }
        wrote = true;
        st.mval[o + j] = st.mval[o + j] & !e | st.val[data + j] & e;
        if F {
            st.munk[o + j] = st.munk[o + j] & !e | st.unk[data + j] & e;
        }
    }
    wrote
}

fn sync_read<const F: bool>(aux: &Aux, x: u64, st: &mut State) {
    let r = &aux.sync_reads[x as usize];
    let mem = &aux.mems[r.mem as usize];
    let (d, n) = (r.dst as usize, nwords(mem.width) as usize);
    if let Some(en) = r.en
        && !active::<F>(st, en.word, en.bit as u64, en.pol)
    {
        copy::<F>(st, d, r.q as usize, n);
        return;
    }
    mem_read::<F>(aux, r.mem, r.addr.off, d, mem.width, st);
    for &wi in &r.transparent {
        let wr = &aux.writes[wi as usize];
        let (wa, ra) = (wr.addr.off as usize, r.addr.off as usize);
        if F && (st.unk[wa] != 0 || st.unk[ra] != 0) {
            continue;
        }
        if st.val[wa] != st.val[ra] {
            continue;
        }
        let (en, data) = (wr.en.off as usize, wr.data.off as usize);
        for j in 0..n {
            let e = if F { st.val[en + j] & !st.unk[en + j] } else { st.val[en + j] };
            st.val[d + j] = st.val[d + j] & !e | st.val[data + j] & e;
            if F {
                st.unk[d + j] = st.unk[d + j] & !e | st.unk[data + j] & e;
            }
        }
    }
}

fn flop<const F: bool>(aux: &Aux, x: u64, st: &mut State) {
    let f = &aux.flops[x as usize];
    let on = |st: &State, c: Ctl| active::<F>(st, c.word, c.bit as u64, c.pol);
    let en = f.en.is_none_or(|c| on(st, c));
    let src = if let Some((c, v)) = f.arst
        && on(st, c)
    {
        v
    } else if let Some((c, v)) = f.srst
        && on(st, c)
        && (!f.en_over_srst || en)
    {
        v
    } else if en {
        f.d
    } else {
        f.q
    };
    copy::<F>(st, f.dst as usize, src as usize, nwords(f.w) as usize);
}

macro_rules! bin {
    ($st:ident, $ins:ident, $F:ident, |$x:ident, $y:ident| $e:expr) => {{
        let (d, a, b) = ($ins.d as usize, $ins.a as usize, $ins.b as usize);
        if !$F || ($st.unk[a] | $st.unk[b]) == 0 {
            let ($x, $y) = ($st.val[a], $st.val[b]);
            $st.val[d] = $e;
            if $F {
                $st.unk[d] = 0;
            }
        } else {
            slow::<$F>($ins, $st);
        }
    }};
}

macro_rules! un {
    ($st:ident, $ins:ident, $F:ident, |$x:ident| $e:expr) => {{
        let (d, a) = ($ins.d as usize, $ins.a as usize);
        if !$F || $st.unk[a] == 0 {
            let $x = $st.val[a];
            $st.val[d] = $e;
            if $F {
                $st.unk[d] = 0;
            }
        } else {
            slow::<$F>($ins, $st);
        }
    }};
}

macro_rules! shift {
    ($st:ident, $ins:ident, $F:ident, |$x:ident, $n:ident, $w:ident| $e:expr) => {{
        let (d, a, b) = ($ins.d as usize, $ins.a as usize, $ins.b as usize);
        let $w = $ins.w;
        if $F && $st.unk[b] & mask($ins.c) != 0 {
            set_x::<$F>($st, d, $w);
        } else {
            let $n = $st.val[b];
            let $x = $st.val[a];
            $st.val[d] = $e;
            if $F {
                let $x = $st.unk[a];
                $st.unk[d] = $e;
            }
        }
    }};
}

/// Runs one instruction (not [`K::Loop`]).
#[inline(always)]
pub fn step<const F: bool>(aux: &Aux, ins: &Ins, st: &mut State) {
    let w = ins.w;
    let m = mask(w);
    match ins.k {
        K::Copy => copy::<F>(st, ins.d as usize, ins.a as usize, nwords(w) as usize),
        K::Gather => gather::<F>(aux, ins, st),
        K::Extract => {
            let (d, a) = (ins.d as usize, ins.a as usize);
            st.val[d] = st.val[a] >> ins.x & m;
            if F {
                st.unk[d] = st.unk[a] >> ins.x & m;
            }
        }
        K::Not => {
            let (d, a) = (ins.d as usize, ins.a as usize);
            if F {
                st.val[d] = !st.val[a] & !st.unk[a] & m;
                st.unk[d] = st.unk[a];
            } else {
                st.val[d] = !st.val[a] & m;
            }
        }
        K::Neg => un!(st, ins, F, |x| x.wrapping_neg() & m),
        K::RedAnd => un!(st, ins, F, |x| (x == m) as u64),
        K::RedOr => un!(st, ins, F, |x| (x != 0) as u64),
        K::RedXor => un!(st, ins, F, |x| (x.count_ones() & 1) as u64),
        K::RedXnor => un!(st, ins, F, |x| (x.count_ones() & 1 == 0) as u64),
        K::LogicNot => un!(st, ins, F, |x| (x == 0) as u64),
        K::And => {
            let (d, a, b) = (ins.d as usize, ins.a as usize, ins.b as usize);
            if F {
                let (av, au, bv, bu) = (st.val[a], st.unk[a], st.val[b], st.unk[b]);
                let one = av & !au & bv & !bu;
                let zero = !av & !au | !bv & !bu;
                st.val[d] = one;
                st.unk[d] = !(one | zero) & m;
            } else {
                st.val[d] = st.val[a] & st.val[b];
            }
        }
        K::Or => {
            let (d, a, b) = (ins.d as usize, ins.a as usize, ins.b as usize);
            if F {
                let (av, au, bv, bu) = (st.val[a], st.unk[a], st.val[b], st.unk[b]);
                let one = av & !au | bv & !bu;
                let zero = !av & !au & !bv & !bu;
                st.val[d] = one & m;
                st.unk[d] = !(one | zero) & m;
            } else {
                st.val[d] = st.val[a] | st.val[b];
            }
        }
        K::Xor | K::Xnor => {
            let (d, a, b) = (ins.d as usize, ins.a as usize, ins.b as usize);
            let inv = if ins.k == K::Xnor { m } else { 0 };
            if F {
                let u = (st.unk[a] | st.unk[b]) & m;
                st.val[d] = (st.val[a] ^ st.val[b] ^ inv) & !u;
                st.unk[d] = u;
            } else {
                st.val[d] = st.val[a] ^ st.val[b] ^ inv;
            }
        }
        K::Add => bin!(st, ins, F, |x, y| x.wrapping_add(y) & m),
        K::Sub => bin!(st, ins, F, |x, y| x.wrapping_sub(y) & m),
        K::Mul => bin!(st, ins, F, |x, y| x.wrapping_mul(y) & m),
        K::DivU | K::DivS | K::ModU | K::ModS => {
            let (d, a, b) = (ins.d as usize, ins.a as usize, ins.b as usize);
            if F && ((st.unk[a] | st.unk[b]) != 0 || st.val[b] == 0) {
                slow::<F>(ins, st);
            } else {
                let (x, y) = (st.val[a], st.val[b]);
                // Division by zero gives 0 (two-state).
                st.val[d] = match ins.k {
                    K::DivU => x.checked_div(y).unwrap_or(0),
                    K::ModU => x.checked_rem(y).unwrap_or(0),
                    _ if y == 0 => 0,
                    K::DivS => sext(x, w).wrapping_div(sext(y, w)) as u64 & m,
                    _ => sext(x, w).wrapping_rem(sext(y, w)) as u64 & m,
                };
                if F {
                    st.unk[d] = 0;
                }
            }
        }
        K::Shl => shift!(st, ins, F, |x, n, w| if n >= w as u64 { 0 } else { x << n & m }),
        K::Shr => shift!(st, ins, F, |x, n, w| if n >= w as u64 { 0 } else { x >> n }),
        K::Sshr => shift!(st, ins, F, |x, n, w| (sext(x, w) >> n.min(63)) as u64 & m),
        K::Eq => bin!(st, ins, F, |x, y| (x == y) as u64),
        K::Ne => bin!(st, ins, F, |x, y| (x != y) as u64),
        K::Eqx | K::Nex => {
            let (d, a, b) = (ins.d as usize, ins.a as usize, ins.b as usize);
            let eq = st.val[a] == st.val[b] && (!F || st.unk[a] == st.unk[b]);
            st.val[d] = (eq == (ins.k == K::Eqx)) as u64;
            if F {
                st.unk[d] = 0;
            }
        }
        K::LtU => bin!(st, ins, F, |x, y| (x < y) as u64),
        K::LeU => bin!(st, ins, F, |x, y| (x <= y) as u64),
        K::GtU => bin!(st, ins, F, |x, y| (x > y) as u64),
        K::GeU => bin!(st, ins, F, |x, y| (x >= y) as u64),
        K::LtS => bin!(st, ins, F, |x, y| (sext(x, w) < sext(y, w)) as u64),
        K::LeS => bin!(st, ins, F, |x, y| (sext(x, w) <= sext(y, w)) as u64),
        K::GtS => bin!(st, ins, F, |x, y| (sext(x, w) > sext(y, w)) as u64),
        K::GeS => bin!(st, ins, F, |x, y| (sext(x, w) >= sext(y, w)) as u64),
        K::LAnd => bin!(st, ins, F, |x, y| (x != 0 && y != 0) as u64),
        K::LOr => bin!(st, ins, F, |x, y| (x != 0 || y != 0) as u64),
        K::Mux => {
            let (d, a, b, c) = (ins.d as usize, ins.a as usize, ins.b as usize, ins.c as usize);
            let bit = ins.x & 63;
            let n = nwords(w) as usize;
            if F && st.unk[c] >> bit & 1 != 0 {
                if ins.x >> 8 & 1 != 0 {
                    for j in 0..n {
                        let (av, au, bv, bu) =
                            (st.val[a + j], st.unk[a + j], st.val[b + j], st.unk[b + j]);
                        let eq = !(av ^ bv) & !au & !bu;
                        st.val[d + j] = av & eq;
                        st.unk[d + j] = !eq & word_mask(w, j);
                    }
                } else {
                    copy::<F>(st, d, a, n);
                }
            } else {
                let src = if st.val[c] >> bit & 1 != 0 { b } else { a };
                copy::<F>(st, d, src, n);
            }
        }
        K::Pmux => pmux::<F>(aux, ins, st),
        K::Shiftx => shiftx::<F>(aux, ins, st),
        K::MemRd => mem_read::<F>(aux, ins.c, ins.a, ins.d as usize, w, st),
        K::Latch | K::Arst => {
            if active::<F>(st, ins.c, ins.x & 63, ins.x >> 8 & 1 != 0) {
                copy::<F>(st, ins.d as usize, ins.a as usize, nwords(w) as usize);
            }
        }
        K::FfEn => {
            let src =
                if active::<F>(st, ins.c, ins.x & 63, ins.x >> 8 & 1 != 0) { ins.a } else { ins.b };
            copy::<F>(st, ins.d as usize, src as usize, nwords(w) as usize);
        }
        K::Ff => flop::<F>(aux, ins.x, st),
        K::MemWr => {
            mem_write::<F>(aux, ins.x as u32, st);
        }
        K::MemRdSync => sync_read::<F>(aux, ins.x, st),
        K::Wide => wide::<F>(aux, ins, st),
        K::Decode => decode_all::<F>(aux, ins, st),
        K::Loop => unreachable!("loops are run by `run`"),
    }
}
