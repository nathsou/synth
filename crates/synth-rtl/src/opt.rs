//! RTL clean-up passes: connection resolution and dead-logic removal.

use synth_core::{FxHashMap, FxHashSet};

use crate::{CellKind, Module, PortDir, Sig, SigBit};

/// Resolves direct connections: every use of a connected wire bit is
/// replaced by its driver, and the connections are dropped except where an
/// output port must stay driven. Returns the number of rewritten bits.
pub fn resolve_connections(m: &mut Module) -> Result<usize, String> {
    // Map each connected wire bit to its source, following chains.
    let mut map: FxHashMap<SigBit, SigBit> = FxHashMap::default();
    for (lhs, rhs) in &m.connections {
        for (l, r) in lhs.iter().zip(rhs.iter()) {
            if l == r {
                continue;
            }
            if let SigBit::Wire(..) = l
                && let Some(prev) = map.insert(*l, *r)
                && prev != *r
            {
                return Err(format!("{} is driven twice", m.bit_name(*l)));
            }
        }
    }
    let resolve = |mut b: SigBit, map: &FxHashMap<SigBit, SigBit>| {
        let mut steps = 0;
        while let Some(&n) = map.get(&b) {
            b = n;
            steps += 1;
            if steps > 100_000 {
                break; // combinational loop through connections
            }
        }
        b
    };
    let mut count = 0;
    for cell in m.cells.iter_mut().filter(|c| c.alive) {
        cell.map_inputs(|b| {
            let r = resolve(b, &map);
            if r != b {
                count += 1;
            }
            r
        });
    }
    for mem in m.memories.iter_mut() {
        let fix = |s: &mut Sig| s.0.iter_mut().for_each(|b| *b = resolve(*b, &map));
        for rd in &mut mem.reads {
            fix(&mut rd.addr);
            let mut en = Sig::bit(rd.en);
            fix(&mut en);
            rd.en = en.0[0];
            if let Some(c) = &mut rd.clk {
                c.sig = resolve(c.sig, &map);
            }
        }
        for wr in &mut mem.writes {
            fix(&mut wr.addr);
            fix(&mut wr.data);
            fix(&mut wr.en);
            wr.clk.sig = resolve(wr.clk.sig, &map);
        }
    }
    // Keep only connections that drive output (or inout) port bits.
    let mut keep = Vec::new();
    for w in m.ports() {
        let wire = &m.wires[w];
        if matches!(wire.port, Some((PortDir::Output, _)) | Some((PortDir::Inout, _))) {
            let lhs = m.wire_sig(w);
            let rhs: Sig = lhs.iter().map(|&b| resolve(b, &map)).collect();
            let pairs: Vec<(SigBit, SigBit)> =
                lhs.iter().zip(rhs.iter()).filter(|(l, r)| l != r).map(|(l, r)| (*l, *r)).collect();
            if !pairs.is_empty() {
                keep.push((
                    pairs.iter().map(|p| p.0).collect(),
                    pairs.iter().map(|p| p.1).collect(),
                ));
            }
        }
    }
    m.connections = keep;
    Ok(count)
}

/// Removes cells and memories whose outputs are never used. Instances are
/// kept (they may have external effects). Returns the number removed.
pub fn remove_dead(m: &mut Module) -> usize {
    let mut removed = 0;
    loop {
        let mut used: FxHashSet<SigBit> = FxHashSet::default();
        for c in m.cells.iter().filter(|c| c.alive) {
            c.for_each_input(|b| {
                used.insert(b);
            });
        }
        for mem in m.memories.iter().filter(|m| m.alive) {
            for rd in &mem.reads {
                used.extend(rd.addr.iter().copied());
                used.insert(rd.en);
                if let Some(c) = rd.clk {
                    used.insert(c.sig);
                }
            }
            for wr in &mem.writes {
                used.extend(wr.addr.iter().chain(wr.data.iter()).chain(wr.en.iter()).copied());
                used.insert(wr.clk.sig);
            }
        }
        for (_, rhs) in &m.connections {
            used.extend(rhs.iter().copied());
        }
        for w in m.ports() {
            if m.wires[w].port.is_some_and(|(d, _)| d != PortDir::Input) {
                used.extend(m.wire_sig(w).iter().copied());
            }
        }
        let mut changed = false;
        for c in m.cells.iter_mut().filter(|c| c.alive) {
            if matches!(c.kind, CellKind::Instance(_)) {
                continue;
            }
            let live = c.outputs().iter().any(|s| s.iter().any(|b| used.contains(b)));
            if !live {
                c.alive = false;
                removed += 1;
                changed = true;
            }
        }
        for mem in m.memories.iter_mut().filter(|m| m.alive) {
            mem.reads.retain(|rd| rd.data.iter().any(|b| used.contains(b)));
            if mem.reads.is_empty() {
                mem.alive = false;
                removed += 1;
                changed = true;
            }
        }
        if !changed {
            return removed;
        }
    }
}

/// Runs the clean-up passes.
pub fn clean(m: &mut Module) -> Result<(), String> {
    resolve_connections(m)?;
    remove_dead(m);
    Ok(())
}
