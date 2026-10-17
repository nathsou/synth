//! Technology-independent AIG optimisations.

use crate::aig::{Aig, Lit, Node};

/// Rebuilds every multi-input AND (a tree of single-fanout,
/// non-complemented AND nodes) as a tree of minimum depth, pairing the
/// shallowest operands first and preferring pairs that already exist.
/// Inputs and outputs keep their order.
pub fn balance(aig: &Aig) -> Aig {
    let fanout = aig.fanout_counts();
    // A node is the root of its supergate unless its only reference is a
    // non-complemented edge from another AND node.
    let mut absorbed = vec![false; aig.nodes.len()];
    let mut compl_ref = vec![false; aig.nodes.len()];
    for n in &aig.nodes {
        if let Node::And(a, b) = *n {
            for f in [a, b] {
                compl_ref[f.node() as usize] |= f.is_compl();
            }
        }
    }
    for o in &aig.outputs {
        compl_ref[o.node() as usize] = true;
    }
    for (i, n) in aig.nodes.iter().enumerate() {
        absorbed[i] = matches!(n, Node::And(..)) && fanout[i] == 1 && !compl_ref[i];
    }

    // Supergate leaves of each root, as literal ids: old literals below
    // `base`, extracted divisors from `base` up.
    let base = 2 * aig.nodes.len() as u32;
    let mut sgs: Vec<(u32, Vec<u32>)> = Vec::new();
    let mut stack = Vec::new();
    for i in 0..aig.nodes.len() {
        if absorbed[i] || !matches!(aig.nodes[i], Node::And(..)) {
            continue;
        }
        let mut leaves = Vec::new();
        stack.push(i as u32);
        while let Some(n) = stack.pop() {
            let (a, b) = aig.fanins(n).expect("supergate nodes are AND nodes");
            for f in [a, b] {
                if !f.is_compl() && absorbed[f.node() as usize] {
                    stack.push(f.node());
                } else {
                    leaves.push(f.0);
                }
            }
        }
        leaves.sort_unstable();
        leaves.dedup();
        sgs.push((i as u32, leaves));
    }
    let divisors = extract_divisors(&mut sgs, base);

    let mut out = Aig::new();
    let mut level: Vec<u32> = vec![0];
    let mut map = vec![Lit::FALSE; aig.nodes.len()];
    for &i in &aig.inputs {
        map[i as usize] = out.input();
        level.push(0);
    }
    let mut built: Vec<Option<Lit>> = vec![None; divisors.len()];
    let mut leaves = Vec::new();
    for (root, ids) in &sgs {
        leaves.clear();
        for &id in ids {
            let l = resolve(id, base, &divisors, &mut built, &map, &mut out, &mut level);
            leaves.push(l);
        }
        map[*root as usize] = build_and(&mut out, &mut level, &mut leaves);
    }
    out.outputs = aig.outputs.iter().map(|o| map[o.node() as usize].xor(o.is_compl())).collect();
    out
}

/// The new-graph literal of a leaf id (an old literal or a divisor).
fn resolve(
    id: u32,
    base: u32,
    divisors: &[(u32, u32)],
    built: &mut [Option<Lit>],
    map: &[Lit],
    out: &mut Aig,
    level: &mut Vec<u32>,
) -> Lit {
    if id < base {
        let l = Lit(id);
        return map[l.node() as usize].xor(l.is_compl());
    }
    let d = (id - base) as usize;
    if let Some(l) = built[d] {
        return l;
    }
    let (x, y) = divisors[d];
    let a = resolve(x, base, divisors, built, map, out, level);
    let b = resolve(y, base, divisors, built, map, out, level);
    let l = and_leveled(out, level, a, b);
    built[d] = Some(l);
    l
}

/// Supergates larger than this do not take part in divisor extraction.
const MAX_DIVISOR_SG: usize = 48;

