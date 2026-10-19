//! Shift registers: chains of flip-flops moved into SRLC32E LUTs.
//!
//! A register whose output only feeds the next register of the same clock
//! and enable, with no reset, is a stage of a shift register. Up to 32 such
//! stages fit in one SLICEM LUT, read at a constant tap address.
use crate::pack::{is_ff, param, value};
use std::collections::{BTreeMap, BTreeSet};
use synth_netlist::{Bit as PBit, CellId, Const, Dir, NetId, Netlist, Param};

/// Three stages are three eighths of a slice's registers, more than the
/// quarter of its LUTs that one SRL takes.
const MIN_STAGES: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Shift {
    clock: NetId,
    inverted: bool,
    ce: Option<NetId>,
}

fn stage(nl: &Netlist, c: CellId) -> Option<Shift> {
    if !is_ff(nl.cell_kind(c)) {
        return None;
    }
    // SRLs have no reset: the reset input must be tied inactive.
    let reset = ["R", "S", "CLR", "PRE"].iter().find_map(|p| nl.pin_net(c, p));
    if reset.is_some_and(|n| value(nl, n) != Some(false)) {
        return None;
    }
    Some(Shift {
        clock: nl.pin_net(c, "C")?,
        inverted: param(nl, c, "IS_C_INVERTED", 0) != 0,
        ce: nl.pin_net(c, "CE").filter(|&n| value(nl, n) != Some(true)),
    })
}

/// Replaces register chains by SRLC32E cells. Returns how many were created.
pub(crate) fn infer(nl: &mut Netlist) -> Result<usize, String> {
    let stages: BTreeMap<CellId, Shift> =
        nl.live_cells().filter_map(|c| stage(nl, c).map(|s| (c, s))).collect();
    let mut next = BTreeMap::new();
    let mut fed = BTreeSet::new();
    for (&a, s) in &stages {
        let Some(q) = nl.pin_net(a, "Q") else { continue };
        let [user] = nl.nets[q].users[..] else { continue };
        let b = user.cell;
        let pin = nl.str(nl.cells[b].pins[user.pin as usize].name);
        if b != a && pin == "D" && stages.get(&b) == Some(s) {
            next.insert(a, b);
            fed.insert(b);
        }
    }
    let mut count = 0;
    for (&head, s) in &stages {
        // Rings have no head and stay registers.
        if fed.contains(&head) || !next.contains_key(&head) {
            continue;
        }
        let mut chain = vec![head];
        while let Some(&b) = next.get(chain.last().unwrap()) {
            chain.push(b);
        }
        for chunk in chain.chunks(32).filter(|c| c.len() >= MIN_STAGES) {
            replace(nl, chunk, s, count)?;
            count += 1;
        }
    }
    Ok(count)
}

