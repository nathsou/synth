//! Don't-care-based optimisation of a LUT mapping (after "mfs" in ABC:
//! Mishchenko, Brayton, Jiang and Jang, "Scalable don't-care-based logic
//! optimization and resynthesis", FPGA 2009).
//!
//! Each LUT is examined in a window: a few levels of its fanouts, and fanin
//! cones expanded while the window has at most a dozen inputs. The window is
//! simulated exhaustively over its inputs, taken as independent (a superset
//! of the patterns that occur); the LUT's care set is where flipping it
//! changes a window output. On its care set, the LUT may then do without a
//! fanin LUT that only it reads, or take another signal of the window in
//! that fanin's place, or equal another signal altogether; the LUTs left
//! unused are removed. Every change is exact for the window, hence for the
//! network, and keeps every LUT within its required time.

use synth_core::FxHashMap;

use crate::aig::{Aig, Lit, Node};
use crate::lutmap::{Lut, Mapping, Timing};

#[derive(Clone, Debug)]
pub struct MfsOptions {
    /// LUT size.
    pub k: usize,
    /// Most inputs of a window (it is simulated over all their values).
    pub max_inputs: usize,
    /// Levels of fanouts in a window.
    pub tfo_levels: usize,
    /// Most nodes of a window.
    pub max_nodes: usize,
    /// Delay of a LUT, in the units of the timing constraints.
    pub lut_delay: u32,
    /// Passes over the network (stopping when one frees nothing).
    pub passes: usize,
}

impl Default for MfsOptions {
    fn default() -> Self {
        MfsOptions { k: 4, max_inputs: 12, tfo_levels: 2, max_nodes: 48, lut_delay: 1, passes: 3 }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Kind {
    Const,
    Input,
    Lut,
    Dead,
}

const NONE: u32 = u32::MAX;

/// Truth tables of six variables.
const VAR6: [u64; 6] = [
    0xAAAA_AAAA_AAAA_AAAA,
    0xCCCC_CCCC_CCCC_CCCC,
    0xF0F0_F0F0_F0F0_F0F0,
    0xFF00_FF00_FF00_FF00,
    0xFFFF_0000_FFFF_0000,
    0xFFFF_FFFF_0000_0000,
];

/// The LUT network, on dense node indices (node 0 is the constant).
struct Net<'a> {
    opts: &'a MfsOptions,
    /// Graph node of each node.
    node: Vec<u32>,
    kind: Vec<Kind>,
    fanins: Vec<Vec<u32>>,
    truth: Vec<u64>,
    fanouts: Vec<Vec<u32>>,
    po_refs: Vec<u32>,
    arrival: Vec<u32>,
    required: Vec<u32>,
    /// Replacement of each node removed in favour of another signal.
    replaced: Vec<Option<(u32, bool)>>,
    // Window state, reused across windows.
    stamp: Vec<u32>,
    epoch: u32,
    /// Role in the current window (valid when `stamp` is current).
    role: Vec<u8>,
    slot: Vec<u32>,
    vals: Vec<u64>,
    words: usize,
}

/// Roles of nodes in a window.
const TFO: u8 = 1;
const TFI: u8 = 2;
const LEAF: u8 = 3;
const SIDE: u8 = 4;

/// A window of the current node.
#[derive(Default)]
struct Window {
    /// The node and its fanouts in the window, in topological order.
    tfo: Vec<u32>,
    /// Inner fanin-side nodes, in topological order.
    tfi: Vec<u32>,
    leaves: Vec<u32>,
    /// Nodes outside the window computed from its nodes (divisors).
    side: Vec<u32>,
}

impl Net<'_> {
    fn add(&mut self, node: u32, kind: Kind) -> u32 {
        let d = self.node.len() as u32;
        self.node.push(node);
        self.kind.push(kind);
        self.fanins.push(Vec::new());
        self.truth.push(0);
        self.fanouts.push(Vec::new());
        self.po_refs.push(0);
        self.arrival.push(0);
        self.required.push(u32::MAX);
        self.replaced.push(None);
        self.stamp.push(0);
        self.role.push(0);
        self.slot.push(0);
        d
    }

