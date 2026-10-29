//! Multiplier (`MULT18X18D`) inference: products of operands wide enough
//! to be worth it become 18 × 18 multipliers without registers, each
//! operand chunk signed where it is the high chunk of a signed operand
//! (`SIGNEDA`, `SIGNEDB`), the partial products added in logic. Partial
//! products preserve the RTL operation modulo its result width, including
//! two's-complement signed multiplication.

use synth_rtl::{BinaryOp, CellKind, Instance, Module, PortDir, Sig, SigBit};

use crate::cells::MULT18X18D;

/// Operand bits of a multiplier.
const WIDTH: usize = 18;

/// `s` without redundant high bits (copies of the sign, or zeros).
fn trimmed(s: &Sig, signed: bool) -> Sig {
    let mut n = s.width();
    while n > 1 && if signed { s.0[n - 1] == s.0[n - 2] } else { s.0[n - 1] == SigBit::ZERO } {
        n -= 1;
    }
    s.slice(0, n)
}

/// Whether a product is worth a multiplier rather than logic: both
/// operands of four bits or more, together of 18 or more, and neither a
/// constant of two set bits or fewer (shifts and adds).
fn useful(a: &Sig, b: &Sig) -> bool {
    if a.width().min(b.width()) < 4 || a.width() + b.width() < 18 {
        return false;
    }
    for s in [a, b] {
        if let Some(v) = s.as_const().and_then(|v| v.to_u64())
            && v.count_ones() <= 2
        {
            return false;
        }
    }
    true
}

/// A `MULT18X18D` computing the 36-bit product `a * b`.
fn multiplier(m: &mut Module, a: Sig, b: Sig, a_signed: bool, b_signed: bool) -> Sig {
    let p = m.new_sig(36);
    let mut conns: Vec<(String, PortDir, Sig)> = Vec::new();
    for (name, s, signed) in [("A", a, a_signed), ("B", b, b_signed)] {
        for (i, &bit) in s.extend(WIDTH, signed).0.iter().enumerate() {
            conns.push((format!("{name}{i}"), PortDir::Input, Sig::bit(bit)));
        }
        let flag = if signed { SigBit::ONE } else { SigBit::ZERO };
        conns.push((format!("SIGNED{name}"), PortDir::Input, Sig::bit(flag)));
    }
    for (i, &bit) in p.0.iter().enumerate() {
        conns.push((format!("P{i}"), PortDir::Output, Sig::bit(bit)));
    }
    m.add_cell(
        format!("$mult18_{}", m.cells.len()),
        CellKind::Instance(Box::new(Instance {
            module: MULT18X18D.into(),
            params: Vec::new(),
            conns,
        })),
    );
    p
}

/// `s` in chunks of at most 18 bits (offset, bits, signed): only the high
/// chunk of a signed operand is signed.
fn chunks(s: &Sig, signed: bool) -> Vec<(usize, Sig, bool)> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < s.width() {
        let n = (s.width() - offset).min(WIDTH);
        let sign = signed && offset + n == s.width();
        out.push((offset, trimmed(&s.slice(offset, n), sign), sign));
        offset += n;
    }
    out
}

/// The operands of multiplication `y = a * b` (`width` bits), widest first,
/// trimmed of redundant high bits.
fn operands(a: &Sig, b: &Sig, width: usize, signed: bool) -> (Sig, Sig) {
    let (a, b) =
        (trimmed(&a.extend(width, signed), signed), trimmed(&b.extend(width, signed), signed));
    if a.width() < b.width() { (b, a) } else { (a, b) }
}

/// Maps the multiplications of `m` to multipliers, at most `budget` of
/// them (the others stay logic); returns the number used.
pub fn map(m: &mut Module, budget: Option<usize>) -> usize {
    let mut count = 0;
    let cells: Vec<_> = m.live_cells().collect();
    for c in cells {
        let CellKind::Binary { op: BinaryOp::Mul, signed, a, b, y } = m.cells[c].kind.clone()
        else {
            continue;
        };
        let width = y.width();
        let (a, b) = operands(&a, &b, width, signed);
        if !useful(&a, &b) {
            continue;
        }
        let (ca, cb) = (chunks(&a, signed), chunks(&b, signed));
        // The multipliers this product takes: its chunk pairs below the
        // result's width.
        let needed = ca
            .iter()
            .flat_map(|x| cb.iter().map(move |y| (x, y)))
            .filter(|(x, y)| x.0 + y.0 < width && useful_pair(&x.1, &y.1, x.2 || y.2))
            .count();
        if budget.is_some_and(|b| count + needed > b) {
            continue;
        }
        let mut partials = Vec::new();
        for (ai, ap, a_signed) in &ca {
            for (bi, bp, b_signed) in &cb {
                if ai + bi >= width {
                    continue;
                }
                let zero = |s: &Sig| s.as_const().is_some_and(|v| v.to_u64() == Some(0));
                if zero(ap) || zero(bp) {
                    continue;
                }
                let bits = width - ai - bi;
                let any_signed = *a_signed || *b_signed;
                let p = if useful_pair(ap, bp, any_signed) {
                    count += 1;
                    multiplier(m, ap.clone(), bp.clone(), *a_signed, *b_signed)
                        .extend(bits, any_signed)
                } else {
                    m.binary(
                        BinaryOp::Mul,
                        any_signed,
                        ap.extend(bits, *a_signed),
                        bp.extend(bits, *b_signed),
                    )
                };
                partials.push(Sig::zeros(ai + bi).concat(&p).extend(width, any_signed));
            }
        }
        while partials.len() > 1 {
            let mut next = Vec::new();
            for pair in partials.chunks(2) {
                next.push(if pair.len() == 2 {
                    m.binary(BinaryOp::Add, false, pair[0].clone(), pair[1].clone())
                } else {
                    pair[0].clone()
                });
            }
            partials = next;
        }
        let product = partials.pop().unwrap_or_else(|| Sig::zeros(width));
        m.cells[c].alive = false;
        m.connect(y, product);
    }
    count
}

/// Whether a partial product takes a multiplier: when worth one, or when
/// signed (logic would multiply operands sign-extended to the result's
/// width, obscuring the small product).
fn useful_pair(a: &Sig, b: &Sig, signed: bool) -> bool {
    useful(a, b) || signed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_operands_split_with_only_the_high_chunk_signed() {
        let mut m = Module::new("t");
        let w = m.add_wire("a", 40);
        let a = Sig::wire(w, 40);
        let c = chunks(&a, true);
        assert_eq!(
            c.iter().map(|c| (c.0, c.1.width(), c.2)).collect::<Vec<_>>(),
            [(0, 18, false), (18, 18, false), (36, 4, true)]
        );
    }
}
