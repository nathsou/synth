//! Word-level multiplexers from bit-level ones.
//!
//! Process lowering builds each bit of a register's next value as its own
//! chain of one-bit multiplexers. Simulated as they are, every bit pays for
//! its own instructions and the bits are gathered back into words. A
//! multiplexer is bitwise, so one-bit multiplexers with the same select can
//! be one wide multiplexer: this pass merges those at the same depth (so
//! that no merged cell feeds another of its own group), ordering each group
//! after where its inputs sit in groups already merged, so that the next
//! level reads whole words.
//!
//! Registers are bit-blasted too (one flip-flop per bit, each with the same
//! clock and controls): [`merge_flops`] puts those back together.

use synth_core::{FxHashMap, LogicVec};
use synth_rtl::{CellId, CellKind, Clock, Control, Dff, Module, Sig, SigBit};

/// Merges one-bit `Mux` and `Pmux` cells; returns the number merged away.
pub fn merge_bit_muxes(m: &mut Module) -> usize {
    // Driver of each wire bit, and depth of each cell (flip-flops, latches
    // and memories start new paths).
    let mut driver: FxHashMap<SigBit, CellId> = FxHashMap::default();
    for (ci, c) in m.cells.iter_enumerated().filter(|(_, c)| c.alive) {
        if matches!(c.kind, CellKind::Dff(_) | CellKind::Latch(_) | CellKind::Instance(_)) {
            continue;
        }
        for o in c.outputs() {
            for &b in o.iter() {
                driver.insert(b, ci);
            }
        }
    }
    let n = m.cells.len();
    let mut depth = vec![u32::MAX; n];
    let mut on_stack = vec![false; n];
    for start in m.live_cells().collect::<Vec<_>>() {
        if depth[start.index()] != u32::MAX {
            continue;
        }
        // Iterative depth-first search; cycles count as depth 0 (they stay
        // apart anyway, merged only with cells of their own depth).
        let mut stack = vec![(start, false)];
        while let Some((c, done)) = stack.pop() {
            if done {
                on_stack[c.index()] = false;
                let mut d = 0;
                m.cells[c].for_each_input(|b| {
                    if let Some(&p) = driver.get(&b)
                        && depth[p.index()] != u32::MAX
                    {
                        d = d.max(depth[p.index()] + 1);
                    }
                });
                depth[c.index()] = d;
                continue;
            }
            if depth[c.index()] != u32::MAX || on_stack[c.index()] {
                continue;
            }
            on_stack[c.index()] = true;
            stack.push((c, true));
            if matches!(m.cells[c].kind, CellKind::Dff(_) | CellKind::Latch(_)) {
                continue;
            }
            let mut ins = Vec::new();
            m.cells[c].for_each_input(|b| ins.push(b));
            for b in ins {
                if let Some(&p) = driver.get(&b)
                    && depth[p.index()] == u32::MAX
                    && !on_stack[p.index()]
                {
                    stack.push((p, false));
                }
            }
        }
    }
    // Groups: one-bit multiplexers by (depth, select, kind).
    #[derive(Hash, PartialEq, Eq, Clone)]
    enum Key {
        Mux(u32, SigBit, bool),
        Pmux(u32, Sig),
    }
    let mut groups: FxHashMap<Key, Vec<CellId>> = FxHashMap::default();
    let mut order: Vec<Key> = Vec::new();
    for ci in m.live_cells() {
        let key = match &m.cells[ci].kind {
            CellKind::Mux { y, s, ternary, .. } if y.width() == 1 => {
                Key::Mux(depth[ci.index()], *s, *ternary)
            }
            CellKind::Pmux { y, s, .. } if y.width() == 1 => {
                Key::Pmux(depth[ci.index()], s.clone())
            }
            _ => continue,
        };
        let g = groups.entry(key.clone()).or_default();
        if g.is_empty() {
            order.push(key);
        }
        g.push(ci);
    }
    // Shallow groups first, so that deeper ones can follow their order.
    order.sort_by_key(|k| match k {
        Key::Mux(d, ..) | Key::Pmux(d, ..) => *d,
    });
    // Position of each merged output bit: (group, index).
    let mut place: FxHashMap<SigBit, (usize, usize)> = FxHashMap::default();
    let mut merged = 0;
    for (gi, key) in order.iter().enumerate() {
        let mut cells = groups.remove(key).unwrap();
        if cells.len() < 2 {
            continue;
        }
        // Order by where the first input sits (in a merged group, or its
        // wire), so that consecutive cells read consecutive bits.
        let pos = |b: SigBit| -> (u64, u64, u64) {
            match place.get(&b) {
                Some(&(g, i)) => (0, g as u64, i as u64),
                None => match b {
                    SigBit::Wire(w, i) => (1, w.index() as u64, i as u64),
                    SigBit::Const(_) => (2, 0, 0),
                },
            }
        };
        let first = |c: &CellKind| -> SigBit {
            match c {
                CellKind::Mux { a, b, .. } => {
                    if a.0[0].is_const() {
                        b.0[0]
                    } else {
                        a.0[0]
                    }
                }
                CellKind::Pmux { default, cases, .. } => std::iter::once(default)
                    .chain(cases)
                    .map(|s| s.0[0])
                    .find(|b| !b.is_const())
                    .unwrap_or(default.0[0]),
                _ => unreachable!(),
            }
        };
        cells.sort_by_key(|&c| pos(first(&m.cells[c].kind)));
        for chunk in cells.chunks(64) {
            if chunk.len() < 2 {
                continue;
            }
            let kinds: Vec<CellKind> = chunk.iter().map(|&c| m.cells[c].kind.clone()).collect();
            let y = Sig(kinds
                .iter()
                .map(|k| match k {
                    CellKind::Mux { y, .. } | CellKind::Pmux { y, .. } => y.0[0],
                    _ => unreachable!(),
                })
                .collect());
            let kind = match &kinds[0] {
                CellKind::Mux { s, ternary, .. } => CellKind::Mux {
                    a: Sig(kinds
                        .iter()
                        .map(|k| match k {
                            CellKind::Mux { a, .. } => a.0[0],
                            _ => unreachable!(),
                        })
                        .collect()),
                    b: Sig(kinds
                        .iter()
                        .map(|k| match k {
                            CellKind::Mux { b, .. } => b.0[0],
                            _ => unreachable!(),
                        })
                        .collect()),
                    s: *s,
                    y: y.clone(),
                    ternary: *ternary,
                },
                CellKind::Pmux { s, cases, .. } => CellKind::Pmux {
                    default: Sig(kinds
                        .iter()
                        .map(|k| match k {
                            CellKind::Pmux { default, .. } => default.0[0],
                            _ => unreachable!(),
                        })
                        .collect()),
                    cases: (0..cases.len())
                        .map(|j| {
                            Sig(kinds
                                .iter()
                                .map(|k| match k {
                                    CellKind::Pmux { cases, .. } => cases[j].0[0],
                                    _ => unreachable!(),
                                })
                                .collect())
                        })
                        .collect(),
                    s: s.clone(),
                    y: y.clone(),
                },
                _ => unreachable!(),
            };
            for &c in chunk {
                m.cells[c].alive = false;
            }
            let name = format!("{}$word", m.cells[chunk[0]].name);
            m.add_cell(name, kind);
            for (i, &b) in y.iter().enumerate() {
                place.insert(b, (gi, i));
            }
            merged += chunk.len() - 1;
        }
    }
    merged
}

