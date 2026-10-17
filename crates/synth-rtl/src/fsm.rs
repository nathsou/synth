//! State-register analysis.
//!
//! A register whose next value is always a constant or its own value (as
//! for the state register of a state machine) can only hold the values its
//! next-state logic produces, starting from its initial or reset value.
//! Comparisons of such a register with a constant then only need the bits
//! that distinguish that constant from the other reachable values: with a
//! one-hot encoding, a single bit.

use synth_core::{FxHashMap, FxHashSet, Logic, LogicVec};

use crate::{BinaryOp, CellId, CellKind, Module, Sig, SigBit};

const MAX_VALUES: usize = 64;
const MAX_PATHS: usize = 4096;

/// Simplifies equality tests on state registers. Returns the number of
/// comparisons simplified.
pub fn simplify_state_compares(m: &mut Module) -> usize {
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
    // Equality tests against a constant, by the tested signal.
    let mut tests: FxHashMap<Vec<SigBit>, Vec<(CellId, u64, bool)>> = FxHashMap::default();
    for (ci, c) in m.cells.iter_enumerated() {
        if !c.alive {
            continue;
        }
        if let CellKind::Binary { op: op @ (BinaryOp::Eq | BinaryOp::Ne), a, b, .. } = &c.kind {
            let (sig, k) = match (a.as_const(), b.as_const()) {
                (None, Some(k)) => (a, k),
                (Some(k), None) => (b, k),
                _ => continue,
            };
            if let Some(k) = k.to_u64() {
                tests.entry(sig.0.clone()).or_default().push((ci, k, *op == BinaryOp::Ne));
            }
        }
    }
    let regs: Vec<CellId> = m
        .cells
        .iter_enumerated()
        .filter(|(_, c)| {
            c.alive
                && matches!(&c.kind, CellKind::Dff(d)
                    if d.en.is_none() && d.srst.is_none() && (2..=32).contains(&d.q.width())
                        && tests.contains_key(&d.q.0))
        })
        .map(|(ci, _)| ci)
        .collect();
    let mut n = 0;
    for ci in regs {
        let CellKind::Dff(d) = &m.cells[ci].kind else { unreachable!() };
        let d = d.as_ref().clone();
        let Some(values) =
            reachable(m, &mux_of, &d.q, &d.d, &d.init, d.arst.as_ref().map(|r| &r.1))
        else {
            continue;
        };
        for (tc, k, negate) in tests[&d.q.0].clone() {
            let w = d.q.width();
            let r = if !values.contains(&k) {
                SigBit::Const(if negate { Logic::One } else { Logic::Zero })
            } else {
                // Bits telling `k` apart from every other reachable value.
                let others: Vec<u64> = values.iter().copied().filter(|&v| v != k).collect();
                let mut pos: Vec<usize> = Vec::new();
                let mut left = others.clone();
                while !left.is_empty() {
                    // The bit that rules out the most remaining values.
                    let best = (0..w)
                        .filter(|p| !pos.contains(p))
                        .max_by_key(|&p| {
                            left.iter().filter(|&&v| (v >> p) & 1 != (k >> p) & 1).count()
                        })
                        .unwrap();
                    pos.push(best);
                    left.retain(|&v| (v >> best) & 1 == (k >> best) & 1);
                }
                if pos.len() >= w {
                    continue;
                }
                let mut acc = SigBit::ONE;
                for &p in &pos {
                    let b = d.q.0[p];
                    let lit = if (k >> p) & 1 == 1 { b } else { m.not1(b) };
                    acc = m.and1(acc, lit);
                }
                if negate { m.not1(acc) } else { acc }
            };
            let y = m.cells[tc].outputs()[0].clone();
            m.cells[tc].alive = false;
            m.connect(y, Sig::bit(r));
            n += 1;
        }
    }
    n
}