    fn is_lut(&self, n: u32) -> bool {
        self.kind[n as usize] == Kind::Lut
    }

    fn refs(&self, n: u32) -> usize {
        self.fanouts[n as usize].len() + self.po_refs[n as usize] as usize
    }

    fn in_window(&self, n: u32) -> bool {
        self.stamp[n as usize] == self.epoch
    }

    fn mark(&mut self, n: u32, role: u8) {
        self.stamp[n as usize] = self.epoch;
        self.role[n as usize] = role;
    }

    /// Nodes reached from `n` through fanouts, if there are at most `limit`.
    fn tfo_set(&self, n: u32, limit: usize) -> Option<synth_core::FxHashSet<u32>> {
        let mut seen = synth_core::FxHashSet::default();
        let mut stack = vec![n];
        while let Some(x) = stack.pop() {
            for &f in &self.fanouts[x as usize] {
                if seen.insert(f) {
                    if seen.len() > limit {
                        return None;
                    }
                    stack.push(f);
                }
            }
        }
        Some(seen)
    }

    /// The window of `n`, or `None` if its inputs cannot be kept within the
    /// limit.
    fn window(&mut self, n: u32) -> Option<Window> {
        let tfo_all = self.tfo_set(n, 400);
        let max_levels = if tfo_all.is_some() { self.opts.tfo_levels } else { 0 };
        for levels in (0..=max_levels).rev() {
            self.epoch += 1;
            let mut w = Window::default();
            self.mark(n, TFO);
            w.tfo.push(n);
            let mut frontier = vec![n];
            for _ in 0..levels {
                let mut next = Vec::new();
                for &x in &frontier {
                    for i in 0..self.fanouts[x as usize].len() {
                        let f = self.fanouts[x as usize][i];
                        if !self.in_window(f) && w.tfo.len() < self.opts.max_nodes / 2 {
                            self.mark(f, TFO);
                            w.tfo.push(f);
                            next.push(f);
                        }
                    }
                }
                frontier = next;
            }
            // Fanins outside the fanout part are leaves; they must not depend
            // on `n`.
            let mut ok = true;
            for i in 0..w.tfo.len() {
                let x = w.tfo[i];
                for j in 0..self.fanins[x as usize].len() {
                    let f = self.fanins[x as usize][j];
                    if self.in_window(f) {
                        continue;
                    }
                    if tfo_all.as_ref().is_some_and(|t| t.contains(&f)) {
                        ok = false;
                    }
                    self.mark(f, LEAF);
                    w.leaves.push(f);
                }
            }
            if !ok || w.leaves.len() > self.opts.max_inputs {
                continue;
            }
            self.expand(&mut w);
            self.topo_sort(&mut w.tfo);
            self.topo_sort(&mut w.tfi);
            self.collect_side(&mut w);
            return Some(w);
        }
        None
    }

    /// Expands leaves into their fanins while the window keeps few inputs,
    /// cheapest expansions first.
    fn expand(&mut self, w: &mut Window) {
        loop {
            if w.tfo.len() + w.tfi.len() >= self.opts.max_nodes {
                return;
            }
            let mut best: Option<(usize, usize)> = None;
            for (i, &l) in w.leaves.iter().enumerate() {
                if !self.is_lut(l) {
                    continue;
                }
                let new = self.fanins[l as usize].iter().filter(|&&f| !self.in_window(f)).count();
                if w.leaves.len() - 1 + new <= self.opts.max_inputs
                    && best.is_none_or(|(_, b)| new < b)
                {
                    best = Some((i, new));
                }
            }
            let Some((i, _)) = best else { return };
            let l = w.leaves.swap_remove(i);
            self.mark(l, TFI);
            w.tfi.push(l);
            for j in 0..self.fanins[l as usize].len() {
                let f = self.fanins[l as usize][j];
                if !self.in_window(f) {
                    self.mark(f, LEAF);
                    w.leaves.push(f);
                }
            }
        }
    }

