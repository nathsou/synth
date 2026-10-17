//! And-inverter graphs.

use std::ops::Not;

use synth_core::FxHashMap;

/// An edge: a node index and a complement flag (`node << 1 | compl`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Lit(pub u32);

impl Lit {
    pub const FALSE: Lit = Lit(0);
    pub const TRUE: Lit = Lit(1);

    pub fn new(node: u32, compl: bool) -> Lit {
        Lit(node << 1 | compl as u32)
    }

    pub fn node(self) -> u32 {
        self.0 >> 1
    }

    pub fn is_compl(self) -> bool {
        self.0 & 1 == 1
    }

    /// The positive literal of the same node.
    pub fn regular(self) -> Lit {
        Lit(self.0 & !1)
    }

    pub fn is_const(self) -> bool {
        self.node() == 0
    }

    /// Complements the literal if `c`.
    pub fn xor(self, c: bool) -> Lit {
        Lit(self.0 ^ c as u32)
    }
}

impl Not for Lit {
    type Output = Lit;
    fn not(self) -> Lit {
        Lit(self.0 ^ 1)
    }
}

/// A node of the graph. Node 0 is the constant false.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Const,
    /// The `i`-th primary input.
    Input(u32),
    And(Lit, Lit),
}

/// An and-inverter graph. Nodes are created in topological order (fanins
/// always have smaller indices), and identical AND nodes are shared.
#[derive(Clone, Debug)]
pub struct Aig {
    pub nodes: Vec<Node>,
    /// Node index of each primary input.
    pub inputs: Vec<u32>,
    /// Primary outputs.
    pub outputs: Vec<Lit>,
    strash: FxHashMap<(Lit, Lit), u32>,
}

impl Default for Aig {
    fn default() -> Self {
        Self::new()
    }
}

impl Aig {
    pub fn new() -> Aig {
        Aig {
            nodes: vec![Node::Const],
            inputs: Vec::new(),
            outputs: Vec::new(),
            strash: Default::default(),
        }
    }

    pub fn num_ands(&self) -> usize {
        self.nodes.iter().filter(|n| matches!(n, Node::And(..))).count()
    }

    pub fn input(&mut self) -> Lit {
        let n = self.nodes.len() as u32;
        self.nodes.push(Node::Input(self.inputs.len() as u32));
        self.inputs.push(n);
        Lit::new(n, false)
    }

    pub fn output(&mut self, l: Lit) -> usize {
        self.outputs.push(l);
        self.outputs.len() - 1
    }