fn replace(nl: &mut Netlist, chunk: &[CellId], s: &Shift, k: usize) -> Result<(), String> {
    let first = chunk[0];
    let last = *chunk.last().unwrap();
    let d = nl.pin_net(first, "D").ok_or("register without D")?;
    let q = nl.pin_net(last, "Q").ok_or("register without Q")?;
    // Bit i of the shift register holds the value of stage i.
    let mut init = vec![PBit::Zero; 32];
    for (i, &c) in chunk.iter().enumerate() {
        let set = matches!(nl.cell_kind(c), "FDSE" | "FDPE");
        if param(nl, c, "INIT", u64::from(set)) != 0 {
            init[i] = PBit::One;
        }
    }
    for &c in chunk {
        nl.remove_cell(c);
    }
    let srl = nl.add_cell(&format!("$srl{k}"), "SRLC32E");
    let err = |e: synth_netlist::NetlistError| e.to_string();
    let tap = chunk.len() - 1;
    for i in 0..5 {
        let bit = nl.add_const_net(if tap >> i & 1 != 0 { Const::One } else { Const::Zero });
        nl.add_pin(srl, &format!("A[{i}]"), Dir::Input, Some(bit)).map_err(err)?;
    }
    let ce = s.ce.unwrap_or_else(|| nl.add_const_net(Const::One));
    nl.add_pin(srl, "CE", Dir::Input, Some(ce)).map_err(err)?;
    nl.add_pin(srl, "CLK", Dir::Input, Some(s.clock)).map_err(err)?;
    nl.add_pin(srl, "D", Dir::Input, Some(d)).map_err(err)?;
    nl.add_pin(srl, "Q", Dir::Output, Some(q)).map_err(err)?;
    nl.set_param(srl, "INIT", Param::Bits(init));
    nl.set_param(
        srl,
        "IS_CLK_INVERTED",
        Param::Bits(vec![if s.inverted { PBit::One } else { PBit::Zero }]),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A register `kind` with D `d` and returns its Q net.
    fn ff(nl: &mut Netlist, kind: &str, clk: NetId, d: NetId, init: bool) -> NetId {
        let c = nl.add_cell("ff", kind);
        let q = nl.add_net("q");
        let zero = nl.add_const_net(Const::Zero);
        let one = nl.add_const_net(Const::One);
        let reset = if kind == "FDSE" { "S" } else { "R" };
        for (pin, n) in [("C", clk), ("D", d), ("CE", one), (reset, zero)] {
            nl.add_pin(c, pin, Dir::Input, Some(n)).unwrap();
        }
        nl.add_pin(c, "Q", Dir::Output, Some(q)).unwrap();
        nl.set_param(c, "INIT", Param::Bits(vec![if init { PBit::One } else { PBit::Zero }]));
        q
    }

    fn output(nl: &mut Netlist, n: NetId) {
        let c = nl.add_cell("out", synth_netlist::OUTPUT_PORT);
        nl.add_pin(c, "PAD", Dir::Input, Some(n)).unwrap();
    }

    fn count(nl: &Netlist, kind: &str) -> usize {
        nl.live_cells().filter(|&c| nl.cell_kind(c) == kind).count()
    }

    #[test]
    fn chains_become_shift_registers_with_their_initial_values() {
        let mut nl = Netlist::new();
        let (clk, d) = (nl.add_net("clk"), nl.add_net("d"));
        let mut q = d;
        for (i, init) in [true, false, true, true].into_iter().enumerate() {
            q = ff(&mut nl, if i == 1 { "FDSE" } else { "FDRE" }, clk, q, init);
        }
        output(&mut nl, q);
        assert_eq!(infer(&mut nl).unwrap(), 1);
        assert_eq!(count(&nl, "SRLC32E") + count(&nl, "FDRE") + count(&nl, "FDSE"), 1);
        let srl = nl.live_cells().find(|&c| nl.cell_kind(c) == "SRLC32E").unwrap();
        assert_eq!(param(&nl, srl, "INIT", 0), 0b1101);
        let tap: Vec<_> =
            (0..5).map(|i| value(&nl, nl.pin_net(srl, &format!("A[{i}]")).unwrap())).collect();
        assert_eq!(tap, [Some(true), Some(true), Some(false), Some(false), Some(false)]);
        assert_eq!(nl.pin_net(srl, "D"), Some(d));
        nl.check().unwrap();
    }

    #[test]
    fn taps_resets_and_short_chains_stay_registers() {
        let mut nl = Netlist::new();
        let (clk, d) = (nl.add_net("clk"), nl.add_net("d"));
        // A tap after two stages: neither half is long enough.
        let a = ff(&mut nl, "FDRE", clk, d, false);
        let b = ff(&mut nl, "FDRE", clk, a, false);
        let c = ff(&mut nl, "FDRE", clk, b, false);
        let e = ff(&mut nl, "FDRE", clk, c, false);
        output(&mut nl, b);
        output(&mut nl, e);
        // A chain whose middle register has a live reset.
        let r = nl.add_net("r");
        let x = ff(&mut nl, "FDRE", clk, d, false);
        let y = ff(&mut nl, "FDRE", clk, x, false);
        let reset = nl.nets[y].driver.unwrap().cell;
        let pin = nl.cells[reset].pins.iter().position(|p| nl.str(p.name) == "R").unwrap();
        let pin = synth_netlist::PinRef { cell: reset, pin: pin as u32 };
        nl.disconnect(pin);
        nl.connect(pin, r).unwrap();
        let z = ff(&mut nl, "FDRE", clk, y, false);
        output(&mut nl, z);
        assert_eq!(infer(&mut nl).unwrap(), 0);
        assert_eq!(count(&nl, "FDRE"), 7);
    }
}
