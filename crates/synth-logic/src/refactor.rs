//! Cone refactoring: each node's function over a small reconvergent cut is
//! re-expressed as a factored sum of products (from an irredundant cover)
//! and replaces the cone when that needs fewer AND nodes than the logic it
//! frees (after "refactor" in ABC).

use crate::aig::{Aig, Lit, Node};

/// Truth tables of the 6 variables over 64 minterms.
const VARS: [u64; 6] = [
    0xAAAA_AAAA_AAAA_AAAA,
    0xCCCC_CCCC_CCCC_CCCC,
    0xF0F0_F0F0_F0F0_F0F0,
    0xFF00_FF00_FF00_FF00,
    0xFFFF_0000_FFFF_0000,
    0xFFFF_FFFF_0000_0000,
];

#[derive(Clone, Debug)]
pub struct RefactorOptions {
    /// Cut size (at most 6).
    pub max_leaves: usize,
    /// Also accept replacements that do not reduce the node count (they
    /// change the structure, which helps later passes and choices).
    pub zero_gain: bool,
}

impl Default for RefactorOptions {
    fn default() -> Self {
        RefactorOptions { max_leaves: 6, zero_gain: false }
    }
}

/// A product term: variables that must be 1 (`pos`) and 0 (`neg`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Cube {
    pos: u8,
    neg: u8,
}

impl Cube {
    fn lits(&self) -> u32 {
        self.pos.count_ones() + self.neg.count_ones()
    }
}

fn cof0(t: u64, v: usize) -> u64 {
    let x = t & !VARS[v];
    x | (x << (1 << v))
}

fn cof1(t: u64, v: usize) -> u64 {
    let x = t & VARS[v];
    x | (x >> (1 << v))
}

