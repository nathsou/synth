//! And-inverter graphs under in-place editing: reference counts, fanout
//! lists, logic levels and node replacement, for local rewriting passes.
//!
//! Unlike [`Aig`], node indices are not topological once nodes have been
//! replaced; [`EditAig::to_aig`] restores a topological, compact graph.

use synth_core::FxHashMap;

use crate::aig::{Aig, Lit, Node};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Kind {
    Const,
    Input(u32),
    And(Lit, Lit),
    Dead,
}

/// An editable and-inverter graph.
#[derive(Clone, Debug)]
pub struct EditAig {
    kind: Vec<Kind>,
    /// References from live AND nodes and outputs.
    refs: Vec<u32>,
    /// AND nodes using each node.
    fanouts: Vec<Vec<u32>>,
    level: Vec<u32>,
    strash: FxHashMap<(Lit, Lit), u32>,
    inputs: Vec<u32>,
    outputs: Vec<Lit>,
}

impl EditAig {
    pub fn from_aig(g: &Aig) -> EditAig {
        let n = g.nodes.len();
        let mut e = EditAig {
            kind: Vec::with_capacity(n),
            refs: Vec::with_capacity(n),
            fanouts: Vec::with_capacity(n),
            level: Vec::with_capacity(n),
            strash: FxHashMap::default(),
            inputs: g.inputs.clone(),
            outputs: Vec::new(),
        };
        let mut map = vec![Lit::FALSE; n];
        for (i, nd) in g.nodes.iter().enumerate() {
            map[i] = match *nd {
                Node::Const => {
                    e.push(Kind::Const, 0);
                    Lit::FALSE
                }
                Node::Input(k) => Lit::new(e.push(Kind::Input(k), 0), false),
                Node::And(a, b) => {
                    let fa = map[a.node() as usize].xor(a.is_compl());
                    let fb = map[b.node() as usize].xor(b.is_compl());
                    e.and(fa, fb)
                }
            };
        }
        for (k, &i) in g.inputs.iter().enumerate() {
            e.inputs[k] = map[i as usize].node();
        }
        for &o in &g.outputs {
            let l = map[o.node() as usize].xor(o.is_compl());
            e.add_output(l);
        }
        e.sweep();
        e
    }

    fn push(&mut self, k: Kind, level: u32) -> u32 {
        let n = self.kind.len() as u32;
        self.kind.push(k);
        self.refs.push(0);
        self.fanouts.push(Vec::new());
        self.level.push(level);
        n
    }

    /// Number of node slots (live or dead).
    pub fn len(&self) -> usize {
        self.kind.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kind.len() <= 1
    }

    pub fn num_ands(&self) -> usize {
        self.kind.iter().filter(|k| matches!(k, Kind::And(..))).count()
    }

    pub fn is_and(&self, n: u32) -> bool {
        matches!(self.kind[n as usize], Kind::And(..))
    }

    pub fn is_dead(&self, n: u32) -> bool {
        self.kind[n as usize] == Kind::Dead
    }

    pub fn fanins(&self, n: u32) -> Option<(Lit, Lit)> {
        match self.kind[n as usize] {
            Kind::And(a, b) => Some((a, b)),
            _ => None,
        }
    }

    pub fn refs(&self, n: u32) -> u32 {
        self.refs[n as usize]
    }

    pub fn level(&self, n: u32) -> u32 {
        self.level[n as usize]
    }

    pub fn outputs(&self) -> &[Lit] {
        &self.outputs
    }

