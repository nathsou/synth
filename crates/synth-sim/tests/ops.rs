//! Every combinational operation agrees with `synth_rtl::eval` on random
//! values, in both modes, at narrow and wide widths.

use synth_core::{Logic, LogicVec, Rng};
use synth_rtl::{BinaryOp, CellKind, Module, PortDir, Sig, UnaryOp, eval};
use synth_sim::{Options, Sim};

fn input(m: &mut Module, name: &str, w: usize, pos: u32) -> Sig {
    let id = m.add_wire(name, w as u32);
    m.wires[id].port = Some((PortDir::Input, pos));
    m.wire_sig(id)
}

fn output(m: &mut Module, name: &str, w: usize, pos: u32) -> Sig {
    let id = m.add_wire(name, w as u32);
    m.wires[id].port = Some((PortDir::Output, pos));
    m.wire_sig(id)
}

fn random(rng: &mut Rng, w: usize, four: bool) -> LogicVec {
    // Mostly known values, with small values and extremes over-represented.
    let bits: Vec<Logic> = match rng.below(6) {
        0 => (0..w).map(|i| Logic::from_bool(i == 0 && rng.below(2) == 0)).collect(),
        1 => (0..w).map(|_| Logic::One).collect(),
        2 => (0..w).map(|i| Logic::from_bool(i + 1 == w)).collect(),
        _ => (0..w)
            .map(|_| {
                if four && rng.below(12) == 0 {
                    if rng.below(2) == 0 { Logic::X } else { Logic::Z }
                } else {
                    Logic::from_bool(rng.below(2) == 1)
                }
            })
            .collect(),
    };
    LogicVec::from_bits(&bits)
}

/// Two-state view of an expected value: unknown bits read as 0.
fn two(v: &LogicVec) -> LogicVec {
    let bits: Vec<Logic> =
        v.bits().map(|b| if b == Logic::One { b } else { Logic::Zero }).collect();
    LogicVec::from_bits(&bits)
}

fn check(
    name: &str,
    m: &Module,
    ins: &[(&str, usize)],
    expect: impl Fn(&[LogicVec]) -> LogicVec,
    rng: &mut Rng,
) {
    for four in [true, false] {
        let mut sim =
            Sim::new(m, &Options { four_state: four, prune: false, optimize: false }).unwrap();
        let sigs: Vec<_> = ins.iter().map(|(n, _)| sim.signal(n).unwrap()).collect();
        let y = sim.signal("y").unwrap();
        for _ in 0..200 {
            let vals: Vec<LogicVec> = ins.iter().map(|(_, w)| random(rng, *w, four)).collect();
            for (s, v) in sigs.iter().zip(&vals) {
                sim.poke_vec(s, v);
            }
            sim.eval();
            let got = sim.peek_vec(&y);
            let mut want = expect(&vals);
            if !four {
                want = two(&want);
            }
            assert_eq!(got, want, "{name} ({}-state) on {vals:?}", if four { 4 } else { 2 });
        }
    }
}

const WIDTHS: [usize; 8] = [1, 3, 8, 31, 63, 64, 65, 130];

#[test]
fn binary_ops() {
    let mut rng = Rng::new(1);
    let ops = [
        BinaryOp::And,
        BinaryOp::Or,
        BinaryOp::Xor,
        BinaryOp::Xnor,
        BinaryOp::Add,
        BinaryOp::Sub,
        BinaryOp::Mul,
        BinaryOp::Div,
        BinaryOp::Mod,
        BinaryOp::Eq,
        BinaryOp::Ne,
        BinaryOp::Eqx,
        BinaryOp::Nex,
        BinaryOp::Lt,
        BinaryOp::Le,
        BinaryOp::Gt,
        BinaryOp::Ge,
        BinaryOp::LogicAnd,
        BinaryOp::LogicOr,
    ];
    for op in ops {
        for signed in [false, true] {
            for w in WIDTHS {
                let mut m = Module::new("t");
                let a = input(&mut m, "a", w, 0);
                let b = input(&mut m, "b", w, 1);
                let yw = if op.is_compare() || op.is_logic() { 1 } else { w };
                let y = output(&mut m, "y", yw, 2);
                let t = m.new_sig(yw);
                m.add_cell("c", CellKind::Binary { op, signed, a, b, y: t.clone() });
                m.connect(y, t);
                // Eqx compares unknown bits literally; in 2-state they are 0.
                check(
                    &format!("{op:?} signed={signed} w={w}"),
                    &m,
                    &[("a", w), ("b", w)],
                    |v| eval::binary(op, signed, &v[0], &v[1]).resize(yw, false),
                    &mut rng,
                );
            }
        }
    }
}

#[test]
fn shifts() {
    let mut rng = Rng::new(2);
    for op in [BinaryOp::Shl, BinaryOp::Shr, BinaryOp::Sshr] {
        for w in WIDTHS {
            for bw in [1, 3, 7, 8, 70] {
                let mut m = Module::new("t");
                let a = input(&mut m, "a", w, 0);
                let b = input(&mut m, "b", bw, 1);
                let y = output(&mut m, "y", w, 2);
                m.add_cell("c", CellKind::Binary { op, signed: false, a, b, y });
                check(
                    &format!("{op:?} w={w} bw={bw}"),
                    &m,
                    &[("a", w), ("b", bw)],
                    |v| eval::binary(op, false, &v[0], &v[1]),
                    &mut rng,
                );
            }
        }
    }
}

