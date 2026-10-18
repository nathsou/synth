//! Slice packing with shared control-set legality and explicit carry macros.
use std::collections::{BTreeMap, BTreeSet};
use synth_netlist::{CellId, Const, Dir, NetId, Netlist, Param, PinRef};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Controls {
    pub clock: NetId,
    pub ce: Option<NetId>,
    pub sr: Option<NetId>,
    pub inverted: bool,
    pub synchronous: bool,
}
#[derive(Clone, Debug, Default)]
pub struct Slice {
    pub luts: [Option<CellId>; 4],
    pub ffs: [Option<CellId>; 4],
    pub carry: Option<CellId>,
    pub controls: Option<Controls>,
}
#[derive(Clone, Debug)]
pub struct Packed {
    pub slices: Vec<Slice>,
    pub chains: Vec<Vec<usize>>,
    pub slots: BTreeMap<CellId, (usize, usize)>,
    pub constants: BTreeMap<NetId, bool>,
    pub constant_luts: BTreeMap<NetId, CellId>,
}
pub fn is_lut(k: &str) -> bool {
    matches!(k, "LUT1" | "LUT2" | "LUT3" | "LUT4" | "LUT5" | "LUT6")
}
pub fn is_ff(k: &str) -> bool {
    matches!(k, "FDRE" | "FDSE" | "FDCE" | "FDPE")
}
pub fn ff_mux(nl: &Netlist, slice: &Slice, lane: usize, ff: CellId) -> &'static str {
    let driver = nl.pin_net(ff, "D").and_then(|n| nl.nets[n].driver).map(|d| d.cell);
    if driver.is_some() && driver == slice.luts[lane] {
        "O6"
    } else if driver.is_some() && driver == slice.carry {
        "XOR"
    } else {
        "BYPASS"
    }
}
pub fn value(nl: &Netlist, n: NetId) -> Option<bool> {
    nl.nets[n].constant.map(|v| v == Const::One)
}
pub fn param(nl: &Netlist, c: CellId, p: &str, default: u64) -> u64 {
    nl.param(c, p).and_then(Param::as_u64).unwrap_or(default)
}
fn controls(nl: &Netlist, c: CellId) -> Result<Controls, String> {
    let clock = nl.pin_net(c, "C").ok_or("flip-flop has no clock")?;
    let sr = ["R", "S", "CLR", "PRE"].iter().find_map(|p| nl.pin_net(c, p));
    Ok(Controls {
        clock,
        ce: nl.pin_net(c, "CE").filter(|&n| value(nl, n) != Some(true)),
        sr: sr.filter(|&n| value(nl, n) != Some(false)),
        inverted: param(nl, c, "IS_C_INVERTED", 0) != 0,
        synchronous: matches!(nl.cell_kind(c), "FDRE" | "FDSE"),
    })
}
fn input(nl: &mut Netlist, c: CellId, p: &str, n: NetId) -> Result<(), String> {
    nl.add_pin(c, p, Dir::Input, Some(n)).map_err(|e| e.to_string())?;
    Ok(())
}
fn output(nl: &mut Netlist, c: CellId, p: &str, n: NetId) -> Result<(), String> {
    nl.add_pin(c, p, Dir::Output, Some(n)).map_err(|e| e.to_string())?;
    Ok(())
}
fn rewire(nl: &mut Netlist, c: CellId, p: &str, n: NetId) -> Result<(), String> {
    let i = nl.cells[c]
        .pins
        .iter()
        .position(|pin| nl.str(pin.name) == p)
        .ok_or_else(|| format!("missing pin {p}"))?;
    let pin = PinRef { cell: c, pin: i as u32 };
    nl.disconnect(pin);
    nl.connect(pin, n).map_err(|e| e.to_string())
}
fn buffer(nl: &mut Netlist, n: NetId) -> Result<CellId, String> {
    let c = nl.add_cell("$slice_buffer", "LUT1");
    if let Some(v) = value(nl, n) {
        nl.set_param(c, "INIT", Param::Int(if v { 3 } else { 0 }));
    } else {
        nl.set_param(c, "INIT", Param::Int(2));
    }
    input(nl, c, "I0", n)?;
    let o = nl.add_net("$slice_buffer_o");
    output(nl, c, "O", o)?;
    Ok(c)
}

