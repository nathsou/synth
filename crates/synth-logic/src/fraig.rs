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
    /// Complement of each node relative to its normalised signature.
    phase: Vec<bool>,
    /// Class of each node (`NONE`: alone).
    class_of: Vec<u32>,
    /// Members of each class, in increasing order (empty: dissolved).
    members: Vec<Vec<u32>>,
    /// Reused simulation buffer.
    vals: Vec<u64>,
}

const NONE: u32 = u32::MAX;

impl Classes {
    /// Classes from `words` 64-pattern words of random simulation.
    fn simulate(aig: &Aig, words: usize, rng: &mut Rng) -> Classes {
        let n = aig.nodes.len();
        let mut key = vec![0u64; n];
        let mut phase = vec![false; n];
        for w in 0..words.max(1) {
            let inputs: Vec<u64> = aig.inputs.iter().map(|_| rng.next_u64()).collect();
            let vals = aig.simulate(&inputs);
            if w == 0 {
                for (p, v) in phase.iter_mut().zip(&vals) {
                    *p = v & 1 == 1;
                }
            }
            for (k, (&v, &p)) in key.iter_mut().zip(vals.iter().zip(&phase)) {
                *k = mix(*k, if p { !v } else { v });
            }
        }
        let mut by_key: FxHashMap<u64, Vec<u32>> = FxHashMap::default();
        for (i, &k) in key.iter().enumerate() {
            by_key.entry(k).or_default().push(i as u32);
        }
        let mut cls =
            Classes { phase, class_of: vec![NONE; n], members: Vec::new(), vals: Vec::new() };
        let mut groups: Vec<Vec<u32>> = by_key.into_values().filter(|g| g.len() > 1).collect();
        groups.sort_unstable_by_key(|g| g[0]);
        for g in groups {
            cls.add_class(g);
        }
        cls
    }

    fn add_class(&mut self, members: Vec<u32>) {
        let c = self.members.len() as u32;
        for &m in &members {
            self.class_of[m as usize] = c;
        }
        self.members.push(members);
    }

    /// The first node of `i`'s class.
    fn head(&self, i: usize) -> usize {
        match self.class_of[i] {
            NONE => i,
            c => self.members[c as usize][0] as usize,
        }
    }

    /// Refines the classes with a counterexample (in bit 0) and patterns at
    /// distance one from it (in the other bits).
    fn refine_with(&mut self, aig: &Aig, cex: &[bool], rng: &mut Rng) {
        let mut inputs: Vec<u64> = cex.iter().map(|&b| if b { !0 } else { 0 }).collect();
        for bit in 1..64 {
            let k = rng.below(inputs.len().max(1));
            if let Some(w) = inputs.get_mut(k) {
                *w ^= 1 << bit;
            }
        }
        let mut vals = std::mem::take(&mut self.vals);
        aig.simulate_into(&inputs, &mut vals);
        self.refine(&vals);
        self.vals = vals;
    }

    /// Takes node `i` out of its class.
    fn isolate(&mut self, i: usize) {
        let c = self.class_of[i];
        if c == NONE {
            return;
        }
        self.class_of[i] = NONE;
        let m = &mut self.members[c as usize];
        m.retain(|&x| x as usize != i);
        if m.len() == 1 {
            self.class_of[m[0] as usize] = NONE;
            m.clear();
        }
    }

    /// Splits every class by the members' (normalised) simulation values.
    fn refine(&mut self, vals: &[u64]) {
        for c in 0..self.members.len() {
            if self.members[c].is_empty() {
                continue;
            }
            let norm = |n: u32| {
                let v = vals[n as usize];
                if self.phase[n as usize] { !v } else { v }
            };
            let v0 = norm(self.members[c][0]);
            if self.members[c].iter().all(|&n| norm(n) == v0) {
                continue;
            }
            let mut ms = std::mem::take(&mut self.members[c]);
            // Stable sort by value keeps each group in increasing node order.
            ms.sort_by_key(|&n| norm(n));
            let mut groups: Vec<Vec<u32>> = Vec::new();
            for n in ms {
                match groups.last_mut() {
                    Some(g) if norm(g[0]) == norm(n) => g.push(n),
                    _ => groups.push(vec![n]),
                }
            }
            for g in groups {
                if g.len() == 1 {
                    self.class_of[g[0] as usize] = NONE;
                } else if self.members[c].is_empty() {
                    for &m in &g {
                        self.class_of[m as usize] = c as u32;
                    }
                    self.members[c] = g;
                } else {
                    self.add_class(g);
                }
            }
        }
    }
}

/// Equivalence checks by SAT, each on a fresh solver holding only the cones
/// of the two nodes compared (a solver shared across checks would assign
/// every variable it ever saw on each satisfiable query).
struct Encoder {
    /// Solver variable of each node in the current query (`stamp` says which
    /// query it belongs to).
    var: Vec<u32>,
    stamp: Vec<u32>,
    epoch: u32,
    cone: Vec<u32>,
    stack: Vec<u32>,
}

impl Encoder {
    fn new() -> Encoder {
        Encoder {
            var: Vec::new(),
            stamp: Vec::new(),
            epoch: 0,
            cone: Vec::new(),
            stack: Vec::new(),
        }
    }