/// Greedily extracts the two-literal ANDs common to most supergates,
/// replacing them in the supergates by new ids from `base` up. Returns the
/// divisors' operands, indexed by `id - base`.
fn extract_divisors(sgs: &mut [(u32, Vec<u32>)], base: u32) -> Vec<(u32, u32)> {
    use std::collections::BinaryHeap;
    use synth_core::FxHashMap;
    let key = |a: u32, b: u32| if a < b { (a, b) } else { (b, a) };
    let mut count: FxHashMap<(u32, u32), u32> = FxHashMap::default();
    let mut occ: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
    for (s, (_, l)) in sgs.iter().enumerate() {
        if l.len() < 2 || l.len() > MAX_DIVISOR_SG {
            continue;
        }
        for (k, &a) in l.iter().enumerate() {
            occ.entry(a).or_default().push(s as u32);
            for &b in &l[k + 1..] {
                *count.entry(key(a, b)).or_default() += 1;
            }
        }
    }
    let mut heap: BinaryHeap<(u32, std::cmp::Reverse<(u32, u32)>)> =
        count.iter().filter(|&(_, &c)| c >= 2).map(|(&p, &c)| (c, std::cmp::Reverse(p))).collect();
    let mut divisors = Vec::new();
    while let Some((c, std::cmp::Reverse(p))) = heap.pop() {
        if count.get(&p) != Some(&c) {
            continue;
        }
        let d = base + divisors.len() as u32;
        divisors.push(p);
        count.remove(&p);
        let users = occ.get(&p.0).cloned().unwrap_or_default();
        for s in users {
            let l = &mut sgs[s as usize].1;
            let (Some(i0), Some(i1)) =
                (l.iter().position(|&x| x == p.0), l.iter().position(|&x| x == p.1))
            else {
                continue;
            };
            l.swap_remove(i0.max(i1));
            l.swap_remove(i0.min(i1));
            for &x in l.iter() {
                for y in [p.0, p.1] {
                    let k = key(x, y);
                    if let Some(c) = count.get_mut(&k) {
                        *c -= 1;
                        if *c >= 2 {
                            heap.push((*c, std::cmp::Reverse(k)));
                        }
                    }
                }
                let k = key(x, d);
                let c = count.entry(k).or_default();
                *c += 1;
                if *c >= 2 {
                    heap.push((*c, std::cmp::Reverse(k)));
                }
            }
            if !l.is_empty() {
                occ.entry(d).or_default().push(s);
            }
            l.push(d);
        }
    }
    divisors
}

fn and_leveled(out: &mut Aig, level: &mut Vec<u32>, a: Lit, b: Lit) -> Lit {
    let l = out.and(a, b);
    if l.node() as usize >= level.len() {
        let lv = 1 + level[a.node() as usize].max(level[b.node() as usize]);
        level.resize(l.node() as usize + 1, 0);
        level[l.node() as usize] = lv;
    }
    l
}

/// The AND of `leaves` as a tree of minimum depth.
fn build_and(out: &mut Aig, level: &mut Vec<u32>, leaves: &mut Vec<Lit>) -> Lit {
    leaves.sort_unstable();
    leaves.dedup();
    if leaves.contains(&Lit::FALSE) || leaves.windows(2).any(|w| w[0] == !w[1]) {
        return Lit::FALSE;
    }
    leaves.retain(|&l| l != Lit::TRUE);
    if leaves.is_empty() {
        return Lit::TRUE;
    }
    // Deepest first; the two shallowest are combined at the end.
    leaves.sort_by_key(|l| std::cmp::Reverse(level[l.node() as usize]));
    while leaves.len() > 1 {
        let n = leaves.len();
        let a = leaves[n - 1];
        // Among the operands as shallow as the second shallowest, prefer
        // one whose AND with `a` already exists.
        let lv = level[leaves[n - 2].node() as usize];
        let mut k = n - 2;
        let mut j = n - 2;
        loop {
            if out.find_and(a, leaves[j]).is_some() {
                k = j;
                break;
            }
            if j == 0 || level[leaves[j - 1].node() as usize] != lv {
                break;
            }
            j -= 1;
        }
        let b = leaves.remove(k);
        leaves.pop();
        let x = and_leveled(out, level, a, b);
        let lx = level[x.node() as usize];
        // Keep the list sorted by decreasing level.
        let pos = leaves.partition_point(|l| level[l.node() as usize] > lx);
        leaves.insert(pos, x);
    }
    leaves[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balances_and_chain() {
        let mut g = Aig::new();
        let x: Vec<Lit> = (0..8).map(|_| g.input()).collect();
        let mut acc = x[0];
        for &l in &x[1..] {
            acc = g.and(acc, l);
        }
        g.output(acc);
        g.output(!acc);
        let b = balance(&g);
        assert_eq!(b.num_ands(), 7);
        assert_eq!(*b.levels().iter().max().unwrap(), 3);
        let inputs: Vec<u64> =
            (0..8).map(|i| 0x0123_4567_89ab_cdefu64.rotate_left(i * 7)).collect();
        let (s0, s1) = (g.simulate(&inputs), b.simulate(&inputs));
        for (o0, o1) in g.outputs.iter().zip(&b.outputs) {
            assert_eq!(crate::aig::lit_val(&s0, *o0), crate::aig::lit_val(&s1, *o1));
        }
    }
}