#[test]
fn unary_ops() {
    let mut rng = Rng::new(3);
    let ops = [
        UnaryOp::Not,
        UnaryOp::Neg,
        UnaryOp::ReduceAnd,
        UnaryOp::ReduceOr,
        UnaryOp::ReduceXor,
        UnaryOp::ReduceXnor,
        UnaryOp::LogicNot,
    ];
    for op in ops {
        for w in WIDTHS {
            let mut m = Module::new("t");
            let a = input(&mut m, "a", w, 0);
            let yw = if matches!(op, UnaryOp::Not | UnaryOp::Neg) { w } else { 1 };
            let y = output(&mut m, "y", yw, 1);
            m.add_cell("c", CellKind::Unary { op, a, y });
            check(&format!("{op:?} w={w}"), &m, &[("a", w)], |v| eval::unary(op, &v[0]), &mut rng);
        }
    }
}

#[test]
fn muxes() {
    let mut rng = Rng::new(4);
    for ternary in [false, true] {
        for w in WIDTHS {
            let mut m = Module::new("t");
            let a = input(&mut m, "a", w, 0);
            let b = input(&mut m, "b", w, 1);
            let s = input(&mut m, "s", 1, 2);
            let y = output(&mut m, "y", w, 3);
            m.add_cell("c", CellKind::Mux { a, b, s: s.0[0], y, ternary });
            check(
                &format!("mux ternary={ternary} w={w}"),
                &m,
                &[("a", w), ("b", w), ("s", 1)],
                |v| {
                    if !ternary && !v[2].get(0).is_known() {
                        v[0].clone()
                    } else {
                        eval::mux(&v[0], &v[1], v[2].get(0))
                    }
                },
                &mut rng,
            );
        }
    }
}

#[test]
fn pmux() {
    let mut rng = Rng::new(5);
    for w in [1, 8, 64, 100] {
        for n in [1, 3, 64, 70] {
            let mut m = Module::new("t");
            let d = input(&mut m, "d", w, 0);
            let s = input(&mut m, "s", n, 1);
            let cases: Vec<Sig> =
                (0..n).map(|i| input(&mut m, &format!("c{i}"), w, 2 + i as u32)).collect();
            let y = output(&mut m, "y", w, 999);
            m.add_cell("c", CellKind::Pmux { default: d, cases, s, y });
            let names: Vec<String> = (0..n).map(|i| format!("c{i}")).collect();
            let mut ins: Vec<(&str, usize)> = vec![("d", w), ("s", n)];
            ins.extend(names.iter().map(|s| (s.as_str(), w)));
            check(
                &format!("pmux w={w} n={n}"),
                &m,
                &ins,
                |v| {
                    let s = &v[1];
                    let ones: Vec<usize> = (0..n).filter(|&i| s.get(i) == Logic::One).collect();
                    if !s.is_known() || ones.len() > 1 {
                        LogicVec::xs(w)
                    } else if let Some(&i) = ones.first() {
                        v[2 + i].clone()
                    } else {
                        v[0].clone()
                    }
                },
                &mut rng,
            );
        }
    }
}

#[test]
fn shiftx() {
    let mut rng = Rng::new(6);
    for aw in [1, 8, 64, 65, 200] {
        for yw in [1, 5, 64, 70] {
            for b_signed in [false, true] {
                for offset in [-3i64, 0, 7] {
                    let mut m = Module::new("t");
                    let a = input(&mut m, "a", aw, 0);
                    let b = input(&mut m, "b", 8, 1);
                    let y = output(&mut m, "y", yw, 2);
                    m.add_cell("c", CellKind::Shiftx { a, b, b_signed, offset, y });
                    check(
                        &format!("shiftx aw={aw} yw={yw} signed={b_signed} offset={offset}"),
                        &m,
                        &[("a", aw), ("b", 8)],
                        |v| eval::shiftx(&v[0], &v[1], b_signed, offset, yw),
                        &mut rng,
                    );
                }
            }
        }
    }
}

#[test]
fn concatenations_and_slices() {
    // y = {a[2:0], 1'b1, b[70:5], x} through a connection.
    let mut rng = Rng::new(7);
    let mut m = Module::new("t");
    let a = input(&mut m, "a", 8, 0);
    let b = input(&mut m, "b", 80, 1);
    let rhs = Sig::bit(synth_rtl::SigBit::X)
        .concat(&b.slice(5, 66))
        .concat(&Sig::bit(synth_rtl::SigBit::ONE))
        .concat(&a.slice(0, 3));
    let y = output(&mut m, "y", rhs.width(), 2);
    let t = m.new_sig(rhs.width());
    m.add_cell(
        "c",
        CellKind::Binary {
            op: BinaryOp::Or,
            signed: false,
            a: rhs.clone(),
            b: Sig::zeros(rhs.width()),
            y: t.clone(),
        },
    );
    m.connect(y, t);
    check(
        "concat",
        &m,
        &[("a", 8), ("b", 80)],
        |v| {
            let parts = [
                &LogicVec::xs(1),
                &v[1].slice(5, 66),
                &LogicVec::from_bool(true),
                &v[0].slice(0, 3),
            ];
            let c = LogicVec::concat(&parts);
            c.or(&LogicVec::zeros(c.width()))
        },
        &mut rng,
    );
}