    /// Nodes outside the window whose fanins are all fanin-side nodes of the
    /// window (so not depending on the window's root).
    fn collect_side(&mut self, w: &mut Window) {
        let mut cands: Vec<u32> = Vec::new();
        for &x in w.leaves.iter().chain(&w.tfi) {
            for &f in &self.fanouts[x as usize] {
                if !self.in_window(f) {
                    cands.push(f);
                }
            }
        }
        cands.sort_unstable();
        cands.dedup();
        for f in cands {
            if w.side.len() >= self.opts.max_nodes {
                break;
            }
            let inside = self.fanins[f as usize]
                .iter()
                .all(|&x| self.in_window(x) && matches!(self.role[x as usize], TFI | LEAF));
            if inside {
                w.side.push(f);
            }
        }
        // Side nodes are computed after the inner nodes; mark them last so
        // that none counts as another's fanin.
        for &f in &w.side {
            self.mark(f, SIDE);
        }
    }

    /// Sorts window nodes so that fanins come first.
    fn topo_sort(&self, nodes: &mut Vec<u32>) {
        let set: synth_core::FxHashSet<u32> = nodes.iter().copied().collect();
        let mut done = synth_core::FxHashSet::default();
        let mut out = Vec::with_capacity(nodes.len());
        for &root in nodes.iter() {
            let mut stack = vec![(root, false)];
            while let Some((x, expanded)) = stack.pop() {
                if expanded {
                    out.push(x);
                    continue;
                }
                if !done.insert(x) {
                    continue;
                }
                stack.push((x, true));
                for &f in &self.fanins[x as usize] {
                    if set.contains(&f) && !done.contains(&f) {
                        stack.push((f, false));
                    }
                }
            }
        }
        *nodes = out;
    }

    fn val(&self, n: u32) -> &[u64] {
        let s = self.slot[n as usize] as usize * self.words;
        &self.vals[s..s + self.words]
    }

    /// Evaluates LUT `n` into slot `dst` from the values in the slots of its
    /// fanins.
    fn eval(&mut self, n: u32, dst: usize) {
        let words = self.words;
        let fanins = &self.fanins[n as usize];
        let truth = self.truth[n as usize];
        for w in 0..words {
            let mut out = 0u64;
            for m in 0..(1usize << fanins.len()) {
                if (truth >> m) & 1 == 0 {
                    continue;
                }
                let mut t = !0u64;
                for (j, &f) in fanins.iter().enumerate() {
                    let v = self.vals[self.slot[f as usize] as usize * words + w];
                    t &= if (m >> j) & 1 == 1 { v } else { !v };
                }
                out |= t;
            }
            self.vals[dst * words + w] = out;
        }
    }

