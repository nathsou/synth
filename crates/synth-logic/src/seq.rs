//! Sequential optimisations: flip-flops that never change value,
//! flip-flops that duplicate others, and synchronous resets folded into
//! flip-flop inputs.

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

/// Flip-flops that must share a candidate reset for it to be extracted
/// (the flip-flops of a tile share their set/reset signal).
const MIN_RESET_SHARING: usize = 4;

/// Drops enables that are always on, and gives flip-flops whose input is
/// `!r & x` (or `r | x`) the synchronous reset (set) `r` and the input `x`,
/// when at least a few flip-flops can share `r`. Flip-flops with an
/// asynchronous reset or a synchronous one already are left alone. Returns
/// the number of resets extracted.
pub fn extract_sync_resets(net: &mut LogicNet) -> usize {
    // The ways each flip-flop's input splits: (reset, value, data).
    let mut options: Vec<Vec<(Lit, bool, Lit)>> = Vec::with_capacity(net.ffs.len());
    let mut count: FxHashMap<Lit, usize> = FxHashMap::default();
    for ff in &mut net.ffs {
        if ff.en.is_some_and(|e| net.aig.outputs[e] == Lit::TRUE) {
            ff.en = None;
        }
        let d = net.aig.outputs[ff.d];
        let mut opts = Vec::new();
        if ff.srst.is_none()
            && ff.arst.is_none()
            && let Some((x, y)) = net.aig.fanins(d.node())
        {
            // d = x & y: reset to 0 when either operand is 0; !d = x & y:
            // set to 1 likewise.
            let v = d.is_compl();
            opts.push((!x, v, y.xor(v)));
            opts.push((!y, v, x.xor(v)));
            for o in &opts {
                *count.entry(o.0).or_default() += 1;
            }
        }
        options.push(opts);
    }
    let mut n = 0;
    for (i, opts) in options.iter().enumerate() {
        let Some(&(r, v, data)) = opts.iter().max_by_key(|o| count[&o.0]) else { continue };
        if count[&r] < MIN_RESET_SHARING {
            continue;
        }
        let g = &mut net.aig;
        let ff = &mut net.ffs[i];
        ff.d = g.output(data);
        ff.srst = Some((g.output(r), v));
        // With an enable, the reset acts only when enabled, as the input did.
        ff.en_over_srst = ff.en.is_some();
        n += 1;
    }
    n
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
    fn extracts_shared_synchronous_resets() {
        // q_i' = !rst & x_i for four flip-flops (one with an enable always
        // on), q4' = rst | x0 (a set), and q5' = !rst & x1 with an
        // asynchronous reset, left alone.
        let mut net = LogicNet::default();
        let g = &mut net.aig;
        let clk = g.input();
        let rst = g.input();
        let x: Vec<Lit> = (0..4).map(|_| g.input()).collect();
        let q: Vec<Lit> = (0..6).map(|_| g.input()).collect();
        let ck = g.output(clk);
        let src = |l: Lit| l.node() - 1;
        let mut ffs = Vec::new();
        for i in 0..4 {
            let d = net.aig.and(!rst, x[i]);
            ffs.push(ff(&mut net, ck, d, src(q[i]), Logic::X));
        }
        ffs[3].en = Some(net.aig.output(Lit::TRUE));
        let d = net.aig.or(rst, x[0]);
        ffs.push(ff(&mut net, ck, d, src(q[4]), Logic::X));
        let d = net.aig.and(!rst, x[1]);
        let mut f5 = ff(&mut net, ck, d, src(q[5]), Logic::X);
        f5.arst = Some((net.aig.output(rst), false));
        ffs.push(f5);
        net.ffs = ffs;
        assert_eq!(extract_sync_resets(&mut net), 5);
        let out = |k: usize| net.aig.outputs[k];
        for (i, f) in net.ffs[..4].iter().enumerate() {
            assert_eq!(out(f.d), x[i]);
            assert_eq!(f.srst.map(|(s, v)| (out(s), v)), Some((rst, false)));
            assert!(f.en.is_none());
        }
        let f4 = &net.ffs[4];
        assert_eq!((out(f4.d), f4.srst.map(|(s, v)| (out(s), v))), (x[0], Some((rst, true))));
        assert!(net.ffs[5].srst.is_none());
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
