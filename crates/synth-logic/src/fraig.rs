//! Functional reduction ("fraiging", SAT sweeping): merges AIG nodes that
//! compute the same function up to complement. Candidate classes come from
//! random simulation; each candidate is proved or refuted with the SAT
//! solver, and counterexamples refine the classes.

use synth_core::{FxHashMap, Rng};

use crate::aig::{Aig, Lit, Node};
use crate::sat::{SLit, SatResult, Solver};

#[derive(Clone, Debug)]
pub struct FraigOptions {
    /// 64-pattern words of initial random simulation.
    pub words: usize,
    /// Conflicts allowed per SAT query before giving up on a candidate.
    pub conflict_limit: u64,
    pub seed: u64,
}

impl Default for FraigOptions {
    fn default() -> Self {
        FraigOptions { words: 16, conflict_limit: 500, seed: 1 }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FraigStats {
    pub proved: usize,
    pub refuted: usize,
    pub unknown: usize,
}

fn mix(h: u64, v: u64) -> u64 {
    let mut z = h.rotate_left(23) ^ v;
    z = (z ^ (z >> 31)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z ^ (z >> 29)
}

/// Simulation signatures and the equivalence classes they induce.
struct Classes {
    /// Hash of each node's normalised simulation values.
    key: Vec<u64>,
    /// Complement of each node relative to its normalised signature.
    phase: Vec<bool>,
    /// Smallest node of each class.
    head: FxHashMap<u64, u32>,
}

impl Classes {
    fn rebuild_heads(&mut self) {
        self.head.clear();
        for (n, &k) in self.key.iter().enumerate() {
            self.head.entry(k).or_insert(n as u32);
        }
    }

    fn refine(&mut self, vals: &[u64]) {
        for (n, k) in self.key.iter_mut().enumerate() {
            let v = if self.phase[n] { !vals[n] } else { vals[n] };
            *k = mix(*k, v);
        }
        self.rebuild_heads();
    }
}

/// The SAT encoding of a growing AIG: one variable per node, Tseitin
/// clauses added on demand for the cones being compared.
struct Encoder {
    solver: Solver,
    encoded: Vec<bool>,
}

impl Encoder {
    fn new() -> Encoder {
        let mut solver = Solver::new();
        let v0 = solver.new_var();
        solver.add_clause(&[SLit::new(v0, true)]);
        Encoder { solver, encoded: vec![true] }
    }

    fn slit(l: Lit) -> SLit {
        SLit::new(l.node(), l.is_compl())
    }

    fn encode(&mut self, g: &Aig, root: u32) {
        while self.solver.num_vars() < g.nodes.len() as u32 {
            self.solver.new_var();
            self.encoded.push(false);
        }
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            if self.encoded[n as usize] {
                continue;
            }
            self.encoded[n as usize] = true;
            if let Node::And(a, b) = g.nodes[n as usize] {
                let (x, a, b) = (SLit::new(n, false), Self::slit(a), Self::slit(b));
                self.solver.add_clause(&[!x, a]);
                self.solver.add_clause(&[!x, b]);
                self.solver.add_clause(&[x, !a, !b]);
                stack.push(a.var());
                stack.push(b.var());
            }
        }
    }

    /// Checks `a == b`. On a difference, returns the values of the graph's
    /// inputs in the counterexample (unconstrained inputs from `rng`).
    fn equal(
        &mut self,
        g: &Aig,
        a: Lit,
        b: Lit,
        limit: u64,
        rng: &mut Rng,
    ) -> Result<bool, Vec<bool>> {
        self.encode(g, a.node());
        self.encode(g, b.node());
        let (sa, sb) = (Self::slit(a), Self::slit(b));
        let mut unknown = false;
        for assume in [[sa, !sb], [!sa, sb]] {
            match self.solver.solve(&assume, limit) {
                SatResult::Unsat => {}
                SatResult::Unknown => unknown = true,
                SatResult::Sat => {
                    let cex = g
                        .inputs
                        .iter()
                        .map(|&n| {
                            if self.encoded[n as usize] {
                                self.solver.model_value(n)
                            } else {
                                rng.below(2) == 1
                            }
                        })
                        .collect();
                    return Err(cex);
                }
            }
        }
        if unknown {
            return Ok(false);
        }
        // Record the equivalence; it helps later proofs.
        self.solver.add_clause(&[!sa, sb]);
        self.solver.add_clause(&[sa, !sb]);
        Ok(true)
    }
}

/// Returns a functionally equivalent graph with equivalent nodes merged,
/// keeping the inputs and outputs in order. Dangling logic is left for
/// [`Aig::cleanup`].
pub fn fraig(aig: &Aig, opts: &FraigOptions) -> (Aig, FraigStats) {
    let mut rng = Rng::new(opts.seed);
    let n = aig.nodes.len();
    let mut cls = Classes { key: vec![0; n], phase: vec![false; n], head: FxHashMap::default() };
    for w in 0..opts.words.max(1) {
        let inputs: Vec<u64> = aig.inputs.iter().map(|_| rng.next_u64()).collect();
        let vals = aig.simulate(&inputs);
        if w == 0 {
            for (p, v) in cls.phase.iter_mut().zip(&vals) {
                *p = v & 1 == 1;
            }
        }
        for (k, (&v, &p)) in cls.key.iter_mut().zip(vals.iter().zip(&cls.phase)) {
            *k = mix(*k, if p { !v } else { v });
        }
    }
    cls.rebuild_heads();

    let mut out = Aig::new();
    let mut map = vec![Lit::FALSE; n];
    for &i in &aig.inputs {
        map[i as usize] = out.input();
    }
    let mut enc = Encoder::new();
    let mut stats = FraigStats::default();
    for i in 0..n {
        let Node::And(a, b) = aig.nodes[i] else { continue };
        let l = out.and(
            map[a.node() as usize].xor(a.is_compl()),
            map[b.node() as usize].xor(b.is_compl()),
        );
        map[i] = l;
        if l.is_const() {
            continue;
        }
        loop {
            let r = cls.head[&cls.key[i]] as usize;
            if r == i {
                break;
            }
            let target = map[r].xor(cls.phase[i] ^ cls.phase[r]);
            if target == l {
                break;
            }
            match enc.equal(&out, l, target, opts.conflict_limit, &mut rng) {
                Ok(true) => {
                    map[i] = target;
                    stats.proved += 1;
                    break;
                }
                Ok(false) => {
                    // Undecided: take the node out of its class.
                    stats.unknown += 1;
                    cls.key[i] = mix(cls.key[i], 0xD1B5_4A32_D192_ED03 ^ i as u64);
                    cls.head.insert(cls.key[i], i as u32);
                    break;
                }
                Err(cex) => {
                    stats.refuted += 1;
                    // The counterexample in bit 0, and patterns at distance
                    // one from it in the other bits.
                    let mut inputs: Vec<u64> =
                        cex.iter().map(|&b| if b { !0 } else { 0 }).collect();
                    for bit in 1..64 {
                        let k = rng.below(inputs.len().max(1));
                        if let Some(w) = inputs.get_mut(k) {
                            *w ^= 1 << bit;
                        }
                    }
                    cls.refine(&aig.simulate(&inputs));
                }
            }
        }
    }
    out.outputs = aig.outputs.iter().map(|o| map[o.node() as usize].xor(o.is_compl())).collect();
    (out, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_equivalent(a: &Aig, b: &Aig) {
        let mut rng = Rng::new(3);
        for _ in 0..8 {
            let inputs: Vec<u64> = a.inputs.iter().map(|_| rng.next_u64()).collect();
            let (va, vb) = (a.simulate(&inputs), b.simulate(&inputs));
            for (oa, ob) in a.outputs.iter().zip(&b.outputs) {
                assert_eq!(crate::aig::lit_val(&va, *oa), crate::aig::lit_val(&vb, *ob));
            }
        }
    }

    #[test]
    fn merges_structurally_different_equivalents() {
        let mut g = Aig::new();
        let x: Vec<Lit> = (0..4).map(|_| g.input()).collect();
        // Two forms of a 4-input XOR, and a mux written two ways.
        let p = g.xor(x[0], x[1]);
        let q = g.xor(x[2], x[3]);
        let x1 = g.xor(p, q);
        let r = g.xor(x[0], x[2]);
        let s = g.xor(x[1], x[3]);
        let x2 = g.xor(r, s);
        let m1 = g.mux(x[0], x[1], x[2]);
        let t = g.or(x[1], !x[0]);
        let e = g.or(x[2], x[0]);
        let m2 = g.and(t, e);
        // A redundant constant: x0 & x1 & !x0.
        let k = g.and(x[0], x[1]);
        let z = g.and(k, !x[0]);
        for o in [x1, !x2, m1, m2, z] {
            g.output(o);
        }
        let (f, st) = fraig(&g, &FraigOptions::default());
        let f = f.cleanup();
        check_equivalent(&g, &f);
        assert!(st.proved >= 2, "{st:?}");
        assert_eq!(f.outputs[0], !f.outputs[1]);
        assert_eq!(f.outputs[2], f.outputs[3]);
        assert_eq!(f.outputs[4], Lit::FALSE);
        assert!(f.num_ands() < g.num_ands());
    }

    #[test]
    fn keeps_distinct_functions() {
        // Functions that differ on a single pattern out of 2^16.
        let mut g = Aig::new();
        let x: Vec<Lit> = (0..16).map(|_| g.input()).collect();
        let all = g.and_all(&x);
        let nearly = g.and_all(&x[..15]);
        let other = g.and(nearly, !x[15]);
        g.output(all);
        g.output(other);
        g.output(Lit::FALSE);
        let (f, _) = fraig(&g, &FraigOptions::default());
        let f = f.cleanup();
        check_equivalent(&g, &f);
        assert_ne!(f.outputs[0], Lit::FALSE);
        assert_ne!(f.outputs[1], Lit::FALSE);
        assert_ne!(f.outputs[0], f.outputs[1]);
    }
}
