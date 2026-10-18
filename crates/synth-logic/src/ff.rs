//! Flip-flop legalisation: rewrites flip-flop controls the target cannot
//! implement into logic in front of them.

use synth_core::Logic;

use crate::aig::{Aig, Lit, Node};
use crate::net::LogicNet;

/// What the target's flip-flops support.
#[derive(Clone, Debug)]
pub struct FfRules {
    /// Power-on value 1 (else such flip-flops are inverted).
    pub init_one: bool,
    /// Synchronous and asynchronous reset on the same flip-flop.
    pub srst_with_arst: bool,
    /// A synchronous reset that acts even when the enable is low.
    pub srst_over_en: bool,
}

/// Legalises every flip-flop of `net` for `rules`.
pub fn legalize(net: &mut LogicNet, rules: &FfRules) {
    let mut invert = vec![false; net.aig.inputs.len()];
    for i in 0..net.ffs.len() {
        let ff = net.ffs[i].clone();
        let mut ff2 = ff.clone();
        let g = &mut net.aig;
        let val = |b: bool| if b { Lit::TRUE } else { Lit::FALSE };
        // Sync reset that cannot stay: fold it into D (and the enable).
        if let Some((r, v)) = ff.srst
            && ff.arst.is_some()
            && !rules.srst_with_arst
        {
            let (rl, dl) = (g.outputs[r], g.outputs[ff.d]);
            g.outputs[ff.d] = g.mux(rl, val(v), dl);
            if let (Some(e), false) = (ff.en, ff.en_over_srst) {
                let el = g.outputs[e];
                let ne = g.or(el, rl);
                ff2.en = Some(g.output(ne));
            }
            ff2.srst = None;
            ff2.en_over_srst = false;
        }
        if let (Some((r, _)), Some(e), false) = (ff2.srst, ff2.en, ff2.en_over_srst)
            && !rules.srst_over_en
        {
            // Reset over enable: enable whenever resetting.
            let (rl, el) = (g.outputs[r], g.outputs[e]);
            let ne = g.or(el, rl);
            ff2.en = Some(g.output(ne));
            ff2.en_over_srst = true;
        }
        if ff2.init == Logic::One && !rules.init_one {
            // Store the complement: power-on 0 means 1.
            invert[ff2.q as usize] = true;
            let dl = g.outputs[ff2.d];
            ff2.d = g.output(!dl);
            ff2.srst = ff2.srst.map(|(s, v)| (s, !v));
            ff2.arst = ff2.arst.map(|(s, v)| (s, !v));
            ff2.init = Logic::Zero;
        }
        net.ffs[i] = ff2;
    }
    if invert.iter().any(|&x| x) {
        net.aig = invert_inputs(&net.aig, &invert);
    }
}

/// Rebuilds `g` with the given inputs complemented wherever they are used.
fn invert_inputs(g: &Aig, invert: &[bool]) -> Aig {
    let mut out = Aig::new();
    let mut map = vec![Lit::FALSE; g.nodes.len()];
    for (i, n) in g.nodes.iter().enumerate() {
        map[i] = match *n {
            Node::Const => Lit::FALSE,
            Node::Input(k) => out.input().xor(invert[k as usize]),
            Node::And(a, b) => {
                let fa = map[a.node() as usize].xor(a.is_compl());
                let fb = map[b.node() as usize].xor(b.is_compl());
                out.and(fa, fb)
            }
        };
    }
    out.outputs = g.outputs.iter().map(|o| map[o.node() as usize].xor(o.is_compl())).collect();
    out
}
