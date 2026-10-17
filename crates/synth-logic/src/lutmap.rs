//! LUT mapping with priority cuts.
//!
//! The classic flow (Mishchenko et al., "Combinational and sequential
//! mapping with priority cuts", ICCAD 2007): every AND node keeps a small
//! set of its best K-feasible cuts, computed bottom-up from its fanins'
//! sets. A depth-optimal mapping is selected first; area is then recovered
//! without exceeding the optimal depth, with area flow and then exact local
//! area.

use crate::aig::{Aig, Node};

/// Mapping parameters.
#[derive(Clone, Debug)]
pub struct MapOptions {
    /// LUT size (at most 6).
    pub k: usize,
    /// Cuts kept per node.
    pub cuts_per_node: usize,
    /// Area-flow recovery passes.
    pub area_flow_passes: usize,
    /// Exact-area recovery passes.
    pub exact_area_passes: usize,
    /// Extra depth allowed over the optimum (in LUT levels).
    pub depth_relax: u32,
}

impl Default for MapOptions {
    fn default() -> Self {
        MapOptions {
            k: 4,
            cuts_per_node: 8,
            area_flow_passes: 2,
            exact_area_passes: 2,
            depth_relax: 0,
        }
    }
}

/// A LUT of the mapping: computes AIG node `node` (positive polarity) from
/// the values of the `leaves` nodes. Bit `i` of `truth` is the output when
/// leaf `j` has the value of bit `j` of `i`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lut {
    pub node: u32,
    pub leaves: Vec<u32>,
    pub truth: u64,
}

/// The result of mapping: LUTs in topological order.
#[derive(Clone, Debug)]
pub struct Mapping {
    pub luts: Vec<Lut>,
    /// Depth in LUT levels.
    pub depth: u32,
}

/// A cut and the function of its node over the cut's leaves (bit `i` of
/// `truth` is the value when leaf `j` has the value of bit `j` of `i`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Cut {
    leaves: [u32; 6],
    len: u8,
    sig: u64,
    truth: u64,
}

/// All-ones over the truth table of `len` variables.
fn truth_mask(len: usize) -> u64 {
    if len >= 6 { !0 } else { (1u64 << (1 << len)) - 1 }
}

/// Re-expresses truth table `t` over leaves `from` as a table over `to`
/// (a superset, both sorted).
fn expand(t: u64, from: &[u32], to: &[u32]) -> u64 {
    if from == to {
        return t;
    }
    let mut pos = [0usize; 6];
    for (j, l) in from.iter().enumerate() {
        pos[j] = to.iter().position(|x| x == l).expect("leaf in merged cut");
    }
    let mut out = 0u64;
    for idx in 0..(1usize << to.len()) {
        let mut src = 0;
        for (j, &p) in pos[..from.len()].iter().enumerate() {
            src |= ((idx >> p) & 1) << j;
        }
        out |= ((t >> src) & 1) << idx;
    }
    out
}

impl Cut {
    fn trivial(n: u32) -> Cut {
        let mut leaves = [0; 6];
        leaves[0] = n;
        Cut { leaves, len: 1, sig: 1 << (n % 64), truth: 0b10 }
    }

    fn is_trivial_of(&self, n: u32) -> bool {
        self.len == 1 && self.leaves[0] == n
    }

    fn leaves(&self) -> &[u32] {
        &self.leaves[..self.len as usize]
    }

    /// Merges two cuts if the union has at most `k` leaves.
    fn merge(a: &Cut, b: &Cut, k: usize) -> Option<Cut> {
        if (a.sig | b.sig).count_ones() as usize > k {
            return None;
        }
        let (x, y) = (a.leaves(), b.leaves());
        let mut out = [0u32; 6];
        let (mut i, mut j, mut n) = (0, 0, 0);
        while i < x.len() || j < y.len() {
            let v = if j == y.len() || (i < x.len() && x[i] < y[j]) {
                i += 1;
                x[i - 1]
            } else if i == x.len() || y[j] < x[i] {
                j += 1;
                y[j - 1]
            } else {
                i += 1;
                j += 1;
                x[i - 1]
            };
            if n == k {
                return None;
            }
            out[n] = v;
            n += 1;
        }
        Some(Cut { leaves: out, len: n as u8, sig: a.sig | b.sig, truth: 0 })
    }

