//! Simplification of carry chains after the graph around them changed:
//! low bits whose operands became constant are folded back into the graph,
//! and high bits nobody reads are dropped.

use synth_core::FxHashMap;

use crate::aig::{Aig, Lit, Node};
use crate::net::{LogicNet, Source};

/// Returns a copy of `aig` in which each input in `subst` is replaced by a
/// literal of the original graph (inputs keep their positions and stay as
/// unused inputs). The substitutions must not form a cycle.
pub fn substitute_inputs(aig: &Aig, subst: &FxHashMap<Source, Lit>) -> Aig {
    let n = aig.nodes.len();
    let mut out = Aig::new();
    let mut map: Vec<Option<Lit>> = vec![None; n];
    map[0] = Some(Lit::FALSE);
    let mut by_node: FxHashMap<u32, Lit> = FxHashMap::default();
    for (k, &node) in aig.inputs.iter().enumerate() {
        let l = out.input();
        match subst.get(&(k as Source)) {
            Some(&s) => {
                by_node.insert(node, s);
            }
            None => map[node as usize] = Some(l),
        }
    }
    // Depth-first translation; a node is expanded once its operands are.
    let mut stack: Vec<(u32, bool)> = Vec::new();
    let mut translate = |root: u32, map: &mut Vec<Option<Lit>>, out: &mut Aig| {
        stack.push((root, false));
        while let Some((x, ready)) = stack.pop() {
            if map[x as usize].is_some() {
                continue;
            }
            let deps: Vec<Lit> = match (aig.nodes[x as usize], by_node.get(&x)) {
                (_, Some(&s)) => vec![s],
                (Node::And(a, b), None) => vec![a, b],
                _ => unreachable!("inputs and constants are mapped"),
            };
            if ready {
                let v =
                    |l: Lit| map[l.node() as usize].expect("operand translated").xor(l.is_compl());
                map[x as usize] = Some(match deps[..] {
                    [s] => v(s),
                    [a, b] => out.and(v(a), v(b)),
                    _ => unreachable!(),
                });
            } else {
                stack.push((x, true));
                for d in deps {
                    if map[d.node() as usize].is_none() {
                        stack.push((d.node(), false));
                    }
                }
            }
        }
    };
    for o in &aig.outputs {
        translate(o.node(), &mut map, &mut out);
    }
    out.outputs = aig
        .outputs
        .iter()
        .map(|o| map[o.node() as usize].expect("output translated").xor(o.is_compl()))
        .collect();
    out
}

/// Simplifies the carry chains of `net`. Returns the number of chain bits
/// removed.
pub fn simplify_carries(net: &mut LogicNet) -> usize {
    let mut used = vec![false; net.aig.nodes.len()];
    for nd in &net.aig.nodes {
        if let Node::And(a, b) = nd {
            used[a.node() as usize] = true;
            used[b.node() as usize] = true;
        }
    }
    for o in &net.aig.outputs {
        used[o.node() as usize] = true;
    }
    let src_used: Vec<bool> = net.aig.inputs.iter().map(|&n| used[n as usize]).collect();
    let src_used = |s: Source| src_used[s as usize];

    let mut subst: FxHashMap<Source, Lit> = FxHashMap::default();
    let mut removed = 0;
    let mut chains = std::mem::take(&mut net.carries);
    for ch in &mut chains {
        let w = ch.a.len();
        // High bits: drop those whose sums are unread when the carry out is.
        if ch.cout.is_none_or(|c| !src_used(c)) {
            ch.cout = None;
            let mut top = w;
            while top > 0 && ch.sum.get(top - 1).is_none_or(|&s| !src_used(s)) {
                top -= 1;
            }
            if top < w {
                removed += w - top;
                ch.a.truncate(top);
                ch.b.truncate(top);
                ch.sum.truncate(top.min(ch.sum.len()));
            }
        }
        // Low bits: while the carry into a bit is constant and one operand
        // is too, the sum is the other operand (possibly inverted).
        let w = ch.a.len();
        let outs = &net.aig.outputs;
        let lit = |k: usize| outs[k];
        let mut carry = lit(ch.cin);
        let mut lo = 0;
        while lo < w && carry.is_const() {
            let (a, b) = (lit(ch.a[lo]), lit(ch.b[lo]));
            let (k, x) = if a.is_const() {
                (a, b)
            } else if b.is_const() {
                (b, a)
            } else {
                break;
            };
            if let Some(&s) = ch.sum.get(lo) {
                subst.insert(s, x.xor(k == Lit::TRUE).xor(carry == Lit::TRUE));
            }
            // maj(k, x, c): k when k == c, otherwise x.
            carry = if k == carry { k } else { x };
            lo += 1;
        }
        if lo > 0 {
            removed += lo;
            ch.a.drain(..lo);
            ch.b.drain(..lo);
            ch.sum.drain(..lo.min(ch.sum.len()));
            ch.cin = net.aig.output(carry);
            if ch.a.is_empty()
                && let Some(c) = ch.cout.take()
            {
                subst.insert(c, carry);
            }
        }
    }
    chains.retain(|c| !c.a.is_empty());
    net.carries = chains;
    if !subst.is_empty() {
        net.aig = substitute_inputs(&net.aig, &subst);
    }
    removed
}

