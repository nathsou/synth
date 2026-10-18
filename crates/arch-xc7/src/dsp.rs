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
fn multiplier(m: &mut Module, a: Sig, b: Sig, a_signed: bool, b_signed: bool) -> Sig {
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
    ];
    for (name, w, value) in [
        ("ACIN", 30, 0),
        ("BCIN", 18, 0),
        ("C", 48, 0),
        ("D", 25, 0),
        ("PCIN", 48, 0),
        ("ALUMODE", 4, 0),
        ("INMODE", 5, 0),
        ("OPMODE", 7, 5),
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
pub(crate) fn map(m: &mut Module) -> usize {
    let cells: Vec<_> = m.live_cells().collect();
    let mut count = 0;
    for c in cells {
        let CellKind::Binary { op: BinaryOp::Mul, signed, a, b, y } = m.cells[c].kind.clone()
        else {
            continue;
        };
        let width = y.width();
        let (mut a, mut b) =
            (trimmed(&a.extend(width, signed), signed), trimmed(&b.extend(width, signed), signed));
        if a.width() < b.width() {
            std::mem::swap(&mut a, &mut b);
        }
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
                    multiplier(m, ap.clone(), bp, a_signed, b_signed)
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
