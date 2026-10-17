//! DAG-aware rewriting (after Mishchenko, Chatterjee and Brayton, "DAG-aware
//! AIG rewriting: a fresh look at combinational logic synthesis", DAC 2006):
//! the function of every 4-input cut of a node is re-expressed with small
//! structures, and a structure replaces the node's cone when the nodes it
//! adds (logic already in the graph is free) are fewer than the nodes the
//! cone frees.
//!
//! Structures come from decompositions of the cut function (factored
//! irredundant covers, Shannon and XOR expansion, disjoint-support
//! bi-decomposition), built with functional hashing so that repeated
//! subfunctions are shared.

use synth_core::FxHashMap;

use crate::aig::{Aig, Lit};
use crate::edit::EditAig;

/// Truth tables of the four variables over 16 minterms.
const VARS: [u16; 4] = [0xAAAA, 0xCCCC, 0xF0F0, 0xFF00];

#[derive(Clone, Debug)]
pub struct RewriteOptions {
    /// Also accept replacements that keep the node count (they change the
    /// structure, which helps later passes and choices).
    pub zero_gain: bool,
    /// Reject replacements that make a node deeper.
    pub preserve_levels: bool,
    /// Cuts kept per node.
    pub cuts_per_node: usize,
}

impl Default for RewriteOptions {
    fn default() -> Self {
        RewriteOptions { zero_gain: false, preserve_levels: true, cuts_per_node: 12 }
    }
}

/// A cut of at most four leaves and the node's function over them
/// (replicated over the unused variables).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Cut {
    leaves: [u32; 4],
    len: u8,
    truth: u16,
}

impl Cut {
    fn leaves(&self) -> &[u32] {
        &self.leaves[..self.len as usize]
    }

    fn dominates(&self, other: &Cut) -> bool {
        self.len <= other.len && self.leaves().iter().all(|l| other.leaves().contains(l))
    }

    fn merge(a: &Cut, b: &Cut) -> Option<Cut> {
        let mut leaves = [0u32; 4];
        let mut n = 0;
        let (x, y) = (a.leaves(), b.leaves());
        let (mut i, mut j) = (0, 0);
        while i < x.len() || j < y.len() {
            let v = if j == y.len() || (i < x.len() && x[i] < y[j]) {
                i += 1;
                x[i - 1]
            } else if i == x.len() || y[j] < x[i] {
                j += 1;
                y[j - 1]
            } else {
                i += 1;
                j += 1;
                x[i - 1]
            };
            if n == 4 {
                return None;
            }
            leaves[n] = v;
            n += 1;
        }
        Some(Cut { leaves, len: n as u8, truth: 0 })
    }
}

/// Re-expresses table `t` over leaves `from` as a table over `to` (a sorted
/// superset).
fn expand(t: u16, from: &[u32], to: &[u32]) -> u16 {
    if from == to {
        return t;
    }
    let mut pos = [0usize; 4];
    for (j, l) in from.iter().enumerate() {
        pos[j] = to.iter().position(|x| x == l).expect("leaf in merged cut");
    }
    let mut out = 0u16;
    for idx in 0..16usize {
        let mut src = 0;
        for (j, &p) in pos[..from.len()].iter().enumerate() {
            src |= ((idx >> p) & 1) << j;
        }
        out |= ((t >> src) & 1) << idx;
    }
    out
}

/// A small AIG over four variables. Node 0 is the constant false, nodes 1 to
/// 4 the variables and node `5 + j` gate `j`; literals are
/// `node << 1 | complement`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Structure {
    gates: Vec<(u8, u8)>,
    out: u8,
}

/// Builds structures with functional hashing over 16-minterm tables.
#[derive(Clone)]
struct Builder {
    gates: Vec<(u8, u8)>,
    /// Literal computing each table (and its complement).
    by_truth: FxHashMap<u16, u8>,
}

impl Builder {
    fn new() -> Builder {
        let mut by_truth = FxHashMap::default();
        by_truth.insert(0, 0);
        by_truth.insert(0xFFFF, 1);
        for (v, &t) in VARS.iter().enumerate() {
            let l = ((v + 1) << 1) as u8;
            by_truth.insert(t, l);
            by_truth.insert(!t, l | 1);
        }
        Builder { gates: Vec::new(), by_truth }
    }

    fn lit(&self, t: u16) -> Option<u8> {
        self.by_truth.get(&t).copied()
    }