    /// Whether every leaf of `self` is a leaf of `other`.
    fn dominates(&self, other: &Cut) -> bool {
        if self.len > other.len || self.sig & !other.sig != 0 {
            return false;
        }
        self.leaves().iter().all(|l| other.leaves().contains(l))
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Mode {
    Depth,
    AreaFlow,
}

struct Mapper<'a> {
    g: &'a Aig,
    opts: &'a MapOptions,
    /// Choice members of each node (empty without choices).
    choices: &'a [Vec<(u32, bool)>],
    /// Processing order: fanins and choice members first.
    order: Vec<u32>,
    cuts: Vec<Vec<Cut>>,
    best: Vec<Option<Cut>>,
    arrival: Vec<u32>,
    required: Vec<u32>,
    area_flow: Vec<f32>,
    est_refs: Vec<f32>,
    /// References of each node in the current mapping (as a LUT leaf or
    /// an output).
    map_refs: Vec<u32>,
}

/// Maps the outputs of `g` to K-input LUTs.
pub fn map(g: &Aig, opts: &MapOptions) -> Mapping {
    let order = (0..g.nodes.len() as u32).collect();
    map_impl(g, &[], order, opts)
}

/// Maps a graph with structural choices: each node may be implemented by
/// any member of its class.
pub fn map_choices(c: &crate::choice::ChoiceAig, opts: &MapOptions) -> Mapping {
    map_impl(&c.aig, &c.choices, c.order(), opts)
}

fn map_impl(g: &Aig, choices: &[Vec<(u32, bool)>], order: Vec<u32>, opts: &MapOptions) -> Mapping {
    assert!((2..=6).contains(&opts.k));
    let n = g.nodes.len();
    let fanouts = g.fanout_counts();
    let mut m = Mapper {
        g,
        opts,
        choices,
        order,
        cuts: vec![Vec::new(); n],
        best: vec![None; n],
        arrival: vec![0; n],
        required: vec![u32::MAX; n],
        area_flow: vec![0.0; n],
        est_refs: fanouts.iter().map(|&r| r.max(1) as f32).collect(),
        map_refs: vec![0; n],
    };
    m.compute_cuts(Mode::Depth);
    m.set_required();
    for _ in 0..opts.area_flow_passes {
        m.compute_cuts(Mode::AreaFlow);
        m.set_required();
    }
    for _ in 0..opts.exact_area_passes {
        m.exact_area();
        m.set_required();
    }
    m.extract()
}

impl Mapper<'_> {
    fn cut_arrival(&self, c: &Cut) -> u32 {
        1 + c.leaves().iter().map(|&l| self.arrival[l as usize]).max().unwrap_or(0)
    }

    fn cut_area_flow(&self, c: &Cut) -> f32 {
        1.0 + c.leaves().iter().map(|&l| self.area_flow[l as usize]).sum::<f32>()
    }

    fn compute_cuts(&mut self, mode: Mode) {
        let k = self.opts.k;
        for oi in 0..self.order.len() {
            let i = self.order[oi] as usize;
            let Node::And(a, b) = self.g.nodes[i] else {
                self.arrival[i] = 0;
                self.area_flow[i] = 0.0;
                self.cuts[i] = vec![Cut::trivial(i as u32)];
                continue;
            };
            let (na, nb) = (a.node() as usize, b.node() as usize);
            let mut cand: Vec<Cut> = Vec::new();
            // Keep the current best cut so area recovery never loses it.
            if let Some(c) = self.best[i] {
                cand.push(c);
            }
            let add = |cand: &mut Vec<Cut>, c: Cut| {
                if cand.iter().any(|d| d.dominates(&c)) {
                    return;
                }
                cand.retain(|d| !c.dominates(d));
                cand.push(c);
            };
            for ca in &self.cuts[na] {
                for cb in &self.cuts[nb] {
                    let Some(mut c) = Cut::merge(ca, cb, k) else { continue };
                    if cand.iter().any(|d| d.dominates(&c)) {
                        continue;
                    }
                    let mask = truth_mask(c.len as usize);
                    let ta = expand(ca.truth, ca.leaves(), c.leaves())
                        ^ if a.is_compl() { mask } else { 0 };
                    let tb = expand(cb.truth, cb.leaves(), c.leaves())
                        ^ if b.is_compl() { mask } else { 0 };
                    c.truth = ta & tb & mask;
                    add(&mut cand, c);
                }
            }
            // Cuts of the other implementations in the node's class.
            if let Some(ms) = self.choices.get(i) {
                for &(m, phase) in ms {
                    for c in &self.cuts[m as usize] {
                        if c.is_trivial_of(m) {
                            continue;
                        }
                        let mut c = *c;
                        if phase {
                            c.truth ^= truth_mask(c.len as usize);
                        }
                        add(&mut cand, c);
                    }
                }
            }
            let req = self.required[i];
            let mut scored: Vec<(Cut, u32, f32)> = cand
                .into_iter()
                .map(|c| (c, self.cut_arrival(&c), self.cut_area_flow(&c)))
                .collect();
            match mode {
                Mode::Depth => scored.sort_by(|x, y| {
                    x.1.cmp(&y.1).then(x.2.total_cmp(&y.2)).then(x.0.len.cmp(&y.0.len))
                }),
                Mode::AreaFlow => scored.sort_by(|x, y| {
                    // Cuts meeting the required time first, then by area flow.
                    (x.1 > req)
                        .cmp(&(y.1 > req))
                        .then(x.2.total_cmp(&y.2))
                        .then(x.1.cmp(&y.1))
                        .then(x.0.len.cmp(&y.0.len))
                }),
            }
            scored.truncate(self.opts.cuts_per_node);
            let (best, arr, af) = scored[0];
            self.best[i] = Some(best);
            self.arrival[i] = arr;
            self.area_flow[i] = af / self.est_refs[i];
            let mut cuts: Vec<Cut> = scored.iter().map(|s| s.0).collect();
            cuts.push(Cut::trivial(i as u32));
            self.cuts[i] = cuts;
        }
    }