    /// Simulates the window exhaustively; returns the care set of its root
    /// (where flipping it changes a window output).
    fn simulate(&mut self, w: &Window) -> Vec<u64> {
        let words = 1usize << w.leaves.len().saturating_sub(6);
        self.words = words;
        let slots = w.leaves.len() + w.tfi.len() + w.side.len() + 2 * w.tfo.len() + 1;
        self.vals.clear();
        self.vals.resize(slots * words, 0);
        let mut next = 1; // slot 0: the constant
        self.slot[0] = 0;
        for (k, &l) in w.leaves.iter().enumerate() {
            if self.kind[l as usize] == Kind::Const {
                self.slot[l as usize] = 0;
                continue;
            }
            self.slot[l as usize] = next as u32;
            for i in 0..words {
                self.vals[next * words + i] = if k < 6 {
                    VAR6[k]
                } else if (i >> (k - 6)) & 1 == 1 {
                    !0
                } else {
                    0
                };
            }
            next += 1;
        }
        for &x in w.tfi.iter().chain(&w.side).chain(&w.tfo) {
            self.slot[x as usize] = next as u32;
            self.eval(x, next);
            next += 1;
        }
        // The fanout part again with the root flipped.
        let root = w.tfo[0];
        let orig: Vec<u32> = w.tfo.iter().map(|&x| self.slot[x as usize]).collect();
        let r = self.slot[root as usize] as usize;
        for i in 0..words {
            self.vals[next * words + i] = !self.vals[r * words + i];
        }
        self.slot[root as usize] = next as u32;
        next += 1;
        for &x in &w.tfo[1..] {
            self.slot[x as usize] = next as u32;
            self.eval(x, next);
            next += 1;
        }
        let mut care = vec![0u64; words];
        for (j, &x) in w.tfo.iter().enumerate() {
            let output = self.po_refs[x as usize] > 0
                || self.fanouts[x as usize]
                    .iter()
                    .any(|&f| !(self.in_window(f) && self.role[f as usize] == TFO));
            if !output {
                continue;
            }
            let (a, b) = (orig[j] as usize, self.slot[x as usize] as usize);
            for (i, c) in care.iter_mut().enumerate() {
                *c |= self.vals[a * words + i] ^ self.vals[b * words + i];
            }
        }
        for (j, &x) in w.tfo.iter().enumerate() {
            self.slot[x as usize] = orig[j];
        }
        care
    }

    /// Minterm masks of `vars` restricted to `care` (bit `j` of the minterm
    /// index is the value of `vars[j]`).
    fn masks(&self, vars: &[u32], care: &[u64]) -> Vec<Vec<u64>> {
        let mut masks = vec![care.to_vec()];
        for &v in vars {
            let val = self.val(v);
            let mut next = Vec::with_capacity(masks.len() * 2);
            for m in &masks {
                next.push(m.iter().zip(val).map(|(a, b)| a & !b).collect());
            }
            for m in &masks {
                next.push(m.iter().zip(val).map(|(a, b)| a & b).collect());
            }
            masks = next;
        }
        masks
    }

    /// A function of the masks' variables equal to `f` wherever a mask is
    /// set, if there is one.
    fn fit(masks: &[Vec<u64>], f: &[u64]) -> Option<u64> {
        let mut t = 0u64;
        for (m, mask) in masks.iter().enumerate() {
            let on = mask.iter().zip(f).any(|(a, b)| a & b != 0);
            let off = mask.iter().zip(f).any(|(a, b)| a & !b != 0);
            if on && off {
                return None;
            }
            if on {
                t |= 1 << m;
            }
        }
        Some(t)
    }

    /// LUTs freed if `n` lost its last reference.
    fn mffc(&self, n: u32, out: &mut Vec<u32>) {
        out.clear();
        if !self.is_lut(n) {
            return;
        }
        let mut refs: FxHashMap<u32, usize> = FxHashMap::default();
        let mut stack = vec![n];
        out.push(n);
        while let Some(x) = stack.pop() {
            for &f in &self.fanins[x as usize] {
                if !self.is_lut(f) {
                    continue;
                }
                let r = refs.entry(f).or_insert_with(|| self.refs(f));
                *r -= 1;
                if *r == 0 {
                    out.push(f);
                    stack.push(f);
                }
            }
        }
    }

    /// Removes an unreferenced LUT and the LUTs left unreferenced.
    fn kill(&mut self, n: u32) -> usize {
        let mut killed = 0;
        let mut stack = vec![n];
        while let Some(x) = stack.pop() {
            if !self.is_lut(x) || self.refs(x) > 0 {
                continue;
            }
            self.kind[x as usize] = Kind::Dead;
            killed += 1;
            for f in std::mem::take(&mut self.fanins[x as usize]) {
                let fo = &mut self.fanouts[f as usize];
                if let Some(p) = fo.iter().position(|&y| y == x) {
                    fo.swap_remove(p);
                }
                stack.push(f);
            }
        }
        killed
    }