    fn and(&mut self, a: (u8, u16), b: (u8, u16)) -> (u8, u16) {
        let t = a.1 & b.1;
        if let Some(l) = self.lit(t) {
            return (l, t);
        }
        let l = ((5 + self.gates.len()) << 1) as u8;
        self.gates.push((a.0, b.0));
        self.by_truth.insert(t, l);
        self.by_truth.insert(!t, l | 1);
        (l, t)
    }

    fn or(&mut self, a: (u8, u16), b: (u8, u16)) -> (u8, u16) {
        let (l, t) = self.and(not(a), not(b));
        (l ^ 1, !t)
    }

    fn xor(&mut self, a: (u8, u16), b: (u8, u16)) -> (u8, u16) {
        let x = self.and(a, not(b));
        let y = self.and(not(a), b);
        self.or(x, y)
    }

    fn mux(&mut self, s: (u8, u16), t: (u8, u16), e: (u8, u16)) -> (u8, u16) {
        let x = self.and(s, t);
        let y = self.and(not(s), e);
        self.or(x, y)
    }

    fn finish(self, out: u8) -> Structure {
        Structure { gates: self.gates, out }
    }
}

fn not(a: (u8, u16)) -> (u8, u16) {
    (a.0 ^ 1, !a.1)
}

fn var(v: usize) -> (u8, u16) {
    (((v + 1) << 1) as u8, VARS[v])
}

fn cof0(t: u16, v: usize) -> u16 {
    let x = t & !VARS[v];
    x | (x << (1 << v))
}

fn cof1(t: u16, v: usize) -> u16 {
    let x = t & VARS[v];
    x | (x >> (1 << v))
}

fn depends(t: u16, v: usize) -> bool {
    cof0(t, v) != cof1(t, v)
}

/// A product term over four variables.
#[derive(Copy, Clone, Debug)]
struct Cube {
    pos: u8,
    neg: u8,
}

/// Minato-Morreale irredundant cover of a function between `on` and `ondc`.
fn isop(on: u16, ondc: u16, nvars: usize, out: &mut Vec<Cube>) -> u16 {
    if on == 0 {
        return 0;
    }
    if ondc == 0xFFFF {
        out.push(Cube { pos: 0, neg: 0 });
        return 0xFFFF;
    }
    let v = (0..nvars)
        .rev()
        .find(|&v| depends(on, v) || depends(ondc, v))
        .expect("a non-constant function depends on some variable");
    let (on0, on1) = (cof0(on, v), cof1(on, v));
    let (dc0, dc1) = (cof0(ondc, v), cof1(ondc, v));
    let start = out.len();
    let f0 = isop(on0 & !dc1, dc0, v, out);
    for c in &mut out[start..] {
        c.neg |= 1 << v;
    }
    let mid = out.len();
    let f1 = isop(on1 & !dc0, dc1, v, out);
    for c in &mut out[mid..] {
        c.pos |= 1 << v;
    }
    let fs = isop((on0 & !f0) | (on1 & !f1), dc0 & dc1, v, out);
    (f0 & !VARS[v]) | (f1 & VARS[v]) | fs
}

/// Builds a cover as a factored form (dividing by the most frequent
/// literal).
fn factor(b: &mut Builder, cubes: &[Cube]) -> (u8, u16) {
    if cubes.is_empty() {
        return (0, 0);
    }
    let mut best = (0, 0usize, false);
    for v in 0..4 {
        for neg in [false, true] {
            let n =
                cubes.iter().filter(|c| (if neg { c.neg } else { c.pos }) >> v & 1 == 1).count();
            if n > best.0 {
                best = (n, v, neg);
            }
        }
    }
    if best.0 == 0 {
        return (1, 0xFFFF);
    }
    if best.0 == 1 || cubes.len() == 1 {
        // No literal shared: a sum of single cubes.
        let mut acc = (0u8, 0u16);
        for c in cubes {
            let mut p = (1u8, 0xFFFFu16);
            for v in 0..4 {
                if c.pos >> v & 1 == 1 {
                    p = b.and(p, var(v));
                } else if c.neg >> v & 1 == 1 {
                    p = b.and(p, not(var(v)));
                }
            }
            acc = b.or(acc, p);
        }
        return acc;
    }
    let (_, v, neg) = best;
    let bit = 1u8 << v;
    let has = |c: &Cube| (if neg { c.neg } else { c.pos }) & bit != 0;
    let quotient: Vec<Cube> =
        cubes
            .iter()
            .filter(|c| has(c))
            .map(|c| {
                if neg {
                    Cube { neg: c.neg & !bit, ..*c }
                } else {
                    Cube { pos: c.pos & !bit, ..*c }
                }
            })
            .collect();
    let rest: Vec<Cube> = cubes.iter().filter(|c| !has(c)).copied().collect();
    let q = factor(b, &quotient);
    let l = if neg { not(var(v)) } else { var(v) };
    let t = b.and(l, q);
    if rest.is_empty() {
        return t;
    }
    let r = factor(b, &rest);
    b.or(t, r)
}