/// Flip-flops narrower than this are merged.
const NARROW: usize = 8;

/// Merges narrow flip-flops of the same wire (a register split into bits)
/// with the same clock and controls into words of up to 64 bits; returns
/// the number merged away. (Unrelated flags merged too would make each
/// one's readers depend on all of them.)
pub fn merge_flops(m: &mut Module) -> usize {
    type Key = (Clock, Option<Control>, Option<Control>, Option<Control>, bool, u64);
    let mut groups: FxHashMap<Key, Vec<CellId>> = FxHashMap::default();
    let mut order: Vec<Key> = Vec::new();
    for ci in m.live_cells() {
        let CellKind::Dff(d) = &m.cells[ci].kind else { continue };
        if d.q.width() >= NARROW {
            continue;
        }
        let key = (
            d.clk,
            d.en,
            d.srst.as_ref().map(|(c, _)| *c),
            d.arst.as_ref().map(|(c, _)| *c),
            d.en_over_srst && d.en.is_some() && d.srst.is_some(),
            match d.q.0[0] {
                SigBit::Wire(w, _) => w.index() as u64,
                SigBit::Const(_) => u64::MAX,
            },
        );
        let g = groups.entry(key).or_default();
        if g.is_empty() {
            order.push(key);
        }
        g.push(ci);
    }
    let pos = |b: SigBit| match b {
        SigBit::Wire(w, i) => (w.index() as u64, i as u64),
        SigBit::Const(_) => (u64::MAX, 0),
    };
    let mut merged = 0;
    for key in order {
        let mut cells = groups.remove(&key).unwrap();
        if cells.len() < 2 {
            continue;
        }
        cells.sort_by_key(|&c| match &m.cells[c].kind {
            CellKind::Dff(d) => pos(d.q.0[0]),
            _ => unreachable!(),
        });
        // Chunks of at most 64 bits, each flip-flop whole.
        let mut chunks: Vec<Vec<Dff>> = vec![Vec::new()];
        let mut width = 0;
        for &c in &cells {
            let CellKind::Dff(d) = &m.cells[c].kind else { unreachable!() };
            if width + d.q.width() > 64 {
                chunks.push(Vec::new());
                width = 0;
            }
            width += d.q.width();
            chunks.last_mut().unwrap().push(d.as_ref().clone());
        }
        for &c in &cells {
            m.cells[c].alive = false;
        }
        let name = m.cells[cells[0]].name.clone();
        for (k, chunk) in chunks.into_iter().enumerate() {
            let first = &chunk[0];
            let cat = |f: &dyn Fn(&Dff) -> LogicVec| {
                let parts: Vec<LogicVec> =
                    chunk.iter().map(|d| f(d).resize(d.q.width(), false)).collect();
                LogicVec::concat(&parts.iter().collect::<Vec<_>>())
            };
            let dff = Dff {
                clk: first.clk,
                d: chunk.iter().flat_map(|d| d.d.iter().copied()).collect(),
                q: chunk.iter().flat_map(|d| d.q.iter().copied()).collect(),
                en: first.en,
                srst: first
                    .srst
                    .as_ref()
                    .map(|(c, _)| (*c, cat(&|d| d.srst.as_ref().unwrap().1.clone()))),
                en_over_srst: first.en_over_srst,
                arst: first
                    .arst
                    .as_ref()
                    .map(|(c, _)| (*c, cat(&|d| d.arst.as_ref().unwrap().1.clone()))),
                init: cat(&|d| d.init.clone()),
            };
            merged += chunk.len() - 1;
            m.add_cell(format!("{name}$word{k}"), CellKind::Dff(Box::new(dff)));
        }
    }
    merged
}