    /// Updates arrival times from `n` towards the outputs.
    fn update_arrival(&mut self, n: u32) {
        let mut stack = vec![n];
        while let Some(x) = stack.pop() {
            if !self.is_lut(x) {
                continue;
            }
            let a = self.opts.lut_delay
                + self.fanins[x as usize]
                    .iter()
                    .map(|&f| self.arrival[f as usize])
                    .max()
                    .unwrap_or(0);
            if a != self.arrival[x as usize] {
                self.arrival[x as usize] = a;
                stack.extend_from_slice(&self.fanouts[x as usize]);
            }
        }
    }

    /// Tightens required times from `n` towards the inputs.
    fn tighten_required(&mut self, n: u32, req: u32) {
        let mut stack = vec![(n, req)];
        while let Some((x, r)) = stack.pop() {
            if r >= self.required[x as usize] {
                continue;
            }
            self.required[x as usize] = r;
            if self.is_lut(x) {
                let rf = r.saturating_sub(self.opts.lut_delay);
                stack.extend(self.fanins[x as usize].iter().map(|&f| (f, rf)));
            }
        }
    }

    /// Gives LUT `n` the fanins `vars` and table `t`; returns the LUTs freed.
    fn set_function(&mut self, n: u32, vars: Vec<u32>, t: u64) -> usize {
        let old = std::mem::take(&mut self.fanins[n as usize]);
        for &v in &vars {
            if !old.contains(&v) {
                self.fanouts[v as usize].push(n);
            }
        }
        let rf = self.required[n as usize].saturating_sub(self.opts.lut_delay);
        for &v in &vars {
            self.tighten_required(v, rf);
        }
        self.fanins[n as usize] = vars;
        self.truth[n as usize] = t;
        let mut killed = 0;
        for &f in &old {
            if !self.fanins[n as usize].contains(&f) {
                let fo = &mut self.fanouts[f as usize];
                if let Some(p) = fo.iter().position(|&y| y == n) {
                    fo.swap_remove(p);
                }
                killed += self.kill(f);
            }
        }
        self.update_arrival(n);
        killed
    }

    /// Replaces node `n` by `d` (complemented if `c`) in its fanouts and
    /// outputs; returns the LUTs freed.
    fn replace(&mut self, n: u32, d: u32, c: bool) -> usize {
        let req = self.required[n as usize];
        self.tighten_required(d, req);
        // A constant is folded into the fanouts' tables.
        let konst = self.kind[d as usize] == Kind::Const;
        for f in std::mem::take(&mut self.fanouts[n as usize]) {
            let old = self.fanins[f as usize].clone();
            let mut vars = old.clone();
            vars.retain(|&v| v != n);
            if !konst && !vars.contains(&d) {
                vars.push(d);
            }
            let t = self.truth[f as usize];
            let mut nt = 0u64;
            for m in 0..(1usize << vars.len()) {
                let bit =
                    |v: u32| (m >> vars.iter().position(|&x| x == v).expect("fanin kept")) & 1;
                let mut idx = 0;
                for (j, &v) in old.iter().enumerate() {
                    let b = match v == n {
                        true if konst => c as usize,
                        true => bit(d) ^ c as usize,
                        false => bit(v),
                    };
                    idx |= b << j;
                }
                nt |= ((t >> idx) & 1) << m;
            }
            if !konst && !self.fanins[f as usize].contains(&d) {
                self.fanouts[d as usize].push(f);
            }
            self.fanins[f as usize] = vars;
            self.truth[f as usize] = nt;
            self.update_arrival(f);
        }
        self.po_refs[d as usize] += self.po_refs[n as usize];
        self.po_refs[n as usize] = 0;
        self.replaced[n as usize] = Some((d, c));
        self.kill(n)
    }

