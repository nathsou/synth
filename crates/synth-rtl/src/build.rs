//! Cell construction helpers with on-the-fly constant folding.
//!
//! Frontends build logic through these methods; when all inputs are
//! constant the result is computed directly and no cell is created.

use synth_core::{Logic, LogicVec};

use crate::{BinaryOp, CellKind, Module, Sig, SigBit, UnaryOp, eval};

impl Module {
    fn fresh_name(&self, prefix: &str) -> String {
        format!("${prefix}{}", self.cells.len())
    }

    /// Unary operation. `Not`/`Neg` keep the width; others give 1 bit.
    pub fn unary(&mut self, op: UnaryOp, a: Sig) -> Sig {
        if let Some(c) = a.as_const() {
            return Sig::from_const(&eval::unary(op, &c));
        }
        let width = if matches!(op, UnaryOp::Not | UnaryOp::Neg) { a.width() } else { 1 };
        let y = self.new_sig(width);
        let name = self.fresh_name("unary");
        self.add_cell(name, CellKind::Unary { op, a, y: y.clone() });
        y
    }

    /// Binary operation. Operand widths must follow the cell conventions
    /// (see [`BinaryOp`]).
    pub fn binary(&mut self, op: BinaryOp, signed: bool, a: Sig, b: Sig) -> Sig {
        debug_assert!(
            op.is_logic() || op.is_shift() || a.width() == b.width(),
            "{op:?}: operand widths {} and {}",
            a.width(),
            b.width()
        );
        if let (Some(ca), Some(cb)) = (a.as_const(), b.as_const()) {
            return Sig::from_const(&eval::binary(op, signed, &ca, &cb));
        }
        // Simple identities that matter during process lowering.
        match op {
            BinaryOp::Shl | BinaryOp::Shr | BinaryOp::Sshr
                if b.as_const().and_then(|c| c.to_u64()) == Some(0) =>
            {
                return a;
            }
            BinaryOp::Shl | BinaryOp::Shr if b.is_const() => {
                // Constant shifts are just rewiring.
                let n = b.as_const().unwrap().to_u64().unwrap_or(u64::MAX).min(a.width() as u64)
                    as usize;
                let w = a.width();
                return if op == BinaryOp::Shl {
                    Sig::zeros(n).concat(&a.slice(0, w - n))
                } else {
                    a.slice(n, w - n).concat(&Sig::zeros(n))
                };
            }
            BinaryOp::Sshr if b.is_const() && a.width() > 0 => {
                let n = b.as_const().unwrap().to_u64().unwrap_or(u64::MAX).min(a.width() as u64)
                    as usize;
                let w = a.width();
                let sign = a.0[w - 1];
                return a.slice(n, w - n).concat(&Sig(vec![sign; n]));
            }
            _ => {}
        }
        let width = if op.is_compare() || op.is_logic() { 1 } else { a.width() };
        let y = self.new_sig(width);
        let name = self.fresh_name("binary");
        self.add_cell(name, CellKind::Binary { op, signed, a, b, y: y.clone() });
        y
    }

    /// `s ? b : a`, from procedural code (`if`/`case`).
    pub fn mux(&mut self, s: SigBit, a: Sig, b: Sig) -> Sig {
        self.mux_impl(s, a, b, false)
    }

    /// `s ? b : a`, from a `?:` expression (see [`CellKind::Mux`]).
    pub fn ternary(&mut self, s: SigBit, a: Sig, b: Sig) -> Sig {
        self.mux_impl(s, a, b, true)
    }

    fn mux_impl(&mut self, s: SigBit, a: Sig, b: Sig, ternary: bool) -> Sig {
        debug_assert_eq!(a.width(), b.width());
        match s {
            SigBit::Const(Logic::Zero) => return a,
            SigBit::Const(Logic::One) => return b,
            _ => {}
        }
        if a == b {
            return a;
        }
        if let (Some(ca), Some(cb), Some(cs)) = (a.as_const(), b.as_const(), s.as_const()) {
            return Sig::from_const(&eval::mux(&ca, &cb, cs));
        }
        // Bits equal in both inputs need no multiplexer.
        let width = a.width();
        let diff: Vec<usize> = (0..width).filter(|&i| a.0[i] != b.0[i]).collect();
        if diff.len() < width {
            let sa: Sig = diff.iter().map(|&i| a.0[i]).collect();
            let sb: Sig = diff.iter().map(|&i| b.0[i]).collect();
            let sy = self.mux_impl(s, sa, sb, ternary);
            let mut y = a.clone();
            for (k, &i) in diff.iter().enumerate() {
                y.0[i] = sy.0[k];
            }
            return y;
        }
        // 1-bit special cases: s ? 1 : 0 == s, s ? 0 : 1 == !s. Only for `?:`:
        // with an unknown select, `if` would pick `a` where `s` stays unknown,
        // and the RTL should simulate like its source (synthesis folds these
        // later anyway).
        if width == 1 && ternary {
            match (a.0[0], b.0[0]) {
                (SigBit::Const(Logic::Zero), SigBit::Const(Logic::One)) => return Sig::bit(s),
                (SigBit::Const(Logic::One), SigBit::Const(Logic::Zero)) => {
                    return self.unary(UnaryOp::Not, Sig::bit(s));
                }
                _ => {}
            }
        }
        let y = self.new_sig(width);
        let name = self.fresh_name("mux");
        self.add_cell(name, CellKind::Mux { a, b, s, y: y.clone(), ternary });
        y
    }