/// How a function decomposes (see [`plan`]).
#[derive(Copy, Clone, Debug)]
enum Step {
    Lit,
    And(u16, u16),
    Or(u16, u16),
    Xor(u16, u16),
    Mux(usize, u16, u16),
}

/// The cheapest decomposition of `t` into disjoint-support ANDs, ORs and
/// XORs and Shannon expansions, counting gates without sharing; memoised.
fn plan(t: u16, memo: &mut FxHashMap<u16, (u32, Step)>) -> u32 {
    if let Some(&(c, _)) = memo.get(&t) {
        return c;
    }
    let supp: Vec<usize> = (0..4).filter(|&v| depends(t, v)).collect();
    if supp.len() <= 1 {
        memo.insert(t, (0, Step::Lit));
        return 0;
    }
    let exists = |t: u16, vs: &[usize]| vs.iter().fold(t, |t, &v| cof0(t, v) | cof1(t, v));
    let forall = |t: u16, vs: &[usize]| vs.iter().fold(t, |t, &v| cof0(t, v) & cof1(t, v));
    let restrict0 = |t: u16, vs: &[usize]| vs.iter().fold(t, |t, &v| cof0(t, v));
    let mut best = (u32::MAX, Step::Lit);
    let n = supp.len();
    // Each split of the support once (the first variable on the left).
    for mask in (1..(1u32 << n) - 1).step_by(2) {
        let (mut a, mut c) = (Vec::new(), Vec::new());
        for (k, &v) in supp.iter().enumerate() {
            if mask >> k & 1 == 1 { a.push(v) } else { c.push(v) }
        }
        let (g, h) = (exists(t, &c), exists(t, &a));
        if g & h == t {
            let cost = 1 + plan(g, memo) + plan(h, memo);
            if cost < best.0 {
                best = (cost, Step::And(g, h));
            }
        }
        let (g, h) = (forall(t, &c), forall(t, &a));
        if g | h == t {
            let cost = 1 + plan(g, memo) + plan(h, memo);
            if cost < best.0 {
                best = (cost, Step::Or(g, h));
            }
        }
        let g = restrict0(t, &c);
        let h = restrict0(t, &a) ^ restrict0(restrict0(t, &a), &c);
        if g ^ h == t {
            let cost = 3 + plan(g, memo) + plan(h, memo);
            if cost < best.0 {
                best = (cost, Step::Xor(g, h));
            }
        }
    }
    for &v in &supp {
        let (f0, f1) = (cof0(t, v), cof1(t, v));
        let cost = 3 + plan(f1, memo) + plan(f0, memo);
        if cost < best.0 {
            best = (cost, Step::Mux(v, f1, f0));
        }
    }
    memo.insert(t, best);
    best.0
}

/// Builds `t` following its [`plan`].
fn build_plan(b: &mut Builder, t: u16, memo: &mut FxHashMap<u16, (u32, Step)>) -> (u8, u16) {
    if let Some(l) = b.lit(t) {
        return (l, t);
    }
    plan(t, memo);
    match memo[&t].1 {
        Step::Lit => unreachable!("literals are in the builder"),
        Step::And(g, h) => {
            let x = build_plan(b, g, memo);
            let y = build_plan(b, h, memo);
            b.and(x, y)
        }
        Step::Or(g, h) => {
            let x = build_plan(b, g, memo);
            let y = build_plan(b, h, memo);
            b.or(x, y)
        }
        Step::Xor(g, h) => {
            let x = build_plan(b, g, memo);
            let y = build_plan(b, h, memo);
            b.xor(x, y)
        }
        Step::Mux(v, f1, f0) => {
            let x1 = build_plan(b, f1, memo);
            let x0 = build_plan(b, f0, memo);
            b.mux(var(v), x1, x0)
        }
    }
}