/// The values register `q` (next value `d`) can take, if its next-state
/// logic only selects constants or holds each bit.
fn reachable(
    m: &Module,
    mux_of: &FxHashMap<SigBit, (CellId, usize)>,
    q: &Sig,
    d: &Sig,
    init: &LogicVec,
    reset: Option<&LogicVec>,
) -> Option<FxHashSet<u64>> {
    // Leaf patterns of the next-state trees: per bit 0, 1 or "hold".
    #[derive(Copy, Clone, PartialEq, Eq, Hash)]
    enum Leaf {
        Zero,
        One,
        Hold,
    }
    let mut patterns: FxHashSet<Vec<Leaf>> = FxHashSet::default();
    let mut paths = 0usize;
    // Enumerate jointly, deciding select signals at the roots of the trees.
    fn walk(
        m: &Module,
        mux_of: &FxHashMap<SigBit, (CellId, usize)>,
        q: &Sig,
        cur: Vec<SigBit>,
        assign: &mut Vec<(SigBit, bool)>,
        out: &mut FxHashSet<Vec<Leaf>>,
        paths: &mut usize,
    ) -> Option<()> {
        *paths += 1;
        if *paths > MAX_PATHS {
            return None;
        }
        let mut cur = cur;
        let mut free: Option<SigBit> = None;
        for (j, b) in cur.iter_mut().enumerate() {
            loop {
                if *b == q.0[j] || matches!(b, SigBit::Const(Logic::Zero | Logic::One)) {
                    break;
                }
                let &(ci, i) = mux_of.get(b)?;
                let CellKind::Mux { a, b: bb, s, .. } = &m.cells[ci].kind else { return None };
                match assign.iter().find(|(x, _)| x == s) {
                    Some(&(_, v)) => *b = if v { bb.0[i] } else { a.0[i] },
                    None => {
                        free.get_or_insert(*s);
                        break;
                    }
                }
            }
        }
        match free {
            None => {
                let leaves = cur
                    .iter()
                    .enumerate()
                    .map(|(j, &b)| match b {
                        SigBit::Const(Logic::Zero) => Leaf::Zero,
                        SigBit::Const(Logic::One) => Leaf::One,
                        _ if b == q.0[j] => Leaf::Hold,
                        _ => unreachable!(),
                    })
                    .collect();
                out.insert(leaves);
                Some(())
            }
            Some(s) => {
                for v in [false, true] {
                    assign.push((s, v));
                    let r = walk(m, mux_of, q, cur.clone(), assign, out, paths);
                    assign.pop();
                    r?;
                }
                Some(())
            }
        }
    }
    walk(m, mux_of, q, d.0.clone(), &mut Vec::new(), &mut patterns, &mut paths)?;
    let mut values: FxHashSet<u64> = FxHashSet::default();
    if let Some(r) = reset {
        values.insert(r.to_u64()?);
    }
    match init.to_u64() {
        Some(v) => {
            values.insert(v);
        }
        // An unspecified initial value means nothing before the register is
        // first loaded with a constant: start from those constants.
        None if init.bits().all(|b| !b.is_known()) => {
            for p in &patterns {
                if p.iter().all(|l| *l != Leaf::Hold) {
                    values.insert(
                        p.iter().enumerate().map(|(j, l)| ((*l == Leaf::One) as u64) << j).sum(),
                    );
                }
            }
            if values.is_empty() {
                return None;
            }
        }
        None => return None,
    }
    let mut work: Vec<u64> = values.iter().copied().collect();
    while let Some(v) = work.pop() {
        for p in &patterns {
            let mut next = 0u64;
            for (j, l) in p.iter().enumerate() {
                let bit = match l {
                    Leaf::Zero => 0,
                    Leaf::One => 1,
                    Leaf::Hold => (v >> j) & 1,
                };
                next |= bit << j;
            }
            if values.insert(next) {
                if values.len() > MAX_VALUES {
                    return None;
                }
                work.push(next);
            }
        }
    }
    Some(values)
}
