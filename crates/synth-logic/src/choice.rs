//! Structural choices: several implementations of the same logic, kept
//! side by side so that the mapper can pick the best structure locally
//! (after Chatterjee, Mishchenko et al., "Reducing structural bias in
//! technology mapping", ICCAD 2005).
//!
//! Snapshots of the graph taken at different optimisation steps are merged
//! into one graph; nodes proved equivalent form classes. In the resulting
//! graph every fanin points at its class's representative (the earliest
//! node), and the other members hang off the representative as choices.

use crate::aig::{Aig, Lit, Node};
use crate::fraig::{FraigOptions, equivalences};

/// A graph with choices. Members are not used by any fanin or output; they
/// are reached only through `choices`.
#[derive(Clone, Debug)]
pub struct ChoiceAig {
    pub aig: Aig,
    /// Members of each representative's class, with their phase relative
    /// to it.
    pub choices: Vec<Vec<(u32, bool)>>,
}

impl ChoiceAig {
    /// A graph without choices.
    pub fn plain(aig: Aig) -> ChoiceAig {
        let n = aig.nodes.len();
        ChoiceAig { aig, choices: vec![Vec::new(); n] }
    }

    pub fn num_choices(&self) -> usize {
        self.choices.iter().map(Vec::len).sum()
    }

    /// Nodes in an order where fanins and choice members precede the nodes
    /// using them.
    pub fn order(&self) -> Vec<u32> {
        let n = self.aig.nodes.len();
        let mut done = vec![false; n];
        let mut order = Vec::with_capacity(n);
        let mut stack: Vec<(u32, bool)> = Vec::new();
        for root in 0..n as u32 {
            if done[root as usize] {
                continue;
            }
            stack.push((root, false));
            while let Some((x, expanded)) = stack.pop() {
                if done[x as usize] {
                    continue;
                }
                if expanded {
                    done[x as usize] = true;
                    order.push(x);
                    continue;
                }
                stack.push((x, true));
                for &(m, _) in &self.choices[x as usize] {
                    if !done[m as usize] {
                        stack.push((m, false));
                    }
                }
                if let Node::And(a, b) = self.aig.nodes[x as usize] {
                    for f in [a, b] {
                        if !done[f.node() as usize] {
                            stack.push((f.node(), false));
                        }
                    }
                }
            }
        }
        order
    }
}

/// Merges `snapshots` (functionally equivalent graphs with the same inputs
/// and outputs) into a graph with choices. Outputs come from the first.
pub fn build(snapshots: &[&Aig], opts: &FraigOptions) -> ChoiceAig {
    let first = snapshots[0];
    // All snapshots in one graph, sharing inputs and identical structure.
    let mut comb = Aig::new();
    let inputs: Vec<Lit> = first.inputs.iter().map(|_| comb.input()).collect();
    let mut outputs = Vec::new();
    for (k, g) in snapshots.iter().enumerate() {
        let mut map = vec![Lit::FALSE; g.nodes.len()];
        for (j, &n) in g.inputs.iter().enumerate() {
            map[n as usize] = inputs[j];
        }
        for (i, nd) in g.nodes.iter().enumerate() {
            if let Node::And(a, b) = *nd {
                let fa = map[a.node() as usize].xor(a.is_compl());
                let fb = map[b.node() as usize].xor(b.is_compl());
                map[i] = comb.and(fa, fb);
            }
        }
        if k == 0 {
            outputs = g.outputs.iter().map(|o| map[o.node() as usize].xor(o.is_compl())).collect();
        }
    }
    let repr = equivalences(&comb, opts);

    // Rebuild with fanins on representatives; members become choices.
    let mut out = Aig::new();
    let mut map = vec![Lit::FALSE; comb.nodes.len()];
    for &n in &comb.inputs {
        map[n as usize] = out.input();
    }
    let mut choices: Vec<Vec<(u32, bool)>> = Vec::new();
    for i in 0..comb.nodes.len() {
        let Node::And(a, b) = comb.nodes[i] else { continue };
        let fa = map[a.node() as usize].xor(a.is_compl());
        let fb = map[b.node() as usize].xor(b.is_compl());
        let before = out.nodes.len();
        let l = out.and(fa, fb);
        match repr[i] {
            None => map[i] = l,
            Some((r, c)) => {
                let target = map[r as usize].xor(c);
                map[i] = target;
                // A fresh node becomes a choice of the representative, unless
                // its own fanin cone reaches the representative (a cycle).
                if out.nodes.len() > before && !target.is_const() {
                    choices.resize(out.nodes.len(), Vec::new());
                    let (m, rn) = (l.node(), target.node());
                    if !reaches(&out, &choices, m, rn) {
                        choices[rn as usize].push((m, l.is_compl() ^ target.is_compl()));
                    }
                }
            }
        }
    }
    choices.resize(out.nodes.len(), Vec::new());
    out.outputs = outputs.iter().map(|o| map[o.node() as usize].xor(o.is_compl())).collect();
    ChoiceAig { aig: out, choices }
}

/// Whether `target` is in the fanin cone of `from` (following fanins and
/// existing choices). Gives up (answering yes) on very large cones.
fn reaches(g: &Aig, choices: &[Vec<(u32, bool)>], from: u32, target: u32) -> bool {
    let mut stack = vec![from];
    let mut seen = synth_core::FxHashSet::default();
    while let Some(x) = stack.pop() {
        if x == target {
            return true;
        }
        if !seen.insert(x) {
            continue;
        }
        if seen.len() > 2000 {
            return true;
        }
        if let Node::And(a, b) = g.nodes[x as usize] {
            stack.push(a.node());
            stack.push(b.node());
        }
        if let Some(ms) = choices.get(x as usize) {
            stack.extend(ms.iter().map(|&(m, _)| m));
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_two_structures_of_an_and() {
        // ((a & b) & c) & d versus (a & b) & (c & d).
        let mut g0 = Aig::new();
        let x: Vec<Lit> = (0..4).map(|_| g0.input()).collect();
        let ab = g0.and(x[0], x[1]);
        let abc = g0.and(ab, x[2]);
        let y = g0.and(abc, x[3]);
        g0.output(y);
        let g1 = crate::opt::balance(&g0);
        let c = build(&[&g0, &g1], &FraigOptions::default());
        assert_eq!(c.num_choices(), 1);
        let o = c.aig.outputs[0];
        assert_eq!(c.choices[o.node() as usize].len(), 1);
        let order = c.order();
        let pos = |n: u32| order.iter().position(|&x| x == n).unwrap();
        let (m, _) = c.choices[o.node() as usize][0];
        assert!(pos(m) < pos(o.node()));
    }
}