/// Candidate structures for a function, fewest gates first.
fn structures(t: u16, memo: &mut FxHashMap<u16, (u32, Step)>) -> Vec<Structure> {
    let mut out: Vec<Structure> = Vec::new();
    let b = Builder::new();
    if let Some(l) = b.lit(t) {
        out.push(b.finish(l));
        return out;
    }
    let mut add = |s: Structure| {
        if !out.contains(&s) && s.gates.len() <= MAX_GATES {
            out.push(s);
        }
    };
    for compl in [false, true] {
        let f = if compl { !t } else { t };
        let mut cover = Vec::new();
        isop(f, f, 4, &mut cover);
        let mut b = Builder::new();
        let (l, _) = factor(&mut b, &cover);
        add(b.finish(l ^ compl as u8));
    }
    let mut b = Builder::new();
    let (l, _) = build_plan(&mut b, t, memo);
    add(b.finish(l));
    out.sort_by_key(|s| s.gates.len());
    out
}

/// Gates of the largest structure (nodes are numbered in a byte).
const MAX_GATES: usize = 59;

/// Small structures for functions of up to four variables, derived on
/// demand.
#[derive(Debug, Default)]
pub struct Library {
    plans: FxHashMap<u16, (u32, Step)>,
    structures: FxHashMap<u16, Vec<Structure>>,
}

impl Library {
    fn get(&mut self, t: u16) -> &[Structure] {
        if !self.structures.contains_key(&t) {
            let s = structures(t, &mut self.plans);
            self.structures.insert(t, s);
        }
        &self.structures[&t]
    }

    /// Builds the smallest structure known for the function with table `t`
    /// (bit `i` is the value when variable `j` has the value of bit `j` of
    /// `i`) over `vars` (at most four) into `g`.
    pub fn build(&mut self, g: &mut Aig, t: u64, vars: &[Lit]) -> Lit {
        assert!(vars.len() <= 4);
        // Replicate the table over the unused variables.
        let mut t16 = 0u16;
        for i in 0..16 {
            let src = i & ((1 << vars.len()) - 1);
            t16 |= ((t >> src) as u16 & 1) << i;
        }
        let s = &self.get(t16)[0];
        let mut lits = [Lit::FALSE; 64];
        lits[1..=vars.len()].copy_from_slice(vars);
        for (j, &(x, y)) in s.gates.iter().enumerate() {
            let a = lits[(x >> 1) as usize].xor(x & 1 == 1);
            let b = lits[(y >> 1) as usize].xor(y & 1 == 1);
            lits[5 + j] = g.and(a, b);
        }
        lits[(s.out >> 1) as usize].xor(s.out & 1 == 1)
    }
}

struct Rewriter<'a> {
    e: EditAig,
    opts: &'a RewriteOptions,
    cuts: Vec<Option<Vec<Cut>>>,
    library: Library,
}