/// Minato-Morreale irredundant sum of products of a function between `on`
/// and `on | dc` (`ondc`); returns the cover and the function it computes.
fn isop(on: u64, ondc: u64, nvars: usize, out: &mut Vec<Cube>) -> u64 {
    if on == 0 {
        return 0;
    }
    if ondc == !0 {
        out.push(Cube { pos: 0, neg: 0 });
        return !0;
    }
    // Highest variable either function depends on.
    let v = (0..nvars)
        .rev()
        .find(|&v| cof0(on, v) != cof1(on, v) || cof0(ondc, v) != cof1(ondc, v))
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

/// A factored form, built into a graph.
fn factor(g: &mut Aig, cubes: &[Cube], vars: &[Lit]) -> Lit {
    if cubes.is_empty() {
        return Lit::FALSE;
    }
    if cubes.len() == 1 {
        let c = cubes[0];
        let lits: Vec<Lit> = (0..vars.len())
            .filter_map(|v| {
                if c.pos >> v & 1 == 1 {
                    Some(vars[v])
                } else if c.neg >> v & 1 == 1 {
                    Some(!vars[v])
                } else {
                    None
                }
            })
            .collect();
        return g.and_all(&lits);
    }
    // The literal shared by most cubes.
    let mut best = (0, 0usize, false);
    for v in 0..vars.len() {
        for neg in [false, true] {
            let n =
                cubes.iter().filter(|c| (if neg { c.neg } else { c.pos }) >> v & 1 == 1).count();
            if n > best.0 {
                best = (n, v, neg);
            }
        }
    }
    if best.0 < 2 {
        let terms: Vec<Lit> = cubes.iter().map(|c| factor(g, &[*c], vars)).collect();
        return g.or_all(&terms);
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
    let q = factor(g, &quotient, vars);
    let lit = if neg { !vars[v] } else { vars[v] };
    let t = g.and(lit, q);
    if rest.is_empty() {
        return t;
    }
    let r = factor(g, &rest, vars);
    g.or(t, r)
}

/// Builds the function with table `t` over `vars` (at most six) into `g` as
/// the factored irredundant cover of the cheaper polarity.
pub fn sop(g: &mut Aig, t: u64, vars: &[Lit]) -> Lit {
    let n = vars.len();
    // Replicate the table over the unused variables.
    let mut full = 0u64;
    for i in 0..64 {
        full |= ((t >> (i & ((1 << n) - 1))) & 1) << i;
    }
    let mut best: Option<(Vec<Cube>, bool)> = None;
    for compl in [false, true] {
        let f = if compl { !full } else { full };
        let mut cover = Vec::new();
        isop(f, f, n, &mut cover);
        let lits: u32 = cover.iter().map(Cube::lits).sum();
        if best.as_ref().is_none_or(|(c, _)| lits < c.iter().map(Cube::lits).sum()) {
            best = Some((cover, compl));
        }
    }
    let (cubes, compl) = best.expect("two polarities tried");
    factor(g, &cubes, vars).xor(compl)
}

/// Grows a reconvergence-driven cut of `root`: repeatedly expands the leaf
/// whose fanins add the fewest new leaves.
fn find_cut(g: &Aig, root: u32, max: usize) -> Vec<u32> {
    let (a, b) = g.fanins(root).expect("cut of an AND node");
    let mut leaves = vec![a.node(), b.node()];
    leaves.dedup();
    let mut inner = vec![root];
    loop {
        let mut best: Option<(usize, i32)> = None;
        for (k, &l) in leaves.iter().enumerate() {
            let Some((x, y)) = g.fanins(l) else { continue };
            let cost = [x.node(), y.node()]
                .iter()
                .filter(|&&f| !leaves.contains(&f) && !inner.contains(&f))
                .count() as i32
                - 1;
            if best.is_none_or(|(_, c)| cost < c) {
                best = Some((k, cost));
            }
        }
        let Some((k, cost)) = best else { break };
        if leaves.len() as i32 + cost > max as i32 {
            break;
        }
        let l = leaves.swap_remove(k);
        inner.push(l);
        let (x, y) = g.fanins(l).unwrap();
        for f in [x.node(), y.node()] {
            if !leaves.contains(&f) && !inner.contains(&f) {
                leaves.push(f);
            }
        }
    }
    leaves.sort_unstable();
    leaves
}

/// Truth table of `root` over `leaves`, and the nodes strictly inside.
fn cone(g: &Aig, root: u32, leaves: &[u32]) -> (u64, Vec<u32>) {
    let mut val: synth_core::FxHashMap<u32, u64> = Default::default();
    val.insert(0, 0);
    for (j, &l) in leaves.iter().enumerate() {
        val.insert(l, VARS[j]);
    }
    let mut inner = Vec::new();
    let mut stack = vec![(root, false)];
    while let Some((n, ready)) = stack.pop() {
        if val.contains_key(&n) {
            continue;
        }
        let (a, b) = g.fanins(n).expect("cone stays above its leaves");
        if ready {
            let v = |l: Lit| val[&l.node()] ^ if l.is_compl() { !0 } else { 0 };
            let r = v(a) & v(b);
            val.insert(n, r);
            inner.push(n);
        } else {
            stack.push((n, true));
            stack.push((a.node(), false));
            stack.push((b.node(), false));
        }
    }
    (val[&root], inner)
}

/// Number of nodes of `root`'s cone (inside `inner`) that only `root`
/// uses, given reference counts.
fn mffc_size(g: &Aig, root: u32, inner: &[u32], refs: &mut [u32]) -> usize {
    let mut freed = vec![root];
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        let (a, b) = g.fanins(n).unwrap();
        for f in [a.node(), b.node()] {
            if inner.contains(&f) {
                refs[f as usize] -= 1;
                if refs[f as usize] == 0 {
                    freed.push(f);
                    stack.push(f);
                }
            }
        }
    }
    // Restore the counts.
    for &n in &freed {
        let (a, b) = g.fanins(n).unwrap();
        for f in [a.node(), b.node()] {
            if inner.contains(&f) {
                refs[f as usize] += 1;
            }
        }
    }
    freed.len()
}

/// Refactors `aig`; inputs and outputs keep their order.
pub fn refactor(aig: &Aig, opts: &RefactorOptions) -> Aig {
    let mut refs = aig.fanout_counts();
    let mut out = Aig::new();
    let mut map = vec![Lit::FALSE; aig.nodes.len()];
    for &i in &aig.inputs {
        map[i as usize] = out.input();
    }
    for i in 0..aig.nodes.len() {
        let Node::And(a, b) = aig.nodes[i] else { continue };
        let fa = map[a.node() as usize].xor(a.is_compl());
        let fb = map[b.node() as usize].xor(b.is_compl());
        let plain = out.and(fa, fb);
        map[i] = plain;
        if refs[i] == 0 {
            continue;
        }
        let leaves = find_cut(aig, i as u32, opts.max_leaves.min(6));
        if leaves.len() < 3 {
            continue;
        }
        let (t, inner) = cone(aig, i as u32, &leaves);
        let saved = mffc_size(aig, i as u32, &inner, &mut refs);
        if saved < 2 {
            continue;
        }
        let vars: Vec<Lit> = leaves.iter().map(|&l| map[l as usize]).collect();
        // Build it and keep it if it adds fewer nodes than the cone frees.
        let before = out.nodes.len();
        let alt = sop(&mut out, t, &vars);
        let gain = saved as i64 - (out.nodes.len() - before) as i64;
        if gain > 0 || (opts.zero_gain && gain == 0 && alt != plain) {
            map[i] = alt;
        }
    }
    out.outputs = aig.outputs.iter().map(|o| map[o.node() as usize].xor(o.is_compl())).collect();
    out.cleanup()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same_function(a: &Aig, b: &Aig) {
        let mut rng = synth_core::Rng::new(5);
        for _ in 0..8 {
            let inputs: Vec<u64> = a.inputs.iter().map(|_| rng.next_u64()).collect();
            let (va, vb) = (a.simulate(&inputs), b.simulate(&inputs));
            for (oa, ob) in a.outputs.iter().zip(&b.outputs) {
                assert_eq!(crate::aig::lit_val(&va, *oa), crate::aig::lit_val(&vb, *ob));
            }
        }
    }

    #[test]
    fn isop_covers_exactly() {
        let mut rng = synth_core::Rng::new(9);
        for n in 1..=6 {
            for _ in 0..50 {
                let mask = if n == 6 { !0 } else { (1u64 << (1 << n)) - 1 };
                let mut t = rng.next_u64() & mask;
                // Replicate to a full 64-bit table.
                let mut w = 1 << n;
                while w < 64 {
                    t |= t << w;
                    w *= 2;
                }
                let mut cover = Vec::new();
                let f = isop(t, t, n, &mut cover);
                assert_eq!(f, t);
                let mut g = 0u64;
                for c in &cover {
                    let mut m = !0u64;
                    for (v, var) in VARS.iter().enumerate().take(n) {
                        if c.pos >> v & 1 == 1 {
                            m &= var;
                        }
                        if c.neg >> v & 1 == 1 {
                            m &= !var;
                        }
                    }
                    g |= m;
                }
                assert_eq!(g, t);
            }
        }
    }

    #[test]
    fn refactors_redundant_logic() {
        // (a & b) | (a & c) | (a & d) built naively: a & (b | c | d) is
        // smaller.
        let mut g = Aig::new();
        let x: Vec<Lit> = (0..4).map(|_| g.input()).collect();
        let t: Vec<Lit> = (1..4).map(|k| g.and(x[0], x[k])).collect();
        let y = g.or_all(&t);
        g.output(y);
        let r = refactor(&g, &RefactorOptions::default());
        same_function(&g, &r);
        assert!(r.num_ands() < g.num_ands(), "{} vs {}", r.num_ands(), g.num_ands());
    }
}