pub fn pack(nl: &mut Netlist) -> Result<Packed, String> {
    nl.check().map_err(|e| e.to_string())?;
    // Canonicalize constant nets before packing. Multiple constant drivers
    // waste LUTs and force the router to negotiate electrically identical nets.
    let zero = nl.add_const_net(Const::Zero);
    let one = nl.add_const_net(Const::One);
    let constants: Vec<_> = nl.live_nets().filter_map(|n| value(nl, n).map(|v| (n, v))).collect();
    for (n, v) in constants {
        let canonical = if v { one } else { zero };
        if n == canonical {
            continue;
        }
        for user in nl.nets[n].users.clone() {
            nl.disconnect(user);
            nl.connect(user, canonical).map_err(|e| e.to_string())?;
        }
    }
    for c in nl.live_cells().filter(|&c| is_ff(nl.cell_kind(c))) {
        for key in [
            "IS_D_INVERTED",
            "IS_R_INVERTED",
            "IS_S_INVERTED",
            "IS_CLR_INVERTED",
            "IS_PRE_INVERTED",
        ] {
            if param(nl, c, key, 0) != 0 {
                return Err(format!(
                    "{}: {key} is not supported by physical packing",
                    nl.str(nl.cells[c].name)
                ));
            }
        }
    }
    // CARRY4 sum and carry outputs share the slice OUTMUX. Export general
    // carry users through a three-input Boolean identity; dedicated cascades
    // remain on COUT/CIN and do not consume fabric routing.
    let carries: Vec<_> = nl.live_cells().filter(|&c| nl.cell_kind(c) == "CARRY4").collect();
    for &c in &carries {
        for lane in 0..4 {
            let Some(co) = nl.pin_net(c, &format!("CO[{lane}]")) else { continue };
            let external: Vec<_> = nl.nets[co]
                .users
                .iter()
                .copied()
                .filter(|p| {
                    lane != 3
                        || nl.cell_kind(p.cell) != "CARRY4"
                        || nl.str(nl.cells[p.cell].pins[p.pin as usize].name) != "CI"
                })
                .collect();
            if external.is_empty() {
                continue;
            }
            // When the sum is unused, OUTMUX can export the carry directly.
            // A mixed dedicated cascade/fabric net still needs a separate LUT
            // driver because it has two distinct physical source pins.
            let sum_used =
                nl.pin_net(c, &format!("O[{lane}]")).is_some_and(|n| !nl.nets[n].users.is_empty());
            let cascaded = external.len() != nl.nets[co].users.len();
            if !sum_used && !cascaded {
                continue;
            }
            let op = format!("O[{lane}]");
            let sum = if let Some(n) = nl.pin_net(c, &op) {
                n
            } else {
                let n = nl.add_net("$carry_sum_export");
                if nl.cells[c].pins.iter().any(|p| nl.str(p.name) == op) {
                    rewire(nl, c, &op, n)?;
                } else {
                    output(nl, c, &op, n)?;
                }
                n
            };
            let s = nl.pin_net(c, &format!("S[{lane}]")).ok_or("carry missing S")?;
            let di = nl.pin_net(c, &format!("DI[{lane}]")).ok_or("carry missing DI")?;
            let lut = nl.add_cell("$carry_export", "LUT3");
            nl.set_param(lut, "INIT", Param::Int(0x72)); // S ? !O : DI; I0=S, I1=O, I2=DI
            input(nl, lut, "I0", s)?;
            input(nl, lut, "I1", sum)?;
            input(nl, lut, "I2", di)?;
            let n = nl.add_net("$carry_export_o");
            output(nl, lut, "O", n)?;
            for p in external {
                nl.disconnect(p);
                nl.connect(p, n).map_err(|e| e.to_string())?;
            }
        }
    }
    for &c in &carries {
        let cy = nl.pin_net(c, "CYINIT").ok_or("carry missing CYINIT")?;
        if value(nl, cy).is_some() {
            continue;
        }
        let ci = nl.pin_net(c, "CI").ok_or("carry missing CI")?;
        if value(nl, ci) != Some(false) {
            return Err("CARRY4 with both CI and CYINIT dynamic is not supported".into());
        }
        let pre = nl.add_cell("$carry_dynamic_root", "CARRY4");
        input(nl, pre, "CI", zero)?;
        input(nl, pre, "CYINIT", zero)?;
        for lane in 0..4 {
            input(nl, pre, &format!("S[{lane}]"), if lane == 0 { zero } else { one })?;
            input(nl, pre, &format!("DI[{lane}]"), if lane == 0 { cy } else { zero })?;
        }
        let n = nl.add_net("$carry_dynamic_cascade");
        output(nl, pre, "CO[3]", n)?;
        rewire(nl, c, "CI", n)?;
        rewire(nl, c, "CYINIT", zero)?;
    }
    let mut p = Packed {
        slices: Vec::new(),
        chains: Vec::new(),
        slots: BTreeMap::new(),
        constants: BTreeMap::new(),
        constant_luts: BTreeMap::new(),
    };
    let carries: Vec<_> = nl.live_cells().filter(|&c| nl.cell_kind(c) == "CARRY4").collect();
    for c in carries {
        let idx = p.slices.len();
        let mut slice = Slice { carry: Some(c), ..Slice::default() };
        p.slots.insert(c, (idx, 0));
        for lane in 0..4 {
            let pin = format!("S[{lane}]");
            let n = nl.pin_net(c, &pin).ok_or("carry missing S pin")?;
            let existing = nl.nets[n]
                .driver
                .map(|d| d.cell)
                .filter(|d| is_lut(nl.cell_kind(*d)) && !p.slots.contains_key(d));
            let lut = if let Some(l) = existing {
                l
            } else {
                let l = buffer(nl, n)?;
                rewire(nl, c, &pin, nl.pin_net(l, "O").unwrap())?;
                l
            };
            slice.luts[lane] = Some(lut);
            p.slots.insert(lut, (idx, lane));
        }
        p.slices.push(slice);
    }
    let ffs: Vec<_> = nl.live_cells().filter(|&c| is_ff(nl.cell_kind(c))).collect();
    for c in ffs {
        let ctrl = controls(nl, c)?;
        let d = nl.pin_net(c, "D").ok_or("FF missing D")?;
        let driver = nl.nets[d].driver.map(|p| p.cell);
        let paired = nl.nets[d].driver.and_then(|drv| {
            let &(s, l) = p.slots.get(&drv.cell)?;
            let l = if nl.cell_kind(drv.cell) == "CARRY4" {
                let pin = nl.str(nl.cells[drv.cell].pins[drv.pin as usize].name);
                pin.strip_prefix("O[")?.strip_suffix(']')?.parse::<usize>().ok()?
            } else if is_lut(nl.cell_kind(drv.cell)) {
                l
            } else {
                return None;
            };
            (p.slices[s].ffs[l].is_none()
                && p.slices[s].controls.as_ref().is_none_or(|x| *x == ctrl))
            .then_some((s, l))
        });
        let (s, l) = if let Some(loc) = paired {
            loc
        } else {
            let lut = driver.filter(|d| is_lut(nl.cell_kind(*d)) && !p.slots.contains_key(d));
            let loc = p.slices.iter().enumerate().find_map(|(s, slice)| {
                if slice.controls.as_ref().is_some_and(|x| *x != ctrl) {
                    return None;
                }
                (0..4)
                    .find(|&l| {
                        slice.ffs[l].is_none()
                            && if lut.is_some() {
                                slice.luts[l].is_none()
                            } else {
                                slice.carry.is_none()
                            }
                    })
                    .map(|l| (s, l))
            });
            let (s, l) = loc.unwrap_or_else(|| {
                p.slices.push(Slice::default());
                (p.slices.len() - 1, 0)
            });
            if let Some(lut) = lut {
                p.slices[s].luts[l] = Some(lut);
                p.slots.insert(lut, (s, l));
            }
            (s, l)
        };
        p.slices[s].ffs[l] = Some(c);
        p.slices[s].controls = Some(ctrl);
        p.slots.insert(c, (s, l));
    }
    // Dedicated carries must form chains; branching CI fanout cannot be legal.
    let mut next = BTreeMap::new();
    let mut predecessor = BTreeSet::new();
    for (s, slice) in p.slices.iter().enumerate() {
        if let Some(c) = slice.carry
            && let Some(n) = nl.pin_net(c, "CO[3]")
        {
            for user in &nl.nets[n].users {
                if nl.cell_kind(user.cell) == "CARRY4"
                    && nl.str(nl.cells[user.cell].pins[user.pin as usize].name) == "CI"
                {
                    let target = p.slots[&user.cell].0;
                    if next.insert(s, target).is_some() || !predecessor.insert(target) {
                        return Err("branched carry cascade".into());
                    }
                }
            }
        }
    }
    let mut seen = BTreeSet::new();
    for s in 0..p.slices.len() {
        if p.slices[s].carry.is_none() || predecessor.contains(&s) {
            continue;
        }
        let mut chain = vec![s];
        seen.insert(s);
        let mut at = s;
        while let Some(&t) = next.get(&at) {
            if !seen.insert(t) {
                return Err("carry cycle".into());
            }
            chain.push(t);
            at = t;
        }
        p.chains.push(chain);
    }
    if seen.len() != p.slices.iter().filter(|s| s.carry.is_some()).count() {
        return Err("carry cycle".into());
    }
    let constants: Vec<_> = nl.live_nets().filter_map(|n| value(nl, n).map(|v| (n, v))).collect();
    for (n, v) in constants {
        p.constants.insert(n, v);
        if !nl.nets[n].users.is_empty() {
            let l = buffer(nl, n)?;
            p.constant_luts.insert(n, l);
        }
    }
    for n in [zero, one] {
        if let std::collections::btree_map::Entry::Vacant(e) = p.constant_luts.entry(n) {
            let l = buffer(nl, n)?;
            e.insert(l);
        }
    }
    let luts: Vec<_> =
        nl.live_cells().filter(|&c| is_lut(nl.cell_kind(c)) && !p.slots.contains_key(&c)).collect();
    for c in luts {
        let loc = p
            .slices
            .iter()
            .enumerate()
            .find_map(|(s, x)| x.luts.iter().position(Option::is_none).map(|l| (s, l)));
        let (s, l) = loc.unwrap_or_else(|| {
            p.slices.push(Slice::default());
            (p.slices.len() - 1, 0)
        });
        p.slices[s].luts[l] = Some(c);
        p.slots.insert(c, (s, l));
    }
    for c in nl.live_cells() {
        if nl.cell_kind(c) == "DSP48E1" {
            crate::physical_dsp::validate(nl, c)?;
        }
        if !is_lut(nl.cell_kind(c))
            && !is_ff(nl.cell_kind(c))
            && !matches!(nl.cell_kind(c), "CARRY4" | "RAMB18E1" | "DSP48E1" | "$input" | "$output")
        {
            return Err(format!(
                "physical implementation of {} ({}) is not implemented",
                nl.str(nl.cells[c].name),
                nl.cell_kind(c)
            ));
        }
    }
    nl.check().map_err(|e| e.to_string())?;
    Ok(p)
}
#[cfg(test)]
mod tests {
    #[test]
    fn carry_export_identity() {
        for s in [false, true] {
            for di in [false, true] {
                for carry in [false, true] {
                    let o = s ^ carry;
                    let index = usize::from(s) + 2 * usize::from(o) + 4 * usize::from(di);
                    assert_eq!((0x72 >> index) & 1, usize::from(if s { carry } else { di }));
                }
            }
        }
    }
}