impl Rewriter<'_> {
    fn cuts_of(&mut self, n: u32) -> &[Cut] {
        if self.cuts.len() <= n as usize {
            self.cuts.resize(self.e.len(), None);
        }
        if self.cuts[n as usize].is_none() {
            let trivial = Cut { leaves: [n, 0, 0, 0], len: 1, truth: VARS[0] };
            let set = match self.e.fanins(n) {
                None => vec![trivial],
                Some((a, b)) => {
                    let ca = self.cuts_of(a.node()).to_vec();
                    let cb = self.cuts_of(b.node()).to_vec();
                    let mut set: Vec<Cut> = Vec::new();
                    for x in &ca {
                        for y in &cb {
                            let Some(mut c) = Cut::merge(x, y) else { continue };
                            if set.iter().any(|d| d.dominates(&c)) {
                                continue;
                            }
                            let ta = expand(x.truth, x.leaves(), c.leaves());
                            let tb = expand(y.truth, y.leaves(), c.leaves());
                            let ta = if a.is_compl() { !ta } else { ta };
                            let tb = if b.is_compl() { !tb } else { tb };
                            c.truth = ta & tb;
                            set.retain(|d| !c.dominates(d));
                            set.push(c);
                        }
                    }
                    set.sort_by_key(|c| c.len);
                    set.truncate(self.opts.cuts_per_node);
                    set.push(trivial);
                    set
                }
            };
            self.cuts[n as usize] = Some(set);
        }
        self.cuts[n as usize].as_deref().expect("cuts computed")
    }

    /// Nodes `s` would add to the graph with `leaves` as its variables,
    /// counting nodes of the dereferenced cone as added, and the level of
    /// its output; `None` if it would exceed `max_added`.
    fn evaluate(
        &self,
        n: u32,
        s: &Structure,
        leaves: &[Lit; 4],
        max_added: u32,
    ) -> Option<(u32, u32)> {
        // Per node: the existing literal (if any) and the level.
        let mut lits: [(Option<Lit>, u32); 64] = [(None, 0); 64];
        lits[0] = (Some(Lit::FALSE), 0);
        for (v, l) in leaves.iter().enumerate() {
            lits[v + 1] = (Some(*l), self.e.level(l.node()));
        }
        let mut added = 0;
        for (j, &(x, y)) in s.gates.iter().enumerate() {
            let (lx, vx) = lits[(x >> 1) as usize];
            let (ly, vy) = lits[(y >> 1) as usize];
            let found = match (lx, ly) {
                (Some(a), Some(b)) => self.e.find_and(a.xor(x & 1 == 1), b.xor(y & 1 == 1)),
                _ => None,
            };
            let free =
                |l: Lit| !self.e.is_and(l.node()) || (self.e.refs(l.node()) > 0 && l.node() != n);
            lits[5 + j] = match found {
                Some(l) if free(l) => (Some(l), self.e.level(l.node())),
                f => {
                    added += 1;
                    if added > max_added {
                        return None;
                    }
                    (f, 1 + vx.max(vy))
                }
            };
        }
        Some((added, lits[(s.out >> 1) as usize].1))
    }

    fn build(&mut self, s: &Structure, leaves: &[Lit; 4]) -> Lit {
        let mut lits = [Lit::FALSE; 64];
        for (v, l) in leaves.iter().enumerate() {
            lits[v + 1] = *l;
        }
        for (j, &(x, y)) in s.gates.iter().enumerate() {
            let a = lits[(x >> 1) as usize].xor(x & 1 == 1);
            let b = lits[(y >> 1) as usize].xor(y & 1 == 1);
            lits[5 + j] = self.e.and(a, b);
        }
        lits[(s.out >> 1) as usize].xor(s.out & 1 == 1)
    }

    fn rewrite_node(&mut self, n: u32) -> bool {
        let cuts = self.cuts_of(n).to_vec();
        let level = self.e.level(n);
        // (gain, level, cut, structure index)
        let mut best: Option<(i64, u32, Cut, usize)> = None;
        for c in &cuts {
            if c.len == 1 && c.leaves[0] == n {
                continue;
            }
            if c.leaves().iter().any(|&l| self.e.is_dead(l)) {
                continue;
            }
            self.library.get(c.truth);
            let mut leaves = [Lit::FALSE; 4];
            for (j, &l) in c.leaves().iter().enumerate() {
                leaves[j] = Lit::new(l, false);
            }
            // The cone freed with the cut's leaves kept.
            for &l in c.leaves() {
                self.e.ref_leaf(l);
            }
            let mffc = self.e.deref_cone(n);
            let lib = &self.library.structures[&c.truth];
            for (k, s) in lib.iter().enumerate() {
                let min_gain = if self.opts.zero_gain { 0 } else { 1 };
                let Some(max_added) = (mffc as i64 - min_gain).try_into().ok() else { continue };
                let Some((added, lv)) = self.evaluate(n, s, &leaves, max_added) else {
                    continue;
                };
                if self.opts.preserve_levels && lv > level {
                    continue;
                }
                let gain = mffc as i64 - added as i64;
                if best.as_ref().is_none_or(|b| gain > b.0 || (gain == b.0 && lv < b.1)) {
                    best = Some((gain, lv, *c, k));
                }
            }
            self.e.ref_cone(n);
            for &l in c.leaves() {
                self.e.deref_leaf(l);
            }
        }
        let Some((_, _, c, k)) = best else { return false };
        let s = self.library.structures[&c.truth][k].clone();
        let mut leaves = [Lit::FALSE; 4];
        for (j, &l) in c.leaves().iter().enumerate() {
            leaves[j] = Lit::new(l, false);
        }
        let root = self.build(&s, &leaves);
        if root.node() == n {
            return false;
        }
        self.e.replace(n, root);
        true
    }
}