    /// Tries to simplify LUT `n`; returns the LUTs freed.
    fn optimize_node(&mut self, n: u32) -> usize {
        let freeable: Vec<u32> = self.fanins[n as usize]
            .iter()
            .copied()
            .filter(|&f| self.is_lut(f) && self.refs(f) == 1)
            .collect();
        let Some(w) = self.window(n) else { return 0 };
        let care = self.simulate(&w);
        let f = self.val(n).to_vec();
        let req = self.required[n as usize];
        let delay = self.opts.lut_delay;
        let mut divisors: Vec<u32> = w
            .leaves
            .iter()
            .chain(&w.tfi)
            .chain(&w.side)
            .copied()
            .filter(|&d| self.arrival[d as usize] <= req)
            .collect();
        divisors.push(0);
        divisors.sort_unstable();
        divisors.dedup();
        let mut mffc = Vec::new();
        // The node equal to another signal on its care set.
        if care.iter().any(|&c| c != !0) {
            self.mffc(n, &mut mffc);
            for &d in &divisors {
                if mffc.contains(&d) {
                    continue;
                }
                let dv = self.val(d);
                for c in [false, true] {
                    let flip = if c { !0 } else { 0 };
                    if f.iter().zip(dv).zip(&care).all(|((a, b), m)| (a ^ b ^ flip) & m == 0) {
                        return self.replace(n, d, c);
                    }
                }
            }
        }
        // A fanin LUT read only here, removed or replaced.
        for x in freeable {
            let rest: Vec<u32> =
                self.fanins[n as usize].iter().copied().filter(|&v| v != x).collect();
            let masks = self.masks(&rest, &care);
            if let Some(t) = Self::fit(&masks, &f) {
                return self.set_function(n, rest, t);
            }
            // Merged with the fanin: a support drawn from both LUTs' fanins.
            if self.role[x as usize] == TFI {
                let mut u = rest.clone();
                for &v in &self.fanins[x as usize] {
                    if !u.contains(&v) {
                        u.push(v);
                    }
                }
                for sel in 1u32..(1 << u.len()) {
                    if sel.count_ones() as usize > self.opts.k || sel >> rest.len() == 0 {
                        continue;
                    }
                    let vars: Vec<u32> =
                        (0..u.len()).filter(|&j| (sel >> j) & 1 == 1).map(|j| u[j]).collect();
                    if let Some(t) = Self::fit(&self.masks(&vars, &care), &f) {
                        return self.set_function(n, vars, t);
                    }
                }
            }
            if rest.len() >= self.opts.k {
                continue;
            }
            self.mffc(x, &mut mffc);
            for &d in &divisors {
                if rest.contains(&d) || mffc.contains(&d) || self.arrival[d as usize] + delay > req
                {
                    continue;
                }
                let mut vars = rest.clone();
                vars.push(d);
                let dv = self.val(d);
                let split: Vec<Vec<u64>> = masks
                    .iter()
                    .map(|m| m.iter().zip(dv).map(|(a, b)| a & !b).collect())
                    .chain(masks.iter().map(|m| m.iter().zip(dv).map(|(a, b)| a & b).collect()))
                    .collect();
                if let Some(t) = Self::fit(&split, &f) {
                    return self.set_function(n, vars, t);
                }
            }
        }
        0
    }
}