    /// Marks the current mapping (reference counts) and computes required
    /// times against the (relaxed) optimal depth.
    fn set_required(&mut self) {
        let g = self.g;
        self.map_refs.iter_mut().for_each(|r| *r = 0);
        for o in &g.outputs {
            self.map_refs[o.node() as usize] += 1;
        }
        let n = g.nodes.len();
        for &i in self.order.iter().rev() {
            let i = i as usize;
            if self.map_refs[i] > 0
                && let Some(c) = self.best[i]
            {
                for &l in c.leaves() {
                    self.map_refs[l as usize] += 1;
                }
            }
        }
        let depth = g.outputs.iter().map(|o| self.arrival[o.node() as usize]).max().unwrap_or(0)
            + self.opts.depth_relax;
        self.required.iter_mut().for_each(|r| *r = u32::MAX);
        for o in &g.outputs {
            self.required[o.node() as usize] = depth;
        }
        for &i in self.order.iter().rev() {
            let i = i as usize;
            if self.map_refs[i] > 0
                && let Some(c) = self.best[i]
            {
                let r = self.required[i].saturating_sub(1);
                for &l in c.leaves() {
                    let lr = &mut self.required[l as usize];
                    *lr = (*lr).min(r);
                }
            }
        }
        // Blend reference estimates towards the actual mapping.
        for i in 0..n {
            self.est_refs[i] =
                ((2.0 * self.est_refs[i] + self.map_refs[i].max(1) as f32) / 3.0).max(1.0);
        }
    }

    /// Adds references for cut `c` of a node (recursively mapping leaves that
    /// become used); returns the number of LUTs added.
    fn cut_ref(&mut self, c: &Cut) -> u32 {
        let mut area = 1;
        for &l in c.leaves() {
            let l = l as usize;
            self.map_refs[l] += 1;
            if self.map_refs[l] == 1
                && let Some(lc) = self.best[l]
            {
                area += self.cut_ref(&lc);
            }
        }
        area
    }

    fn cut_deref(&mut self, c: &Cut) -> u32 {
        let mut area = 1;
        for &l in c.leaves() {
            let l = l as usize;
            self.map_refs[l] -= 1;
            if self.map_refs[l] == 0
                && let Some(lc) = self.best[l]
            {
                area += self.cut_deref(&lc);
            }
        }
        area
    }

    fn exact_area(&mut self) {
        for oi in 0..self.order.len() {
            let i = self.order[oi] as usize;
            let Some(cur) = self.best[i] else { continue };
            let req = self.required[i];
            let mapped = self.map_refs[i] > 0;
            if mapped {
                self.cut_deref(&cur);
            }
            let mut best = (cur, u32::MAX, self.cut_arrival(&cur));
            let cuts = std::mem::take(&mut self.cuts[i]);
            for c in &cuts {
                if c.len == 1 && c.leaves[0] == i as u32 {
                    continue; // trivial
                }
                let arr = self.cut_arrival(c);
                if arr > req && arr > best.2 {
                    continue;
                }
                let area = self.cut_ref(c);
                self.cut_deref(c);
                let better_time = best.2 > req && arr < best.2;
                if better_time
                    || (arr <= req && (area < best.1 || (area == best.1 && arr < best.2)))
                {
                    best = (*c, area, arr);
                }
            }
            self.cuts[i] = cuts;
            self.best[i] = Some(best.0);
            self.arrival[i] = best.2;
            if mapped {
                self.cut_ref(&best.0);
            }
        }
    }