    /// The node `and(a, b)` would return, if it exists without creating
    /// one.
    pub fn find_and(&self, a: Lit, b: Lit) -> Option<Lit> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        if a == Lit::FALSE || a == !b {
            return Some(Lit::FALSE);
        }
        if a == Lit::TRUE || a == b {
            return Some(b);
        }
        self.strash.get(&(a, b)).map(|&n| Lit::new(n, false))
    }

    /// The AND of two literals, created (unreferenced) if it does not exist.
    pub fn and(&mut self, a: Lit, b: Lit) -> Lit {
        if let Some(l) = self.find_and(a, b) {
            return l;
        }
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let lv = 1 + self.level[a.node() as usize].max(self.level[b.node() as usize]);
        let n = self.push(Kind::And(a, b), lv);
        self.strash.insert((a, b), n);
        for f in [a, b] {
            self.refs[f.node() as usize] += 1;
            self.fanouts[f.node() as usize].push(n);
        }
        Lit::new(n, false)
    }

    fn add_output(&mut self, l: Lit) {
        self.refs[l.node() as usize] += 1;
        self.outputs.push(l);
    }

    /// Deletes unreferenced AND nodes, and the logic only they used.
    pub fn sweep(&mut self) {
        for n in 0..self.kind.len() as u32 {
            if self.refs[n as usize] == 0 && self.is_and(n) {
                self.delete(n);
            }
        }
    }

    /// Deletes an unreferenced AND node and, recursively, fanins left
    /// unreferenced.
    fn delete(&mut self, n: u32) {
        let mut stack = vec![n];
        while let Some(x) = stack.pop() {
            let Kind::And(a, b) = self.kind[x as usize] else { continue };
            debug_assert_eq!(self.refs[x as usize], 0);
            if self.strash.get(&(a, b)) == Some(&x) {
                self.strash.remove(&(a, b));
            }
            self.kind[x as usize] = Kind::Dead;
            for f in [a.node(), b.node()] {
                let fo = &mut self.fanouts[f as usize];
                if let Some(p) = fo.iter().position(|&y| y == x) {
                    fo.swap_remove(p);
                }
                self.refs[f as usize] -= 1;
                if self.refs[f as usize] == 0 && self.is_and(f) {
                    stack.push(f);
                }
            }
        }
    }

    /// Replaces every use of node `old` by `new` (which must not depend on
    /// `old`), merging fanouts that become structurally identical to other
    /// nodes, and deletes the logic left unused.
    pub fn replace(&mut self, old: u32, new: Lit) {
        // Each pending replacement holds a reference to its new literal.
        self.refs[new.node() as usize] += 1;
        let mut work = vec![(old, new)];
        let mut touched = Vec::new();
        while let Some((old, new)) = work.pop() {
            let nn = new.node();
            if self.is_and(old) && nn != old {
                for o in &mut self.outputs {
                    if o.node() == old {
                        *o = new.xor(o.is_compl());
                        self.refs[old as usize] -= 1;
                        self.refs[nn as usize] += 1;
                    }
                }
                for f in std::mem::take(&mut self.fanouts[old as usize]) {
                    let Kind::And(a, b) = self.kind[f as usize] else { continue };
                    if self.strash.get(&(a, b)) == Some(&f) {
                        self.strash.remove(&(a, b));
                    }
                    let sub = |l: Lit| if l.node() == old { new.xor(l.is_compl()) } else { l };
                    let (na, nb) = (sub(a), sub(b));
                    let (na, nb) = if na <= nb { (na, nb) } else { (nb, na) };
                    self.refs[old as usize] -= 1;
                    self.refs[nn as usize] += 1;
                    self.fanouts[nn as usize].push(f);
                    self.kind[f as usize] = Kind::And(na, nb);
                    match self.find_and(na, nb) {
                        // The fanout now duplicates a node or a literal: it
                        // is replaced in turn.
                        Some(l) => {
                            self.refs[l.node() as usize] += 1;
                            work.push((f, l));
                        }
                        None => {
                            self.strash.insert((na, nb), f);
                            touched.push(f);
                        }
                    }
                }
            }
            self.refs[nn as usize] -= 1;
            if self.refs[old as usize] == 0 && self.is_and(old) {
                self.delete(old);
            }
            if self.refs[nn as usize] == 0 && self.is_and(nn) {
                self.delete(nn);
            }
        }
        self.update_levels(touched);
    }

    /// Recomputes the levels of `nodes` and of their transitive fanouts
    /// where they change.
    fn update_levels(&mut self, mut nodes: Vec<u32>) {
        while let Some(n) = nodes.pop() {
            let Kind::And(a, b) = self.kind[n as usize] else { continue };
            let lv = 1 + self.level[a.node() as usize].max(self.level[b.node() as usize]);
            if lv != self.level[n as usize] {
                self.level[n as usize] = lv;
                nodes.extend_from_slice(&self.fanouts[n as usize]);
            }
        }
    }

    /// Live AND nodes in topological order.
    pub fn topo_order(&self) -> Vec<u32> {
        let n = self.kind.len();
        let mut state = vec![0u8; n];
        let mut order = Vec::new();
        let mut stack: Vec<(u32, bool)> = Vec::new();
        for root in 0..n as u32 {
            if !self.is_and(root) || state[root as usize] != 0 {
                continue;
            }
            stack.push((root, false));
            while let Some((x, done)) = stack.pop() {
                if done {
                    state[x as usize] = 2;
                    order.push(x);
                    continue;
                }
                if state[x as usize] != 0 {
                    continue;
                }
                state[x as usize] = 1;
                stack.push((x, true));
                if let Kind::And(a, b) = self.kind[x as usize] {
                    for f in [a.node(), b.node()] {
                        if state[f as usize] == 0 && self.is_and(f) {
                            stack.push((f, false));
                        }
                    }
                }
            }
        }
        order
    }

    /// Adds a temporary reference to node `n` (dereferencing a cone stops
    /// at it).
    pub fn ref_leaf(&mut self, n: u32) {
        self.refs[n as usize] += 1;
    }

    /// Removes a reference added by [`EditAig::ref_leaf`].
    pub fn deref_leaf(&mut self, n: u32) {
        self.refs[n as usize] -= 1;
    }

    /// Size of the maximum fanout-free cone of `n` (the AND nodes deleted
    /// along with it), leaving reference counts unchanged.
    pub fn mffc_size(&mut self, n: u32) -> u32 {
        let s = self.deref_cone(n);
        self.ref_cone(n);
        s
    }

    /// Dereferences the fanout-free cone of `n` (as if `n` lost its last
    /// reference); returns its size. Undo with [`EditAig::ref_cone`].
    pub fn deref_cone(&mut self, n: u32) -> u32 {
        let Kind::And(a, b) = self.kind[n as usize] else { return 0 };
        let mut size = 1;
        for f in [a.node(), b.node()] {
            self.refs[f as usize] -= 1;
            if self.refs[f as usize] == 0 && self.is_and(f) {
                size += self.deref_cone(f);
            }
        }
        size
    }

    /// Undoes [`EditAig::deref_cone`].
    pub fn ref_cone(&mut self, n: u32) -> u32 {
        let Kind::And(a, b) = self.kind[n as usize] else { return 0 };
        let mut size = 1;
        for f in [a.node(), b.node()] {
            if self.refs[f as usize] == 0 && self.is_and(f) {
                size += self.ref_cone(f);
            }
            self.refs[f as usize] += 1;
        }
        size
    }

    /// The compacted graph: live nodes in topological order, inputs and
    /// outputs in their original order.
    pub fn to_aig(&self) -> Aig {
        let mut out = Aig::new();
        let mut map = vec![Lit::FALSE; self.kind.len()];
        for &i in &self.inputs {
            map[i as usize] = out.input();
        }
        for n in self.topo_order() {
            if let Kind::And(a, b) = self.kind[n as usize] {
                let fa = map[a.node() as usize].xor(a.is_compl());
                let fb = map[b.node() as usize].xor(b.is_compl());
                map[n as usize] = out.and(fa, fb);
            }
        }
        out.outputs =
            self.outputs.iter().map(|o| map[o.node() as usize].xor(o.is_compl())).collect();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_merges_and_cleans_up() {
        // y = (a & b) & c and z = (a & b) & !c; replacing a & b by a makes
        // the fanouts a & c and a & !c, and frees the old AND.
        let mut g = Aig::new();
        let a = g.input();
        let b = g.input();
        let c = g.input();
        let ab = g.and(a, b);
        let y = g.and(ab, c);
        let z = g.and(ab, !c);
        let ac = g.and(a, c);
        g.output(y);
        g.output(z);
        g.output(ac);
        let mut e = EditAig::from_aig(&g);
        assert_eq!(e.num_ands(), 4);
        assert_eq!(e.mffc_size(ab.node()), 1);
        e.replace(ab.node(), a);
        // y merges into the existing a & c.
        assert_eq!(e.num_ands(), 2);
        let h = e.to_aig();
        assert_eq!(h.num_ands(), 2);
        assert_eq!(h.outputs[0], h.outputs[2]);
    }
}