/// Share identical arithmetic boxes before their unused sum/carry outputs
/// are trimmed. A subtract and its comparison can then use the same chain.
/// Sorting each operand pair also catches commuted additions.
pub fn merge_duplicate_carries(net: &mut LogicNet) -> usize {
    let mut seen = FxHashMap::default();
    let mut chains: Vec<crate::net::Carry> = Vec::new();
    let mut subst = FxHashMap::default();
    let mut removed = 0;
    for ch in std::mem::take(&mut net.carries) {
        let operands: Vec<_> =
            ch.a.iter()
                .zip(&ch.b)
                .map(|(&a, &b)| {
                    let (a, b) = (net.aig.outputs[a], net.aig.outputs[b]);
                    (a.min(b), a.max(b))
                })
                .collect();
        let key = (operands, net.aig.outputs[ch.cin]);
        if let Some(&index) = seen.get(&key) {
            let existing: &mut crate::net::Carry = &mut chains[index];
            for (&old, &new) in ch.sum.iter().zip(&existing.sum) {
                subst.insert(old, Lit::new(net.aig.inputs[new as usize], false));
            }
            // A comparison has no sum; retain the sum of the other chain.
            if ch.sum.len() > existing.sum.len() {
                existing.sum.extend_from_slice(&ch.sum[existing.sum.len()..]);
            }
            match (ch.cout, existing.cout) {
                (Some(old), Some(new)) => {
                    subst.insert(old, Lit::new(net.aig.inputs[new as usize], false));
                }
                (Some(new), None) => existing.cout = Some(new),
                _ => {}
            }
            removed += ch.a.len();
        } else {
            seen.insert(key, chains.len());
            chains.push(ch);
        }
    }
    net.carries = chains;
    if !subst.is_empty() {
        net.aig = substitute_inputs(&net.aig, &subst);
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::Carry;

    #[test]
    fn shares_commuted_operands_but_not_different_carry_inputs() {
        let mut net = LogicNet::default();
        let a: Vec<_> = (0..4).map(|_| net.aig.input()).collect();
        let b: Vec<_> = (0..4).map(|_| net.aig.input()).collect();
        let cin = net.aig.input();
        let ca: Vec<_> = a.iter().map(|&a| net.aig.output(a)).collect();
        let cb: Vec<_> = b.iter().map(|&b| net.aig.output(b)).collect();
        let zero = net.aig.output(Lit::FALSE);
        let cin = net.aig.output(cin);
        let mut watched = Vec::new();
        for i in 0..3 {
            let cout = net.aig.inputs.len() as u32;
            let out = net.aig.input();
            watched.push(net.aig.output(out));
            // Comparison comes first; the second chain supplies its sum.
            let mut sum = Vec::new();
            if i == 1 {
                for _ in 0..4 {
                    sum.push(net.aig.inputs.len() as u32);
                    net.aig.input();
                }
            }
            net.carries.push(Carry {
                a: if i == 1 { cb.clone() } else { ca.clone() },
                b: if i == 1 { ca.clone() } else { cb.clone() },
                cin: if i == 2 { cin } else { zero },
                sum,
                cout: Some(cout),
            });
        }
        assert_eq!(merge_duplicate_carries(&mut net), 4);
        assert_eq!(net.carries.len(), 2);
        assert_eq!(net.carries[0].sum.len(), 4);
        assert_eq!(net.aig.outputs[watched[0]], net.aig.outputs[watched[1]]);
        assert_ne!(net.aig.outputs[watched[0]], net.aig.outputs[watched[2]]);
    }

    #[test]
    fn folds_constant_low_bits_and_identity_chains() {
        let mut net = LogicNet::default();
        let g = &mut net.aig;
        let x: Vec<Lit> = (0..4).map(|_| g.input()).collect();
        // Chain 0: 0 + x (identity). Chain 1: {x3, 0} + {x2, 1} + 0.
        let s0: Vec<Source> = (0..4).map(|_| g.input().node() - 1).collect();
        let s1: Vec<Source> = (0..2).map(|_| g.input().node() - 1).collect();
        let z = g.output(Lit::FALSE);
        let xa: Vec<usize> = x.iter().map(|&l| g.output(l)).collect();
        let one = g.output(Lit::TRUE);
        let ca = vec![z, xa[3]];
        let cb = vec![one, xa[2]];
        net.carries.push(Carry {
            a: vec![z; 4],
            b: xa.clone(),
            cin: z,
            sum: s0.clone(),
            cout: None,
        });
        net.carries.push(Carry { a: ca, b: cb, cin: z, sum: s1.clone(), cout: None });
        let g = &mut net.aig;
        let o0 = g.inputs[s0[2] as usize];
        let t = g.and(Lit::new(o0, false), x[0]);
        let ot = g.output(t);
        let o1 = g.inputs[s1[0] as usize];
        let o2 = g.inputs[s1[1] as usize];
        let os = g.output(Lit::new(o1, false));
        g.output(Lit::new(o2, false));
        let removed = simplify_carries(&mut net);
        // Chain 0 vanishes; chain 1 loses its low bit (sum = 1) and keeps
        // x3 + x2 with carry-in 0.
        assert_eq!(net.carries.len(), 1);
        assert_eq!(net.carries[0].a.len(), 1);
        assert!(removed >= 5);
        let g = &net.aig;
        assert_eq!(g.outputs[os], Lit::TRUE);
        let and = g.outputs[ot];
        assert_eq!(g.fanins(and.node()), Some((x[0], x[2])));
    }
}