    fn extract(&self) -> Mapping {
        let g = self.g;
        let n = g.nodes.len();
        let mut used = vec![false; n];
        for o in &g.outputs {
            used[o.node() as usize] = true;
        }
        for &i in self.order.iter().rev() {
            let i = i as usize;
            if used[i]
                && let Some(c) = self.best[i]
            {
                for &l in c.leaves() {
                    used[l as usize] = true;
                }
            }
        }
        let mut luts = Vec::new();
        let mut level = vec![0u32; n];
        for &i in &self.order {
            let i = i as usize;
            if !used[i] {
                continue;
            }
            if let Some(c) = self.best[i] {
                let leaves = c.leaves().to_vec();
                level[i] = 1 + leaves.iter().map(|&l| level[l as usize]).max().unwrap_or(0);
                luts.push(Lut { node: i as u32, leaves, truth: c.truth });
            }
        }
        let depth = g.outputs.iter().map(|o| level[o.node() as usize]).max().unwrap_or(0);
        Mapping { luts, depth }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aig::{Lit, lit_val};

    /// Evaluates a mapping on simulation patterns and compares every output
    /// with the AIG.
    fn check(g: &Aig, m: &Mapping, k: usize) {
        let ni = g.inputs.len();
        let mut rng = synth_core::rng::Rng::new(7);
        let pats: Vec<u64> = (0..ni).map(|_| rng.next_u64()).collect();
        let sim = g.simulate(&pats);
        let mut val: Vec<Option<u64>> = vec![None; g.nodes.len()];
        val[0] = Some(0);
        for (i, &n) in g.inputs.iter().enumerate() {
            val[n as usize] = Some(pats[i]);
        }
        for l in &m.luts {
            assert!(l.leaves.len() <= k);
            let mut out = 0u64;
            for bit in 0..64 {
                let mut idx = 0;
                for (j, &leaf) in l.leaves.iter().enumerate() {
                    let v = val[leaf as usize].expect("leaf computed before use");
                    idx |= ((v >> bit) & 1) << j;
                }
                out |= ((l.truth >> idx) & 1) << bit;
            }
            val[l.node as usize] = Some(out);
        }
        for o in &g.outputs {
            let v = val[o.node() as usize].unwrap();
            let v = if o.is_compl() { !v } else { v };
            assert_eq!(v, lit_val(&sim, *o));
        }
    }

    fn adder(g: &mut Aig, n: usize) {
        let a: Vec<Lit> = (0..n).map(|_| g.input()).collect();
        let b: Vec<Lit> = (0..n).map(|_| g.input()).collect();
        let mut c = Lit::FALSE;
        for i in 0..n {
            let s = g.xor(a[i], b[i]);
            let s = g.xor(s, c);
            g.output(s);
            c = g.maj(a[i], b[i], c);
        }
        g.output(c);
    }

    #[test]
    fn maps_adder_correctly() {
        let mut g = Aig::new();
        adder(&mut g, 8);
        for k in [3, 4, 6] {
            let m = map(&g, &MapOptions { k, ..MapOptions::default() });
            check(&g, &m, k);
        }
    }

    #[test]
    fn wide_and_needs_two_levels() {
        let mut g = Aig::new();
        let x: Vec<Lit> = (0..16).map(|_| g.input()).collect();
        let y = g.and_all(&x);
        g.output(!y);
        let m = map(&g, &MapOptions::default());
        check(&g, &m, 4);
        assert_eq!(m.depth, 2);
        assert_eq!(m.luts.len(), 5);
    }

    #[test]
    fn maps_with_choices() {
        // A wide AND chain with its balanced form as choices, and an adder.
        let mut g = Aig::new();
        let x: Vec<Lit> = (0..9).map(|_| g.input()).collect();
        let mut acc = x[0];
        for &l in &x[1..] {
            acc = g.and(acc, !l);
        }
        g.output(acc);
        adder(&mut g, 4);
        let b = crate::opt::balance(&g);
        let c = crate::choice::build(&[&g, &b], &Default::default());
        assert!(c.num_choices() > 0);
        let m = map_choices(&c, &MapOptions::default());
        check(&c.aig, &m, 4);
        let plain = map(&g, &MapOptions::default());
        assert!(m.depth <= plain.depth);
    }

    #[test]
    fn mux_tree_area() {
        // 8:1 mux, 11 inputs: optimum with 4-LUTs is depth 2 with ~7 LUTs
        // (2:1 muxes as 3-input LUTs, or 4:1 trees).
        let mut g = Aig::new();
        let s: Vec<Lit> = (0..3).map(|_| g.input()).collect();
        let d: Vec<Lit> = (0..8).map(|_| g.input()).collect();
        let mut lvl = d.clone();
        for &sel in &s {
            lvl = lvl.chunks(2).map(|p| g.mux(sel, p[1], p[0])).collect();
        }
        g.output(lvl[0]);
        let m = map(&g, &MapOptions::default());
        check(&g, &m, 4);
        assert!(m.luts.len() <= 7, "{} LUTs", m.luts.len());
    }
}