    pub fn and(&mut self, a: Lit, b: Lit) -> Lit {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        if a == Lit::FALSE || a == !b {
            return Lit::FALSE;
        }
        if a == Lit::TRUE || a == b {
            return b;
        }
        if let Some(&n) = self.strash.get(&(a, b)) {
            return Lit::new(n, false);
        }
        let n = self.nodes.len() as u32;
        self.nodes.push(Node::And(a, b));
        self.strash.insert((a, b), n);
        Lit::new(n, false)
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

    pub fn or(&mut self, a: Lit, b: Lit) -> Lit {
        !self.and(!a, !b)
    }

    pub fn xor(&mut self, a: Lit, b: Lit) -> Lit {
        if a.is_const() {
            return b.xor(a == Lit::TRUE);
        }
        if b.is_const() {
            return a.xor(b == Lit::TRUE);
        }
        if a == b {
            return Lit::FALSE;
        }
        if a == !b {
            return Lit::TRUE;
        }
        let x = self.and(a, !b);
        let y = self.and(!a, b);
        self.or(x, y)
    }

    /// `s ? t : e`.
    pub fn mux(&mut self, s: Lit, t: Lit, e: Lit) -> Lit {
        if s == Lit::TRUE || t == e {
            return t;
        }
        if s == Lit::FALSE {
            return e;
        }
        if t == !e {
            return self.xor(s, e);
        }
        let x = self.and(s, t);
        let y = self.and(!s, e);
        self.or(x, y)
    }

    /// Majority of three.
    pub fn maj(&mut self, a: Lit, b: Lit, c: Lit) -> Lit {
        let ab = self.and(a, b);
        let ac = self.and(a, c);
        let bc = self.and(b, c);
        let t = self.or(ab, ac);
        self.or(t, bc)
    }

    pub fn and_all(&mut self, lits: &[Lit]) -> Lit {
        self.reduce(lits, Aig::and, Lit::TRUE)
    }

    pub fn or_all(&mut self, lits: &[Lit]) -> Lit {
        self.reduce(lits, Aig::or, Lit::FALSE)
    }

    pub fn xor_all(&mut self, lits: &[Lit]) -> Lit {
        self.reduce(lits, Aig::xor, Lit::FALSE)
    }

    /// Balanced reduction tree.
    fn reduce(&mut self, lits: &[Lit], f: fn(&mut Aig, Lit, Lit) -> Lit, unit: Lit) -> Lit {
        match lits.len() {
            0 => unit,
            1 => lits[0],
            n => {
                let (l, r) = lits.split_at(n / 2);
                let a = self.reduce(l, f, unit);
                let b = self.reduce(r, f, unit);
                f(self, a, b)
            }
        }
    }

    pub fn fanins(&self, n: u32) -> Option<(Lit, Lit)> {
        match self.nodes[n as usize] {
            Node::And(a, b) => Some((a, b)),
            _ => None,
        }
    }

    /// Logic level of every node (inputs and constants at 0).
    pub fn levels(&self) -> Vec<u32> {
        let mut lv = vec![0u32; self.nodes.len()];
        for (i, n) in self.nodes.iter().enumerate() {
            if let Node::And(a, b) = n {
                lv[i] = 1 + lv[a.node() as usize].max(lv[b.node() as usize]);
            }
        }
        lv
    }

    /// Number of references to each node from AND nodes and outputs.
    pub fn fanout_counts(&self) -> Vec<u32> {
        let mut r = vec![0u32; self.nodes.len()];
        for n in &self.nodes {
            if let Node::And(a, b) = n {
                r[a.node() as usize] += 1;
                r[b.node() as usize] += 1;
            }
        }
        for o in &self.outputs {
            r[o.node() as usize] += 1;
        }
        r
    }

    /// Rebuilds the graph keeping only logic reachable from the outputs
    /// (inputs are all kept, in order). Returns the new graph.
    pub fn cleanup(&self) -> Aig {
        let mut used = vec![false; self.nodes.len()];
        for o in &self.outputs {
            used[o.node() as usize] = true;
        }
        for i in (0..self.nodes.len()).rev() {
            if used[i]
                && let Node::And(a, b) = self.nodes[i]
            {
                used[a.node() as usize] = true;
                used[b.node() as usize] = true;
            }
        }
        let mut out = Aig::new();
        let mut map = vec![Lit::FALSE; self.nodes.len()];
        for &n in &self.inputs {
            map[n as usize] = out.input();
        }
        for i in 0..self.nodes.len() {
            if used[i]
                && let Node::And(a, b) = self.nodes[i]
            {
                let fa = map[a.node() as usize].xor(a.is_compl());
                let fb = map[b.node() as usize].xor(b.is_compl());
                map[i] = out.and(fa, fb);
            }
        }
        out.outputs =
            self.outputs.iter().map(|o| map[o.node() as usize].xor(o.is_compl())).collect();
        out
    }

    /// Simulates 64 patterns at once: `inputs[i]` holds the values of input
    /// `i`. Returns the value of every node.
    pub fn simulate(&self, inputs: &[u64]) -> Vec<u64> {
        let mut v = vec![0u64; self.nodes.len()];
        for (i, n) in self.nodes.iter().enumerate() {
            v[i] = match *n {
                Node::Const => 0,
                Node::Input(k) => inputs[k as usize],
                Node::And(a, b) => lit_val(&v, a) & lit_val(&v, b),
            };
        }
        v
    }
}

/// Value of a literal given simulated node values.
pub fn lit_val(v: &[u64], l: Lit) -> u64 {
    let x = v[l.node() as usize];
    if l.is_compl() { !x } else { x }
}

impl Aig {
    /// Writes the graph in the ASCII AIGER format (`aag`), for exchange with
    /// other tools.
    pub fn to_aag(&self) -> String {
        use std::fmt::Write as _;
        // AIGER variables: inputs first, then AND nodes, in order.
        let mut var = vec![0u32; self.nodes.len()];
        let mut next = 1;
        for &n in &self.inputs {
            var[n as usize] = next;
            next += 1;
        }
        for (i, n) in self.nodes.iter().enumerate() {
            if let Node::And(..) = n {
                var[i] = next;
                next += 1;
            }
        }
        let lit = |l: Lit| 2 * var[l.node() as usize] + l.is_compl() as u32;
        let ands = self.num_ands();
        let mut o = String::new();
        let _ =
            writeln!(o, "aag {} {} 0 {} {}", next - 1, self.inputs.len(), self.outputs.len(), ands);
        for &n in &self.inputs {
            let _ = writeln!(o, "{}", 2 * var[n as usize]);
        }
        for &l in &self.outputs {
            let _ = writeln!(o, "{}", lit(l));
        }
        for (i, n) in self.nodes.iter().enumerate() {
            if let Node::And(a, b) = n {
                let _ = writeln!(o, "{} {} {}", 2 * var[i], lit(*a), lit(*b));
            }
        }
        o
    }
}

impl Aig {
    /// Writes the graph in the binary AIGER format (`aig`).
    pub fn to_aiger(&self) -> Vec<u8> {
        let mut var = vec![0u32; self.nodes.len()];
        let mut next = 1;
        for &n in &self.inputs {
            var[n as usize] = next;
            next += 1;
        }
        for (i, n) in self.nodes.iter().enumerate() {
            if let Node::And(..) = n {
                var[i] = next;
                next += 1;
            }
        }
        let lit = |l: Lit| 2 * var[l.node() as usize] + l.is_compl() as u32;
        let mut o = format!(
            "aig {} {} 0 {} {}\n",
            next - 1,
            self.inputs.len(),
            self.outputs.len(),
            self.num_ands()
        )
        .into_bytes();
        for &l in &self.outputs {
            o.extend_from_slice(format!("{}\n", lit(l)).as_bytes());
        }
        let push = |mut x: u32, o: &mut Vec<u8>| {
            while x >= 0x80 {
                o.push((x & 0x7f) as u8 | 0x80);
                x >>= 7;
            }
            o.push(x as u8);
        };
        for (i, n) in self.nodes.iter().enumerate() {
            if let Node::And(a, b) = n {
                let lhs = 2 * var[i];
                let (r0, r1) = {
                    let (x, y) = (lit(*a), lit(*b));
                    if x >= y { (x, y) } else { (y, x) }
                };
                push(lhs - r0, &mut o);
                push(r0 - r1, &mut o);
            }
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_and_simplification() {
        let mut g = Aig::new();
        let a = g.input();
        let b = g.input();
        assert_eq!(g.and(a, Lit::TRUE), a);
        assert_eq!(g.and(a, !a), Lit::FALSE);
        let x = g.and(a, b);
        assert_eq!(g.and(b, a), x);
        let m = g.mux(a, b, !b);
        assert_eq!(m, g.xor(a, !b));
        let s = g.simulate(&[0b1100, 0b1010]);
        assert_eq!(lit_val(&s, x) & 0xf, 0b1000);
        assert_eq!(lit_val(&s, m) & 0xf, 0b1001);
    }

    #[test]
    fn cleanup_removes_dangling() {
        let mut g = Aig::new();
        let a = g.input();
        let b = g.input();
        let c = g.input();
        let _unused = g.and(a, c);
        let x = g.xor(a, b);
        g.output(!x);
        let h = g.cleanup();
        assert_eq!(h.num_ands(), 3);
        assert_eq!(h.inputs.len(), 3);
        let s = h.simulate(&[0b1100, 0b1010, 0]);
        assert_eq!(lit_val(&s, h.outputs[0]) & 0xf, 0b1001);
    }
}
