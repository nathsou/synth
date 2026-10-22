//! DSP48E1 multiplier inference. Partial products preserve the RTL operation
//! modulo its result width, including two's-complement signed multiplication.
use synth_core::LogicVec;
use synth_rtl::{Attr, BinaryOp, CellKind, Instance, Module, PortDir, Sig, SigBit};

pub(crate) const REGISTERS: &[&str] = &[
    "ACASCREG",
    "ADREG",
    "ALUMODEREG",
    "AREG",
    "BCASCREG",
    "BREG",
    "CARRYINREG",
    "CARRYINSELREG",
    "CREG",
    "DREG",
    "INMODEREG",
    "MREG",
    "OPMODEREG",
    "PREG",
];

fn trimmed(s: &Sig, signed: bool) -> Sig {
    let mut n = s.width();
    while n > 1 && if signed { s.0[n - 1] == s.0[n - 2] } else { s.0[n - 1] == SigBit::ZERO } {
        n -= 1;
    }
    s.slice(0, n)
}
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
/// A DSP48E1 computing `a * b`, plus `c` (48 bits) through the post-adder.
fn multiplier(
    m: &mut Module,
    a: Sig,
    b: Sig,
    a_signed: bool,
    b_signed: bool,
    c: Option<Sig>,
) -> Sig {
    let p = m.new_sig(48);
    let mut params: Vec<_> =
        REGISTERS.iter().map(|&k| (k.into(), Attr::Const(LogicVec::from_u64(0, 32)))).collect();
    for (k, v) in [
        ("A_INPUT", "DIRECT"),
        ("B_INPUT", "DIRECT"),
        ("USE_DPORT", "FALSE"),
        ("USE_MULT", "MULTIPLY"),
        ("USE_SIMD", "ONE48"),
        ("USE_PATTERN_DETECT", "NO_PATDET"),
    ] {
        params.push((k.into(), Attr::Str(v.into())));
    }
    let mut conns = vec![
        ("P".into(), PortDir::Output, p.clone()),
        ("A".into(), PortDir::Input, a.extend(30, a_signed)),
        ("B".into(), PortDir::Input, b.extend(18, b_signed)),
        ("C".into(), PortDir::Input, c.clone().unwrap_or_else(|| Sig::zeros(48))),
    ];
    // X = Y = M (the product), Z = C or zero.
    let opmode = if c.is_some() { 0b011_0101 } else { 0b000_0101 };
    for (name, w, value) in [
        ("ACIN", 30, 0),
        ("BCIN", 18, 0),
        ("D", 25, 0),
        ("PCIN", 48, 0),
        ("ALUMODE", 4, 0),
        ("INMODE", 5, 0),
        ("OPMODE", 7, opmode),
        ("CARRYINSEL", 3, 0),
    ] {
        conns.push((name.into(), PortDir::Input, Sig::from_const(&LogicVec::from_u64(value, w))));
    }
    for pin in [
        "CARRYCASCIN",
        "CARRYIN",
        "MULTSIGNIN",
        "CLK",
        "CEA1",
        "CEA2",
        "CEAD",
        "CEALUMODE",
        "CEB1",
        "CEB2",
        "CEC",
        "CECARRYIN",
        "CECTRL",
        "CED",
        "CEINMODE",
        "CEM",
        "CEP",
        "RSTA",
        "RSTALLCARRYIN",
        "RSTALUMODE",
        "RSTB",
        "RSTC",
        "RSTCTRL",
        "RSTD",
        "RSTINMODE",
        "RSTM",
        "RSTP",
    ] {
        conns.push((pin.into(), PortDir::Input, Sig::zeros(1)));
    }
    m.add_cell(
        format!("$dsp{}", m.cells.len()),
        CellKind::Instance(Box::new(Instance { module: "DSP48E1".into(), params, conns })),
    );
    p
}
fn chunks(s: &Sig, capacity: usize, signed: bool) -> Vec<(usize, Sig, bool)> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < s.width() {
        let rest = s.width() - offset;
        let n = if signed && rest <= capacity { rest } else { rest.min(capacity - 1) };
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

/// Folds additions of a product into the post-adder of the product's DSP:
/// `x * y + z` is one DSP48E1 computing `P = A * B + C` when the product
/// has no other use and fits one multiplier, and the sum at most 48 bits.
fn fold_additions(m: &mut Module) -> usize {
    let uses = synth_rtl::opt::use_counts(m);
    let mut products = synth_core::FxHashMap::default();
    for c in m.live_cells() {
        if let CellKind::Binary { op: BinaryOp::Mul, y, .. } = &m.cells[c].kind
            && y.iter().all(|b| matches!(b, SigBit::Wire(..)) && uses.get(b) == Some(&1))
        {
            products.insert(y.clone(), c);
        }
    }
    let mut count = 0;
    let adds: Vec<_> = m.live_cells().collect();
    for c in adds {
        let CellKind::Binary { op: BinaryOp::Add, a, b, y, .. } = m.cells[c].kind.clone() else {
            continue;
        };
        if y.width() > 48 {
            continue;
        }
        // An operand that is a product, or an untruncated product extended.
        let found = [(&a, &b), (&b, &a)].into_iter().find_map(|(x, z)| {
            (1..=x.width()).rev().find_map(|k| {
                let &p = products.get(&x.slice(0, k))?;
                Some((p, k, x, z))
            })
        });
        let Some((p, k, operand, addend)) = found else { continue };
        if !m.cells[p].alive {
            continue;
        }
        let CellKind::Binary { signed, a: x, b: z, y: product, .. } = m.cells[p].kind.clone()
        else {
            continue;
        };
        let (x, z) = operands(&x, &z, product.width(), signed);
        if !useful(&x, &z) || chunks(&x, 25, signed).len() > 1 || chunks(&z, 18, signed).len() > 1 {
            continue;
        }
        let top = operand.0[k - 1];
        let extension =
            operand.0[k..].iter().all(|&e| e == if signed { top } else { SigBit::ZERO });
        if k < operand.width() && !(extension && k >= x.width() + z.width()) {
            continue;
        }
        // Sums are modulo the result width, so any extension of the
        // addend to 48 bits will do.
        let sum = multiplier(m, x, z, signed, signed, Some(addend.extend(48, false)));
        m.cells[p].alive = false;
        m.cells[c].alive = false;
        m.connect(y.clone(), sum.slice(0, y.width()));
        products.remove(&product);
        count += 1;
    }
    count
}

pub(crate) fn map(m: &mut Module) -> usize {
    let mut count = fold_additions(m);
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
        let mut partials = Vec::new();
        for (ai, ap, a_signed) in chunks(&a, 25, signed) {
            for (bi, bp, b_signed) in chunks(&b, 18, signed) {
                if ai + bi >= width {
                    continue;
                }
                if ap.as_const().is_some_and(|v| v.to_u64() == Some(0))
                    || bp.as_const().is_some_and(|v| v.to_u64() == Some(0))
                {
                    continue;
                }
                let bits = width - ai - bi;
                // A narrow signed high chunk still benefits from a DSP: logic
                // lowering otherwise multiplies sign-extended result-width
                // operands, obscuring the small partial product.
                let p = if useful(&ap, &bp) || a_signed || b_signed {
                    count += 1;
                    multiplier(m, ap.clone(), bp, a_signed, b_signed, None)
                        .extend(bits, a_signed || b_signed)
                } else {
                    m.binary(
                        BinaryOp::Mul,
                        a_signed || b_signed,
                        ap.extend(bits, a_signed),
                        bp.extend(bits, b_signed),
                    )
                };
                partials.push(Sig::zeros(ai + bi).concat(&p).extend(width, a_signed || b_signed));
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
