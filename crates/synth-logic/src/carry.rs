//! Simplification of carry chains after the graph around them changed:
//! low bits whose operands became constant are folded back into the graph,
//! and high bits nobody reads are dropped. Boxes whose boundary inputs fit
//! in a target LUT can be lowered back into combinational logic.

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

/// Lower carry boxes whose entire input support fits in a LUT. Every
/// sum and carry output then needs at most one LUT, with no dedicated
/// carry entry/exit cells or placement constraints. `k` is the target LUT
/// input count, not an arithmetic width threshold.
pub fn lower_lut_sized_carries(net: &mut LogicNet, k: usize) -> usize {
    let mut subst = FxHashMap::default();
    let mut kept = Vec::new();
    let mut lowered = 0;
    for ch in std::mem::take(&mut net.carries) {
        let mut support = Vec::new();
        for &s in ch.a.iter().chain(&ch.b).chain(std::iter::once(&ch.cin)) {
            let l = net.aig.outputs[s];
            if !l.is_const() && !support.contains(&l.node()) {
                support.push(l.node());
                if support.len() > k {
                    break;
                }
            }
        }
        if support.len() > k {
            kept.push(ch);
            continue;
        }
        let g = &mut net.aig;
        let mut c = g.outputs[ch.cin];
        for i in 0..ch.a.len() {
            let (a, b) = (g.outputs[ch.a[i]], g.outputs[ch.b[i]]);
            if let Some(&s) = ch.sum.get(i) {
                let x = g.xor(a, b);
                let sum = g.xor(x, c);
                subst.insert(s, sum);
            }
            c = g.maj(a, b, c);
        }
        if let Some(s) = ch.cout {
            subst.insert(s, c);
        }
        lowered += 1;
    }
    net.carries = kept;
    if !subst.is_empty() {
        net.aig = substitute_inputs(&net.aig, &subst);
    }
    lowered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::Carry;

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

    #[test]
    fn lut_sized_carries_preserve_sums_cout_and_polarities() {
        use crate::aig::lit_val;
        // Two independent four-bit operands stay on carry hardware. A
        // constant, repeated, or complemented operand fits in one LUT's
        // boundary support. Check every assignment and both carry-ins.
        for mode in 0..4 {
            for cin in [false, true] {
                let mut net = LogicNet::default();
                let g = &mut net.aig;
                let x: Vec<_> = (0..8).map(|_| g.input()).collect();
                let s: Vec<_> = (0..5).map(|_| g.input()).collect();
                let a = x[..4].iter().map(|&l| g.output(l)).collect();
                let b = (0..4)
                    .map(|i| {
                        g.output(match mode {
                            0 => x[4 + i],
                            1 => Lit::FALSE.xor(5 >> i & 1 != 0),
                            2 => x[i],
                            _ => !x[i],
                        })
                    })
                    .collect();
                let ci = g.output(Lit::FALSE.xor(cin));
                let sources: Vec<_> = s
                    .iter()
                    .map(|l| match g.nodes[l.node() as usize] {
                        Node::Input(s) => s,
                        _ => unreachable!(),
                    })
                    .collect();
                net.carries.push(Carry {
                    a,
                    b,
                    cin: ci,
                    sum: sources[..4].to_vec(),
                    cout: Some(sources[4]),
                });
                let outputs: Vec<_> = s.iter().map(|&l| g.output(l)).collect();
                let changed = lower_lut_sized_carries(&mut net, 4);
                assert_eq!(changed, usize::from(mode != 0));
                if changed == 0 {
                    continue;
                }
                assert!(net.carries.is_empty());
                for a in 0..16u64 {
                    let mut inputs = vec![0; net.aig.inputs.len()];
                    for (i, v) in inputs[..4].iter_mut().enumerate() {
                        *v = (a >> i) & 1;
                    }
                    let values = net.aig.simulate(&inputs);
                    let got = outputs.iter().enumerate().fold(0, |v, (i, &o)| {
                        v | ((lit_val(&values, net.aig.outputs[o]) & 1) << i)
                    });
                    let b = match mode {
                        1 => 5,
                        2 => a,
                        _ => a ^ 15,
                    };
                    assert_eq!(got, a + b + u64::from(cin));
                }
            }
        }
    }

    #[test]
    fn lut_sized_carry_in_and_carry_only_outputs() {
        use crate::aig::lit_val;
        for width in 2..=4 {
            for want_sum in [false, true] {
                let mut net = LogicNet::default();
                let g = &mut net.aig;
                let x: Vec<_> = (0..=width).map(|_| g.input()).collect();
                let result: Vec<_> = (0..=width).map(|_| g.input()).collect();
                let a = x[..width].iter().map(|&l| g.output(l)).collect();
                let b = (0..width).map(|i| g.output(Lit::FALSE.xor(5 >> i & 1 != 0))).collect();
                let cin = g.output(x[width]);
                let sources: Vec<_> = result
                    .iter()
                    .map(|l| match g.nodes[l.node() as usize] {
                        Node::Input(s) => s,
                        _ => unreachable!(),
                    })
                    .collect();
                net.carries.push(Carry {
                    a,
                    b,
                    cin,
                    sum: if want_sum { sources[..width].to_vec() } else { Vec::new() },
                    cout: Some(sources[width]),
                });
                let visible = if want_sum { &result[..] } else { &result[width..] };
                let outputs: Vec<_> = visible.iter().map(|&l| g.output(l)).collect();
                let lowered = lower_lut_sized_carries(&mut net, 4);
                assert_eq!(lowered, usize::from(width < 4));
                if lowered == 0 {
                    continue;
                }
                for input in 0..1u64 << (width + 1) {
                    let mut values = vec![0; net.aig.inputs.len()];
                    for (i, v) in values[..=width].iter_mut().enumerate() {
                        *v = input >> i & 1;
                    }
                    let values = net.aig.simulate(&values);
                    let got = outputs.iter().enumerate().fold(0, |v, (i, &o)| {
                        v | ((lit_val(&values, net.aig.outputs[o]) & 1) << i)
                    });
                    let mask = (1 << width) - 1;
                    let expected = (input & mask) + (5 & mask) + (input >> width);
                    assert_eq!(got, if want_sum { expected } else { expected >> width });
                }
            }
        }
    }
}