/// Optimises mapping `m` of graph `g` (whose outputs may be redirected to
/// equivalent nodes); returns the number of LUTs removed.
pub fn optimize(g: &mut Aig, m: &mut Mapping, opts: &MfsOptions, timing: &Timing) -> usize {
    let mut net = Net {
        opts,
        node: Vec::new(),
        kind: Vec::new(),
        fanins: Vec::new(),
        truth: Vec::new(),
        fanouts: Vec::new(),
        po_refs: Vec::new(),
        arrival: Vec::new(),
        required: Vec::new(),
        replaced: Vec::new(),
        stamp: Vec::new(),
        epoch: 0,
        role: Vec::new(),
        slot: Vec::new(),
        vals: Vec::new(),
        words: 1,
    };
    let mut dense = vec![NONE; g.nodes.len()];
    dense[0] = net.add(0, Kind::Const);
    let source = |net: &mut Net, x: u32, dense: &mut Vec<u32>| -> u32 {
        if dense[x as usize] == NONE {
            let kind = match g.nodes[x as usize] {
                Node::Const => Kind::Const,
                _ => Kind::Input,
            };
            dense[x as usize] = net.add(x, kind);
            if let Node::Input(k) = g.nodes[x as usize] {
                let a = timing.input_arrival.get(k as usize).copied().unwrap_or(0);
                net.arrival[dense[x as usize] as usize] = a;
            }
        }
        dense[x as usize]
    };
    let mut luts = Vec::with_capacity(m.luts.len());
    for l in &m.luts {
        let fanins: Vec<u32> = l.leaves.iter().map(|&x| source(&mut net, x, &mut dense)).collect();
        let d = net.add(l.node, Kind::Lut);
        dense[l.node as usize] = d;
        for &f in &fanins {
            net.fanouts[f as usize].push(d);
        }
        net.arrival[d as usize] =
            opts.lut_delay + fanins.iter().map(|&f| net.arrival[f as usize]).max().unwrap_or(0);
        net.fanins[d as usize] = fanins;
        net.truth[d as usize] = l.truth;
        luts.push(d);
    }
    let outs: Vec<u32> = g.outputs.iter().map(|o| source(&mut net, o.node(), &mut dense)).collect();
    let target = outs.iter().map(|&o| net.arrival[o as usize]).max().unwrap_or(0);
    for (k, &o) in outs.iter().enumerate() {
        net.po_refs[o as usize] += 1;
        let r = timing.output_required.get(k).copied().flatten().unwrap_or(target);
        net.required[o as usize] = net.required[o as usize].min(r);
    }
    for &d in luts.iter().rev() {
        let r = net.required[d as usize].max(net.arrival[d as usize]);
        net.required[d as usize] = r;
        for i in 0..net.fanins[d as usize].len() {
            let f = net.fanins[d as usize][i] as usize;
            net.required[f] = net.required[f].min(r - opts.lut_delay);
        }
    }
    let mut removed = 0;
    for _ in 0..opts.passes {
        let before = removed;
        for &d in &luts {
            if net.is_lut(d) {
                removed += net.optimize_node(d);
            }
        }
        if removed == before {
            break;
        }
    }
    // Outputs of replaced nodes move to their replacements.
    for o in &mut g.outputs {
        let mut x = dense[o.node() as usize];
        let mut c = o.is_compl();
        while let Some((y, cy)) = net.replaced[x as usize] {
            x = y;
            c ^= cy;
        }
        *o = Lit::new(net.node[x as usize], c);
    }
    // The LUTs left, in topological order.
    let mut done = vec![false; net.node.len()];
    let mut out = Vec::new();
    for &root in &luts {
        if !net.is_lut(root) || done[root as usize] {
            continue;
        }
        let mut stack = vec![(root, false)];
        while let Some((x, expanded)) = stack.pop() {
            if expanded {
                out.push(Lut {
                    node: net.node[x as usize],
                    leaves: net.fanins[x as usize].iter().map(|&f| net.node[f as usize]).collect(),
                    truth: net.truth[x as usize],
                });
                continue;
            }
            if done[x as usize] {
                continue;
            }
            done[x as usize] = true;
            stack.push((x, true));
            for &f in &net.fanins[x as usize] {
                if net.is_lut(f) && !done[f as usize] {
                    stack.push((f, false));
                }
            }
        }
    }
    m.luts = out;
    m.depth = g
        .outputs
        .iter()
        .map(|o| {
            let x = dense[o.node() as usize];
            if x == NONE { 0 } else { net.arrival[x as usize] }
        })
        .max()
        .unwrap_or(0);
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lutmap::{MapOptions, map};

    /// Values of the outputs of mapping `m` of `g` on random patterns.
    fn outputs(g: &Aig, m: &Mapping, pats: &[u64]) -> Vec<u64> {
        let mut val: FxHashMap<u32, u64> = FxHashMap::default();
        val.insert(0, 0);
        for (i, &n) in g.inputs.iter().enumerate() {
            val.insert(n, pats[i]);
        }
        for l in &m.luts {
            let mut out = 0u64;
            for bit in 0..64 {
                let mut idx = 0;
                for (j, &leaf) in l.leaves.iter().enumerate() {
                    idx |= ((val[&leaf] >> bit) & 1) << j;
                }
                out |= ((l.truth >> idx) & 1) << bit;
            }
            val.insert(l.node, out);
        }
        g.outputs.iter().map(|o| val[&o.node()] ^ if o.is_compl() { !0 } else { 0 }).collect()
    }

    #[test]
    fn keeps_function_and_depth_of_random_mappings() {
        let mut rng = synth_core::Rng::new(4);
        let mut total = 0;
        for _ in 0..40 {
            let mut g = Aig::new();
            let mut lits: Vec<Lit> = (0..10).map(|_| g.input()).collect();
            for _ in 0..120 {
                let a = lits[rng.below(lits.len())].xor(rng.next_u64() & 1 == 1);
                let b = lits[rng.below(lits.len())].xor(rng.next_u64() & 1 == 1);
                let l = g.and(a, b);
                lits.push(l);
            }
            for l in lits.iter().rev().take(8) {
                g.output(*l);
            }
            let m0 = map(&g, &MapOptions::default());
            let (mut g1, mut m1) = (g.clone(), m0.clone());
            let removed = optimize(&mut g1, &mut m1, &MfsOptions::default(), &Timing::default());
            assert_eq!(m1.luts.len() + removed, m0.luts.len());
            assert!(m1.depth <= m0.depth);
            for _ in 0..4 {
                let pats: Vec<u64> = (0..10).map(|_| rng.next_u64()).collect();
                assert_eq!(outputs(&g, &m0, &pats), outputs(&g1, &m1, &pats));
            }
            total += removed;
        }
        assert!(total > 0);
    }

    #[test]
    fn removes_a_lut_made_redundant_by_dont_cares() {
        // y = (a & b) | (a & b & c) | d...: built so that the mapping keeps a
        // LUT whose input is unobservable: z = s ? (a & b) : x, and the
        // output reads z only where s holds.
        let mut g = Aig::new();
        let x: Vec<Lit> = (0..6).map(|_| g.input()).collect();
        let s = x[0];
        let ab = g.and(x[1], x[2]);
        let t = g.xor(x[3], x[4]);
        let u = g.xor(t, x[5]);
        let z = g.mux(s, ab, u);
        let y = g.and(z, s);
        g.output(y);
        let m0 = map(&g, &MapOptions { k: 3, ..MapOptions::default() });
        let (mut g1, mut m1) = (g.clone(), m0.clone());
        let opts = MfsOptions { k: 3, ..MfsOptions::default() };
        optimize(&mut g1, &mut m1, &opts, &Timing::default());
        let pats: Vec<u64> = {
            let mut rng = synth_core::Rng::new(9);
            (0..6).map(|_| rng.next_u64()).collect()
        };
        assert_eq!(outputs(&g, &m0, &pats), outputs(&g1, &m1, &pats));
        assert!(m1.luts.len() < m0.luts.len(), "{} vs {}", m1.luts.len(), m0.luts.len());
    }
}