    /// Checks `a == b`, reading every node through `repr` (nodes already
    /// proved equal to an earlier one, possibly complemented). On a
    /// difference, returns the values of the graph's inputs in the
    /// counterexample (inputs outside both cones from `rng`).
    fn equal(
        &mut self,
        g: &Aig,
        repr: &[Option<(u32, bool)>],
        a: Lit,
        b: Lit,
        limit: u64,
        rng: &mut Rng,
    ) -> Result<bool, Vec<bool>> {
        let res = |l: Lit| match repr.get(l.node() as usize).copied().flatten() {
            Some((r, c)) => Lit::new(r, c ^ l.is_compl()),
            None => l,
        };
        let (a, b) = (res(a), res(b));
        let n = g.nodes.len();
        if self.var.len() < n {
            self.var.resize(n, 0);
            self.stamp.resize(n, 0);
        }
        self.epoch += 1;
        let epoch = self.epoch;
        // The union of both cones, then one variable per node.
        self.cone.clear();
        self.stack.clear();
        self.stack.extend([a.node(), b.node()]);
        while let Some(x) = self.stack.pop() {
            if self.stamp[x as usize] == epoch {
                continue;
            }
            self.stamp[x as usize] = epoch;
            self.cone.push(x);
            if let Node::And(p, q) = g.nodes[x as usize] {
                self.stack.push(res(p).node());
                self.stack.push(res(q).node());
            }
        }
        let mut solver = Solver::new();
        for &x in &self.cone {
            self.var[x as usize] = solver.new_var();
        }
        let lit = |var: &[u32], l: Lit| SLit::new(var[l.node() as usize], l.is_compl());
        for &x in &self.cone {
            let v = SLit::new(self.var[x as usize], false);
            match g.nodes[x as usize] {
                Node::Const => {
                    solver.add_clause(&[!v]);
                }
                Node::And(p, q) => {
                    let (p, q) = (lit(&self.var, res(p)), lit(&self.var, res(q)));
                    solver.add_clause(&[!v, p]);
                    solver.add_clause(&[!v, q]);
                    solver.add_clause(&[v, !p, !q]);
                }
                Node::Input(_) => {}
            }
        }
        let (sa, sb) = (lit(&self.var, a), lit(&self.var, b));
        let mut unknown = false;
        for assume in [[sa, !sb], [!sa, sb]] {
            match solver.solve(&assume, limit) {
                SatResult::Unsat => {}
                SatResult::Unknown => unknown = true,
                SatResult::Sat => {
                    let cex = g
                        .inputs
                        .iter()
                        .map(|&i| {
                            if self.stamp[i as usize] == epoch {
                                solver.model_value(self.var[i as usize])
                            } else {
                                rng.below(2) == 1
                            }
                        })
                        .collect();
                    return Err(cex);
                }
            }
        }
        Ok(!unknown)
    }
}

/// Returns a functionally equivalent graph with equivalent nodes merged,
/// keeping the inputs and outputs in order. Dangling logic is left for
/// [`Aig::cleanup`].
pub fn fraig(aig: &Aig, opts: &FraigOptions) -> (Aig, FraigStats) {
    let mut rng = Rng::new(opts.seed);
    let n = aig.nodes.len();
    let mut cls = Classes::simulate(aig, opts.words, &mut rng);

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
            let r = cls.head(i);
            if r == i {
                break;
            }
            let target = map[r].xor(cls.phase[i] ^ cls.phase[r]);
            if target == l {
                break;
            }
            match enc.equal(&out, &[], l, target, opts.conflict_limit, &mut rng) {
                Ok(true) => {
                    map[i] = target;
                    stats.proved += 1;
                    break;
                }
                Ok(false) => {
                    // Undecided: take the node out of its class.
                    stats.unknown += 1;
                    cls.isolate(i);
                    break;
                }
                Err(cex) => {
                    stats.refuted += 1;
                    cls.refine_with(aig, &cex, &mut rng);
                }
            }
        }
    }
    out.outputs = aig.outputs.iter().map(|o| map[o.node() as usize].xor(o.is_compl())).collect();
    (out, stats)
}

/// Proves which nodes of `aig` are equivalent to an earlier node: entry
/// `i` is `Some((r, c))` when node `i` equals node `r < i`, complemented if
/// `c`. Each node points at the first node of its class.
pub fn equivalences(aig: &Aig, opts: &FraigOptions) -> Vec<Option<(u32, bool)>> {
    let mut rng = Rng::new(opts.seed);
    let n = aig.nodes.len();
    let mut cls = Classes::simulate(aig, opts.words, &mut rng);
    let mut enc = Encoder::new();
    let mut repr: Vec<Option<(u32, bool)>> = vec![None; n];
    for i in 0..n {
        if !matches!(aig.nodes[i], Node::And(..)) {
            continue;
        }
        loop {
            let r = cls.head(i);
            if r == i {
                break;
            }
            let c = cls.phase[i] ^ cls.phase[r];
            match enc.equal(
                aig,
                &repr,
                Lit::new(i as u32, false),
                Lit::new(r as u32, c),
                opts.conflict_limit,
                &mut rng,
            ) {
                Ok(true) => {
                    // Point at the class's first node.
                    repr[i] = Some(match repr[r] {
                        Some((r0, c0)) => (r0, c0 ^ c),
                        None => (r as u32, c),
                    });
                    break;
                }
                Ok(false) => {
                    cls.isolate(i);
                    break;
                }
                Err(cex) => cls.refine_with(aig, &cex, &mut rng),
            }
        }
    }
    repr
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
