//! Sequential optimisations: flip-flops that never change value, and
//! flip-flops that duplicate others.

use synth_core::{FxHashMap, Logic};

use crate::aig::Lit;
use crate::carry::substitute_inputs;
use crate::fraig::{FraigOptions, fraig};
use crate::net::{LogicNet, Source};

fn konst(b: bool) -> Lit {
    if b { Lit::TRUE } else { Lit::FALSE }
}

/// Removes flip-flops that provably hold a constant: their power-on value
/// (unspecified counts as 0, what the hardware does) and reset values agree,
/// and assuming every such candidate holds its value, each candidate's next
/// state is that value again (an inductive proof, using structural hashing
/// and then SAT sweeping). Returns the number removed.
pub fn remove_constant_ffs(net: &mut LogicNet) -> usize {
    let out = |k: usize| net.aig.outputs[k];
    // Candidate value of each flip-flop, and whether its D matters.
    let mut cand: Vec<Option<bool>> = Vec::with_capacity(net.ffs.len());
    let mut loads: Vec<bool> = Vec::with_capacity(net.ffs.len());
    for ff in &net.ffs {
        let mut vals = vec![ff.init == Logic::One];
        let en_never = ff.en.is_some_and(|e| out(e) == Lit::FALSE);
        let never_clocked = out(ff.clk).is_const();
        if let Some((s, v)) = ff.srst
            && out(s) != Lit::FALSE
            && !never_clocked
            && !(en_never && ff.en_over_srst)
        {
            vals.push(v);
        }
        if let Some((s, v)) = ff.arst
            && out(s) != Lit::FALSE
        {
            vals.push(v);
        }
        cand.push(if vals.iter().all(|&v| v == vals[0]) { Some(vals[0]) } else { None });
        loads.push(!never_clocked && !en_never);
    }
    let mut use_sat = false;
    loop {
        let subst: FxHashMap<Source, Lit> =
            net.ffs.iter().zip(&cand).filter_map(|(ff, c)| c.map(|v| (ff.q, konst(v)))).collect();
        if subst.is_empty() {
            return 0;
        }
        let mut g = substitute_inputs(&net.aig, &subst);
        if use_sat {
            g = fraig(&g, &FraigOptions::default()).0;
        }
        let mut violated = false;
        for (i, ff) in net.ffs.iter().enumerate() {
            if let Some(v) = cand[i]
                && loads[i]
                && g.outputs[ff.d] != konst(v)
            {
                cand[i] = None;
                violated = true;
            }
        }
        if violated {
            continue;
        }
        if !use_sat {
            use_sat = true;
            continue;
        }
        // Proved: substitute the constants and drop the flip-flops.
        net.aig = substitute_inputs(&net.aig, &subst);
        let n = subst.len();
        let mut i = 0;
        net.ffs.retain(|_| {
            i += 1;
            cand[i - 1].is_none()
        });
        return n;
    }
}

/// Merges flip-flops with identical inputs, controls and power-on values.
/// Returns the number removed.
pub fn merge_duplicate_ffs(net: &mut LogicNet) -> usize {
    let out = |k: usize| net.aig.outputs[k];
    let mut seen: FxHashMap<_, Source> = FxHashMap::default();
    let mut subst: FxHashMap<Source, Lit> = FxHashMap::default();
    for ff in &net.ffs {
        let key = (
            out(ff.clk),
            ff.posedge,
            out(ff.d),
            ff.en.map(out),
            ff.srst.map(|(s, v)| (out(s), v)),
            ff.en_over_srst,
            ff.arst.map(|(s, v)| (out(s), v)),
            ff.init == Logic::One,
        );
        match seen.get(&key) {
            Some(&q) => {
                subst.insert(ff.q, Lit::new(net.aig.inputs[q as usize], false));
            }
            None => {
                seen.insert(key, ff.q);
            }
        }
    }
    if subst.is_empty() {
        return 0;
    }
    net.aig = substitute_inputs(&net.aig, &subst);
    net.ffs.retain(|ff| !subst.contains_key(&ff.q));
    subst.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::Ff;

    fn ff(net: &mut LogicNet, clk: usize, d: Lit, q: Source, init: Logic) -> Ff {
        Ff {
            name: format!("q{q}"),
            clk,
            posedge: true,
            d: net.aig.output(d),
            en: None,
            srst: None,
            en_over_srst: false,
            arst: None,
            init,
            q,
        }
    }

    #[test]
    fn removes_mutually_constant_flops() {
        let mut net = LogicNet::default();
        let g = &mut net.aig;
        let clk = g.input();
        let a = g.input();
        let q0 = g.input();
        let q1 = g.input();
        let q2 = g.input();
        // q0' = a & q1, q1' = q0 | q1 & !q1: both stay 0 from 0.
        // q2' = !q2 toggles.
        let d0 = g.and(a, q1);
        let t = g.and(q1, !q1);
        let d1 = g.or(q0, t);
        let ck = g.output(clk);
        let (s0, s1, s2) = (q0.node() - 1, q1.node() - 1, q2.node() - 1);
        let f0 = ff(&mut net, ck, d0, s0, Logic::Zero);
        let f1 = ff(&mut net, ck, d1, s1, Logic::X);
        let f2 = ff(&mut net, ck, !q2, s2, Logic::Zero);
        net.ffs = vec![f0, f1, f2];
        let o = net.aig.or(q0, q2);
        net.aig.output(o);
        assert_eq!(remove_constant_ffs(&mut net), 2);
        assert_eq!(net.ffs.len(), 1);
        let last = *net.aig.outputs.last().unwrap();
        assert_eq!(last, q2);
    }

    #[test]
    fn keeps_flops_that_leave_their_initial_value() {
        let mut net = LogicNet::default();
        let g = &mut net.aig;
        let clk = g.input();
        let a = g.input();
        let q0 = g.input();
        let d0 = g.or(a, q0);
        let ck = g.output(clk);
        let f0 = ff(&mut net, ck, d0, q0.node() - 1, Logic::Zero);
        net.ffs = vec![f0];
        assert_eq!(remove_constant_ffs(&mut net), 0);
    }

    #[test]
    fn merges_identical_flops() {
        let mut net = LogicNet::default();
        let g = &mut net.aig;
        let clk = g.input();
        let a = g.input();
        let q0 = g.input();
        let q1 = g.input();
        let ck = g.output(clk);
        let f0 = ff(&mut net, ck, a, q0.node() - 1, Logic::Zero);
        let f1 = ff(&mut net, ck, a, q1.node() - 1, Logic::Zero);
        net.ffs = vec![f0, f1];
        let x = net.aig.xor(q0, q1);
        net.aig.output(x);
        assert_eq!(merge_duplicate_ffs(&mut net), 1);
        assert_eq!(*net.aig.outputs.last().unwrap(), Lit::FALSE);
    }
}