    /// Dynamic part select `a[b + offset +: width]`.
    pub fn shiftx(&mut self, a: Sig, b: Sig, b_signed: bool, offset: i64, width: usize) -> Sig {
        if let Some(cb) = b.as_const() {
            return match cb.to_i64(b_signed) {
                Some(v) => {
                    let lo = v + offset;
                    (0..width as i64)
                        .map(|i| {
                            let j = lo + i;
                            if j >= 0 && (j as usize) < a.width() {
                                a.0[j as usize]
                            } else {
                                SigBit::X
                            }
                        })
                        .collect()
                }
                None => Sig::xs(width),
            };
        }
        if let Some(ca) = a.as_const()
            && ca.bits().all(|b| b == ca.get(0))
            && width <= ca.width()
        {
            // Selecting from a uniform constant gives that constant (ignoring
            // out-of-range reads, which are undefined anyway).
            return Sig(vec![SigBit::Const(ca.get(0)); width]);
        }
        let y = self.new_sig(width);
        let name = self.fresh_name("shiftx");
        self.add_cell(name, CellKind::Shiftx { a, b, b_signed, offset, y: y.clone() });
        y
    }

    /// A single-bit logical AND of two 1-bit signals.
    pub fn and1(&mut self, a: SigBit, b: SigBit) -> SigBit {
        match (a, b) {
            (SigBit::Const(Logic::Zero), _) | (_, SigBit::Const(Logic::Zero)) => SigBit::ZERO,
            (SigBit::Const(Logic::One), x) | (x, SigBit::Const(Logic::One)) => x,
            _ if a == b => a,
            _ => self.binary(BinaryOp::And, false, Sig::bit(a), Sig::bit(b)).0[0],
        }
    }

    pub fn or1(&mut self, a: SigBit, b: SigBit) -> SigBit {
        match (a, b) {
            (SigBit::Const(Logic::One), _) | (_, SigBit::Const(Logic::One)) => SigBit::ONE,
            (SigBit::Const(Logic::Zero), x) | (x, SigBit::Const(Logic::Zero)) => x,
            _ if a == b => a,
            _ => self.binary(BinaryOp::Or, false, Sig::bit(a), Sig::bit(b)).0[0],
        }
    }

    pub fn not1(&mut self, a: SigBit) -> SigBit {
        match a {
            SigBit::Const(l) => SigBit::Const(!l),
            _ => self.unary(UnaryOp::Not, Sig::bit(a)).0[0],
        }
    }

    /// `a != 0` as one bit.
    pub fn reduce_bool(&mut self, a: Sig) -> SigBit {
        if a.width() == 1 {
            return a.0[0];
        }
        self.unary(UnaryOp::ReduceOr, a).0[0]
    }

    /// Equality with a constant, as one bit.
    pub fn eq_const(&mut self, a: Sig, c: &LogicVec) -> SigBit {
        let b = Sig::from_const(&c.resize(a.width(), false));
        self.binary(BinaryOp::Eq, false, a, b).0[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding() {
        let mut m = Module::new("t");
        let a = Sig::from_u64(5, 4);
        let b = Sig::from_u64(3, 4);
        assert_eq!(
            m.binary(BinaryOp::Add, false, a.clone(), b).as_const().unwrap().to_u64(),
            Some(8)
        );
        assert!(m.cells.is_empty());
        let w = m.new_sig(4);
        let y = m.binary(BinaryOp::Shl, false, w.clone(), Sig::from_u64(1, 2));
        assert_eq!(y.0[0], SigBit::ZERO);
        assert_eq!(y.0[1], w.0[0]);
        assert!(m.cells.is_empty());
        let s = m.new_sig(1).0[0];
        assert_eq!(m.mux(s, w.clone(), w.clone()), w);
        let one = m.ternary(s, Sig::from_u64(0, 1), Sig::from_u64(1, 1));
        assert_eq!(one.0[0], s);
        // `if` semantics keep the multiplexer (an unknown select picks `a`).
        let n = m.cells.len();
        let one = m.mux(s, Sig::from_u64(0, 1), Sig::from_u64(1, 1));
        assert_ne!(one.0[0], s);
        assert_eq!(m.cells.len(), n + 1);
        let y = m.binary(BinaryOp::Add, false, w.clone(), a);
        assert_eq!(y.width(), 4);
        assert_eq!(m.cells.len(), n + 2);
    }
}