/// Rewrites `aig`; inputs and outputs keep their order.
pub fn rewrite(aig: &Aig, opts: &RewriteOptions) -> Aig {
    let e = EditAig::from_aig(aig);
    let n = e.len() as u32;
    let mut r = Rewriter { e, opts, cuts: Vec::new(), library: Library::default() };
    for i in 0..n {
        if r.e.is_and(i) && r.e.refs(i) > 0 {
            r.rewrite_node(i);
        }
    }
    r.e.to_aig()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aig::lit_val;

    fn same_function(a: &Aig, b: &Aig) {
        let mut rng = synth_core::Rng::new(3);
        for _ in 0..8 {
            let inputs: Vec<u64> = a.inputs.iter().map(|_| rng.next_u64()).collect();
            let (va, vb) = (a.simulate(&inputs), b.simulate(&inputs));
            for (oa, ob) in a.outputs.iter().zip(&b.outputs) {
                assert_eq!(lit_val(&va, *oa), lit_val(&vb, *ob));
            }
        }
    }

    /// Evaluates a structure on the variables' tables.
    fn eval(s: &Structure) -> u16 {
        let mut t = vec![0u16, VARS[0], VARS[1], VARS[2], VARS[3]];
        let lit = |t: &[u16], l: u8| {
            if l & 1 == 1 { !t[(l >> 1) as usize] } else { t[(l >> 1) as usize] }
        };
        for &(x, y) in &s.gates {
            let v = lit(&t, x) & lit(&t, y);
            t.push(v);
        }
        lit(&t, s.out)
    }

    #[test]
    fn structures_compute_their_function() {
        let mut rng = synth_core::Rng::new(11);
        for _ in 0..300 {
            let t = rng.next_u64() as u16;
            let ss = structures(t, &mut FxHashMap::default());
            assert!(!ss.is_empty());
            for s in &ss {
                assert_eq!(eval(s), t, "{t:04x}");
            }
        }
        // Parity of four variables: three XORs.
        let ss = structures(VARS[0] ^ VARS[1] ^ VARS[2] ^ VARS[3], &mut FxHashMap::default());
        assert_eq!(ss[0].gates.len(), 9);
        // (a ^ b) & (c ^ d): two XORs and an AND.
        let ss = structures((VARS[0] ^ VARS[1]) & (VARS[2] ^ VARS[3]), &mut FxHashMap::default());
        assert_eq!(ss[0].gates.len(), 7);
    }

    #[test]
    fn rewrites_redundant_logic() {
        // A mux built from redundant terms and an XOR built from ORs.
        let mut g = Aig::new();
        let x: Vec<Lit> = (0..4).map(|_| g.input()).collect();
        let t0 = g.and(x[0], x[1]);
        let t1 = g.and(!x[0], x[2]);
        let t2 = g.and(x[1], x[2]);
        let m = g.or_all(&[t0, t1, t2]);
        g.output(m);
        let o1 = g.or(x[2], x[3]);
        let o2 = g.and(x[2], x[3]);
        let xr = g.and(o1, !o2);
        let y = g.and(xr, x[0]);
        g.output(y);
        let r = rewrite(&g, &RewriteOptions::default());
        same_function(&g, &r);
        assert!(r.num_ands() < g.num_ands(), "{} vs {}", r.num_ands(), g.num_ands());
    }

    #[test]
    fn rewriting_random_graphs_preserves_function() {
        let mut rng = synth_core::Rng::new(1);
        for round in 0..20 {
            let mut g = Aig::new();
            let mut lits: Vec<Lit> = (0..8).map(|_| g.input()).collect();
            for _ in 0..60 {
                let a = lits[rng.below(lits.len())].xor(rng.next_u64() & 1 == 1);
                let b = lits[rng.below(lits.len())].xor(rng.next_u64() & 1 == 1);
                let l = g.and(a, b);
                lits.push(l);
            }
            for l in lits.iter().rev().take(6) {
                g.output(*l);
            }
            let opts = RewriteOptions { zero_gain: round % 2 == 1, ..Default::default() };
            let r = rewrite(&g, &opts);
            same_function(&g, &r);
            assert!(r.num_ands() <= g.cleanup().num_ands());
        }
    }
}
