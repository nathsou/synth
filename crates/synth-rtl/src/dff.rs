//! Flip-flop control extraction.
//!
//! The elaborator expresses enables and synchronous resets as multiplexers
//! in front of a plain D flip-flop (`d = en ? x : q`, `d = rst ? 0 : x`).
//! FPGA flip-flops have dedicated clock-enable and set/reset inputs, so
//! these multiplexers are folded into the [`Dff`] controls, bit by bit.

use std::collections::BTreeMap;

use synth_core::{FxHashMap, Logic, LogicVec};

use crate::{CellId, CellKind, Control, Dff, Module, Sig, SigBit};

/// Per-bit result of the analysis.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Ctrl {
    /// Enable terms (all must hold).
    en: Vec<(SigBit, bool)>,
    srst: Option<(SigBit, bool)>,
    en_over_srst: bool,
}

/// Folds enable and synchronous-reset multiplexers into flip-flop controls.
/// Returns the number of flip-flop bits that gained a control.
pub fn extract_controls(m: &mut Module) -> usize {
    // Which mux output bit drives each signal bit.
    let mut mux_of: FxHashMap<SigBit, (CellId, usize)> = FxHashMap::default();
    for (ci, c) in m.cells.iter_enumerated() {
        if c.alive
            && let CellKind::Mux { y, .. } = &c.kind
        {
            for (i, &b) in y.iter().enumerate() {
                mux_of.insert(b, (ci, i));
            }
        }
    }
    let dffs: Vec<CellId> = m
        .cells
        .iter_enumerated()
        .filter(|(_, c)| {
            c.alive && matches!(&c.kind, CellKind::Dff(d) if d.en.is_none() && d.srst.is_none())
        })
        .map(|(ci, _)| ci)
        .collect();
    let mut en_cache: FxHashMap<Vec<(SigBit, bool)>, SigBit> = FxHashMap::default();
    let mut changed = 0;
    for ci in dffs {
        let CellKind::Dff(dff) = &m.cells[ci].kind else { unreachable!() };
        let dff = dff.as_ref().clone();
        let mut groups: BTreeMap<Ctrl, Vec<(usize, SigBit, Logic)>> = BTreeMap::new();
        for j in 0..dff.q.width() {
            let (ctrl, d, val) = analyse(m, &mux_of, dff.q.0[j], dff.d.0[j]);
            if !ctrl.en.is_empty() || ctrl.srst.is_some() {
                changed += 1;
            }
            groups.entry(ctrl).or_default().push((j, d, val));
        }
        if groups.len() == 1
            && groups.keys().next().is_some_and(|c| c.en.is_empty() && c.srst.is_none())
        {
            continue;
        }
        let name = m.cells[ci].name.clone();
        m.cells[ci].alive = false;
        for (ctrl, bits) in groups {
            let en = if ctrl.en.is_empty() {
                None
            } else {
                let sig = *en_cache.entry(ctrl.en.clone()).or_insert_with(|| {
                    let mut acc = SigBit::ONE;
                    for &(s, pol) in &ctrl.en {
                        let t = if pol { s } else { m.not1(s) };
                        acc = m.and1(acc, t);
                    }
                    acc
                });
                Some(Control { sig, pol: true })
            };
            let pick = |s: &Sig| -> Sig { bits.iter().map(|&(j, _, _)| s.0[j]).collect() };
            let pick_v = |v: &LogicVec| {
                LogicVec::from_bits(&bits.iter().map(|&(j, _, _)| v.get(j)).collect::<Vec<_>>())
            };
            let new = Dff {
                clk: dff.clk,
                d: bits.iter().map(|&(_, d, _)| d).collect(),
                q: pick(&dff.q),
                en,
                srst: ctrl.srst.map(|(s, pol)| {
                    (
                        Control { sig: s, pol },
                        LogicVec::from_bits(&bits.iter().map(|b| b.2).collect::<Vec<_>>()),
                    )
                }),
                en_over_srst: ctrl.en_over_srst,
                arst: dff.arst.as_ref().map(|(c, v)| (*c, pick_v(v))),
                init: pick_v(&dff.init),
            };
            m.add_cell(name.clone(), CellKind::Dff(Box::new(new)));
        }
    }
    changed
}

/// Walks the multiplexers in front of flip-flop bit `q` with input `d`.
fn analyse(
    m: &Module,
    mux_of: &FxHashMap<SigBit, (CellId, usize)>,
    q: SigBit,
    mut d: SigBit,
) -> (Ctrl, SigBit, Logic) {
    let mut ctrl = Ctrl { en: Vec::new(), srst: None, en_over_srst: false };
    let mut val = Logic::X;
    for _ in 0..32 {
        let Some(&(ci, i)) = mux_of.get(&d) else { break };
        let CellKind::Mux { a, b, s, .. } = &m.cells[ci].kind else { unreachable!() };
        let (a, b, s) = (a.0[i], b.0[i], *s);
        // A hold: `s ? x : q` or `s ? q : x`. After a reset found inside an
        // enable, further holds stay in the logic.
        let hold = if a == q {
            Some((true, b))
        } else if b == q {
            Some((false, a))
        } else {
            None
        };
        if let Some((pol, next)) = hold {
            if ctrl.en_over_srst {
                break;
            }
            ctrl.en.push((s, pol));
            d = next;
            continue;
        }
        let konst = |x: SigBit| match x {
            SigBit::Const(l @ (Logic::Zero | Logic::One)) => Some(l),
            _ => None,
        };
        if ctrl.srst.is_none() {
            let rst = if let Some(v) = konst(b) {
                Some((true, v, a))
            } else {
                konst(a).map(|v| (false, v, b))
            };
            if let Some((pol, v, next)) = rst {
                ctrl.srst = Some((s, pol));
                ctrl.en_over_srst = !ctrl.en.is_empty();
                val = v;
                d = next;
                continue;
            }
        }
        break;
    }
    (ctrl, d, val)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Clock;

    #[test]
    fn enable_and_reset() {
        let mut m = Module::new("t");
        let clk = m.new_sig(1).0[0];
        let rst = m.new_sig(1).0[0];
        let en = m.new_sig(1).0[0];
        let x = m.new_sig(4);
        let q = m.new_sig(4);
        // d = rst ? 0 : (en ? x : q)
        let held = m.mux(en, q.clone(), x.clone());
        let d = m.mux(rst, held, Sig::zeros(4));
        m.add_cell(
            "ff",
            CellKind::Dff(Box::new(Dff {
                clk: Clock { sig: clk, posedge: true },
                d,
                q: q.clone(),
                en: None,
                srst: None,
                en_over_srst: false,
                arst: None,
                init: LogicVec::xs(4),
            })),
        );
        assert_eq!(extract_controls(&mut m), 4);
        let ff = m
            .cells
            .iter()
            .find_map(|c| match &c.kind {
                CellKind::Dff(d) if c.alive => Some(d.as_ref().clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(ff.d, x);
        assert_eq!(ff.en, Some(Control { sig: en, pol: true }));
        assert_eq!(ff.srst.as_ref().unwrap().0, Control { sig: rst, pol: true });
        assert!(ff.srst.as_ref().unwrap().1.is_zero());
        assert!(!ff.en_over_srst);
    }
}
