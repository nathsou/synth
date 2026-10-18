//! Evaluation of combinational cells on constant values (Verilog semantics).

use std::cmp::Ordering;

use synth_core::{Logic, LogicVec};

use crate::{BinaryOp, UnaryOp};

pub fn unary(op: UnaryOp, a: &LogicVec) -> LogicVec {
    match op {
        UnaryOp::Not => a.inv(),
        UnaryOp::Neg => a.neg(),
        UnaryOp::ReduceAnd => LogicVec::from_bits(&[a.reduce_and()]),
        UnaryOp::ReduceOr => LogicVec::from_bits(&[a.reduce_or()]),
        UnaryOp::ReduceXor => LogicVec::from_bits(&[a.reduce_xor()]),
        UnaryOp::ReduceXnor => LogicVec::from_bits(&[!a.reduce_xor()]),
        UnaryOp::LogicNot => LogicVec::from_bits(&[match a.truthy() {
            Some(t) => Logic::from_bool(!t),
            None => Logic::X,
        }]),
    }
}

fn shift_amount(b: &LogicVec) -> Option<usize> {
    if !b.is_known() {
        return None;
    }
    // Amounts beyond the operand width saturate.
    Some(b.to_u64().map_or(usize::MAX, |v| v.min(usize::MAX as u64) as usize))
}

fn bool_vec(v: Option<bool>) -> LogicVec {
    LogicVec::from_bits(&[match v {
        Some(b) => Logic::from_bool(b),
        None => Logic::X,
    }])
}

pub fn binary(op: BinaryOp, signed: bool, a: &LogicVec, b: &LogicVec) -> LogicVec {
    let w = a.width();
    match op {
        BinaryOp::And => a.and(b),
        BinaryOp::Or => a.or(b),
        BinaryOp::Xor => a.xor(b),
        BinaryOp::Xnor => a.xnor(b),
        BinaryOp::Add => a.add(b),
        BinaryOp::Sub => a.sub(b),
        BinaryOp::Mul => a.mul(b),
        BinaryOp::Div => a.div(b, signed),
        BinaryOp::Mod => a.rem(b, signed),
        BinaryOp::Shl => match shift_amount(b) {
            Some(n) => {
                if n >= w {
                    LogicVec::zeros(w)
                } else {
                    a.shl(n)
                }
            }
            None => LogicVec::xs(w),
        },
        BinaryOp::Shr | BinaryOp::Sshr => match shift_amount(b) {
            Some(n) => a.shr(n.min(w), op == BinaryOp::Sshr),
            None => LogicVec::xs(w),
        },
        BinaryOp::Eq => LogicVec::from_bits(&[a.logic_eq(b)]),
        BinaryOp::Ne => LogicVec::from_bits(&[!a.logic_eq(b)]),
        BinaryOp::Eqx => LogicVec::from_bool(a.case_eq(b)),
        BinaryOp::Nex => LogicVec::from_bool(!a.case_eq(b)),
        BinaryOp::Lt => bool_vec(a.cmp(b, signed).map(|o| o == Ordering::Less)),
        BinaryOp::Le => bool_vec(a.cmp(b, signed).map(|o| o != Ordering::Greater)),
        BinaryOp::Gt => bool_vec(a.cmp(b, signed).map(|o| o == Ordering::Greater)),
        BinaryOp::Ge => bool_vec(a.cmp(b, signed).map(|o| o != Ordering::Less)),
        BinaryOp::LogicAnd => bool_vec(match (a.truthy(), b.truthy()) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        }),
        BinaryOp::LogicOr => bool_vec(match (a.truthy(), b.truthy()) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        }),
    }
}

/// `s ? b : a`, merging bitwise when `s` is unknown.
pub fn mux(a: &LogicVec, b: &LogicVec, s: Logic) -> LogicVec {
    match s {
        Logic::Zero => a.clone(),
        Logic::One => b.clone(),
        _ => {
            let mut r = LogicVec::zeros(a.width());
            for i in 0..a.width() {
                let (x, y) = (a.get(i), b.get(i));
                r.set(i, if x == y && x.is_known() { x } else { Logic::X });
            }
            r
        }
    }
}

/// Dynamic part select.
pub fn shiftx(a: &LogicVec, b: &LogicVec, b_signed: bool, offset: i64, width: usize) -> LogicVec {
    match b.to_i64(b_signed) {
        Some(v) => a.slice((v + offset) as isize, width),
        None => LogicVec::xs(width),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shifts_and_compares() {
        let a = LogicVec::from_u64(0b1001, 4);
        assert_eq!(
            binary(BinaryOp::Shl, false, &a, &LogicVec::from_u64(1, 3)).to_u64(),
            Some(0b0010)
        );
        assert_eq!(binary(BinaryOp::Shl, false, &a, &LogicVec::from_u64(7, 3)).to_u64(), Some(0));
        assert_eq!(
            binary(BinaryOp::Sshr, true, &a, &LogicVec::from_u64(2, 3)).to_u64(),
            Some(0b1110)
        );
        assert_eq!(binary(BinaryOp::Lt, true, &a, &LogicVec::from_u64(1, 4)).to_u64(), Some(1));
        assert_eq!(shiftx(&a, &LogicVec::from_u64(3, 4), false, 0, 2).to_bin_string(), "x1");
    }
}
