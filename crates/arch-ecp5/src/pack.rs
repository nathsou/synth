//! Packing ECP5 primitives into the units placement places.
//!
//! A logic tile (`PLC2`) has four slices of two logic cells: a LUT4 and a
//! flip-flop each. A flip-flop takes its data from its own cell's LUT
//! (`SD = 1`) or from the slice's `M` input (`SD = 0`), so a flip-flop
//! reading a LUT's output shares that LUT's cell, and others take a cell of
//! their own. A slice's two flip-flops share a clock enable; a tile's
//! flip-flops share a clock and a set/reset, as nextpnr requires (the
//! tile's second clock and set/reset are not used). A `CCU2C` fills a
//! slice; a carry chain fills consecutive slices of a row, eastward, and
//! flip-flops reading its sums share their halves' cells.

use synth_core::FxHashMap;
use synth_netlist::{CellId, Const, NetId, Netlist, Param};

use crate::cells::{CCU2C, DP16KD, FF, LUT4, MULT18X18D};

/// A logic cell's contents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lc {
    pub lut: Option<CellId>,
    pub ff: Option<CellId>,
}

/// A slice of a carry chain: its `CCU2C` and the flip-flops reading the
/// sums of its halves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CarrySlice {
    pub ccu: CellId,
    pub ff: [Option<CellId>; 2],
}

/// What a tile's flip-flops share: clock (and its inversion), set/reset
/// (and whether it acts asynchronously).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TileCtrl {
    pub clk: Option<NetId>,
    pub clk_inv: bool,
    pub lsr: Option<NetId>,
    pub lsr_async: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Packed {
    pub lcs: Vec<Lc>,
    pub chains: Vec<Vec<CarrySlice>>,
    /// Each flip-flop's tile controls, as an index into `tile_ctrls`.
    pub ff_tile: FxHashMap<CellId, u32>,
    pub tile_ctrls: Vec<TileCtrl>,
    /// Each flip-flop's clock enable (`None`: always enabled).
    pub ff_ce: FxHashMap<CellId, Option<NetId>>,
    /// Block RAMs and multipliers, placed on their own sites.
    pub blocks: Vec<CellId>,
}

/// Whether a flip-flop takes its data from its own cell's LUT.
pub fn ff_from_lut(nl: &Netlist, ff: CellId, lut: Option<CellId>) -> bool {
    let (Some(lut), Some(d)) = (lut, nl.pin_net(ff, "DI")) else { return false };
    match nl.cell_kind(lut) {
        LUT4 => nl.pin_net(lut, "Z") == Some(d),
        CCU2C => nl.pin_net(lut, "S0") == Some(d) || nl.pin_net(lut, "S1") == Some(d),
        _ => false,
    }
}

/// The name of pin `i` of cell `c`.
pub fn pin(nl: &Netlist, c: CellId, i: u32) -> &str {
    nl.str(nl.cells[c].pins[i as usize].name)
}

fn str_param<'a>(nl: &'a Netlist, c: CellId, name: &str) -> Option<&'a str> {
    match nl.param(c, name) {
        Some(Param::Str(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// A LUT4's `INIT` as a number (entry `D C B A`).
pub fn lut_init(nl: &Netlist, c: CellId, param: &str) -> u16 {
    nl.param(c, param).map_or(0, |p| {
        p.to_bits(16).iter().enumerate().map(|(i, b)| (b.is_one() as u16) << i).sum()
    })
}

/// The logic has no constant drivers: a constant on a LUT input folds into
/// the LUT's table (the input then reads as unconnected); a block RAM or
/// multiplier input takes its constant from its routing tile's tie (a
/// block RAM's address or data input, a multiplier's operand, high where
/// its `<pin>MUX` parameter is `1`) or reads its default when unconnected
/// (a block RAM's clock enables high, its clocks, write enables, resets and
/// chip selects low); and any other constant reader gets a LUT driving the
/// constant. Returns the LUTs added.
pub fn legalize_constants(nl: &mut Netlist) -> Result<usize, String> {
    let constant = |nl: &Netlist, n: NetId| nl.nets[n].constant.map(|k| k == Const::One);
    let luts: Vec<CellId> = nl.live_cells().filter(|&c| nl.cell_kind(c) == LUT4).collect();
    for c in luts {
        let mut init = lut_init(nl, c, "INIT");
        let mut changed = false;
        for (k, p) in ["A", "B", "C", "D"].iter().enumerate() {
            let Some(v) = nl.pin_net(c, p).and_then(|n| constant(nl, n)) else { continue };
            // Every entry reads the entry with input k at the constant.
            init = (0..16)
                .map(|i| {
                    let j = if v { i | 1 << k } else { i & !(1 << k) };
                    ((init >> j) & 1) << i
                })
                .sum();
            let i = nl.cells[c].pins.iter().position(|q| nl.str(q.name) == *p).expect("pin");
            nl.disconnect(synth_netlist::PinRef { cell: c, pin: i as u32 });
            changed = true;
        }
        if changed {
            nl.set_param(
                c,
                "INIT",
                Param::Bits(
                    (0..16)
                        .map(|i| {
                            if (init >> i) & 1 == 1 {
                                synth_netlist::Bit::One
                            } else {
                                synth_netlist::Bit::Zero
                            }
                        })
                        .collect(),
                ),
            );
        }
    }
    let blocks: Vec<CellId> =
        nl.live_cells().filter(|&c| matches!(nl.cell_kind(c), DP16KD | MULT18X18D)).collect();
    for c in blocks {
        let ram = nl.cell_kind(c) == DP16KD;
        for i in 0..nl.cells[c].pins.len() as u32 {
            let Some(v) = nl.cells[c].pins[i as usize].net.and_then(|n| constant(nl, n)) else {
                continue;
            };
            let name = pin(nl, c, i).to_string();
            let tie = if ram {
                name.starts_with("AD") || name.starts_with("DI")
            } else {
                // A multiplier's operands; its signedness takes a LUT.
                matches!(name.as_bytes()[0], b'A' | b'B' | b'C')
                    && name.len() > 1
                    && name[1..].bytes().all(|b| b.is_ascii_digit())
            };
            let default_high = ram && (name.starts_with("CE") || name.starts_with("OCE"));
            if !ram && !tie {
                continue;
            }
            if !tie && v != default_high {
                continue;
            }
            nl.disconnect(synth_netlist::PinRef { cell: c, pin: i });
            if tie && v {
                nl.set_param(c, &format!("{name}MUX"), Param::Str("1".into()));
            }
        }
    }
    let mut added = 0;
    let consts: Vec<NetId> = nl
        .live_nets()
        .filter(|&n| nl.nets[n].constant.is_some() && !nl.nets[n].users.is_empty())
        .collect();
    for n in consts {
        let one = constant(nl, n) == Some(true);
        let users = nl.nets[n].users.clone();
        let driven = nl.add_net(&format!("$const{}", u8::from(one)));
        let lut = nl.add_cell(&format!("$const{}_lut{added}", u8::from(one)), LUT4);
        nl.add_pin(lut, "Z", synth_netlist::Dir::Output, Some(driven)).map_err(|e| e.0)?;
        let init = if one { synth_netlist::Bit::One } else { synth_netlist::Bit::Zero };
        nl.set_param(lut, "INIT", Param::Bits(vec![init; 16]));
        for u in users {
            nl.disconnect(u);
            nl.connect(u, driven).map_err(|e| e.0)?;
        }
        added += 1;
    }
    Ok(added)
}

/// Packs the logic of `nl`.
pub fn pack(nl: &Netlist) -> Result<Packed, String> {
    let mut p = Packed::default();
    let mut ctrl_index: FxHashMap<TileCtrl, u32> = FxHashMap::default();
    for c in nl.live_cells().filter(|&c| nl.cell_kind(c) == FF) {
        let ctrl = TileCtrl {
            clk: nl.pin_net(c, "CLK"),
            clk_inv: str_param(nl, c, "CLKMUX") == Some("INV"),
            lsr: nl.pin_net(c, "LSR"),
            lsr_async: nl.pin_net(c, "LSR").is_some()
                && str_param(nl, c, "SRMODE") == Some("ASYNC"),
        };
        let next = p.tile_ctrls.len() as u32;
        let i = *ctrl_index.entry(ctrl).or_insert_with(|| {
            p.tile_ctrls.push(ctrl);
            next
        });
        p.ff_tile.insert(c, i);
        let ce = match str_param(nl, c, "CEMUX") {
            Some("CE") | None => nl.pin_net(c, "CE"),
            _ => None,
        };
        p.ff_ce.insert(c, ce);
    }
    // Carry chains, by their carries.
    let ccus: Vec<CellId> = nl.live_cells().filter(|&c| nl.cell_kind(c) == CCU2C).collect();
    let mut next_of: FxHashMap<CellId, CellId> = FxHashMap::default();
    let mut has_prev: FxHashMap<CellId, bool> = FxHashMap::default();
    for &c in &ccus {
        if let Some(co) = nl.pin_net(c, "COUT") {
            let readers: Vec<CellId> = nl.nets[co]
                .users
                .iter()
                .filter(|u| nl.cell_kind(u.cell) == CCU2C && pin(nl, u.cell, u.pin) == "CIN")
                .map(|u| u.cell)
                .collect();
            match readers.as_slice() {
                [n] => {
                    next_of.insert(c, *n);
                    has_prev.insert(*n, true);
                }
                [] => {}
                _ => return Err("a carry out feeds several chains".into()),
            }
            if nl.nets[co].users.len() != readers.len() {
                return Err("a carry chain's carry out is read outside the chain".into());
            }
        }
    }
    let mut paired_ff: FxHashMap<CellId, bool> = FxHashMap::default();
    // The flip-flops reading each LUT or carry sum.
    let mut readers: FxHashMap<NetId, Vec<CellId>> = FxHashMap::default();
    for c in nl.live_cells().filter(|&c| nl.cell_kind(c) == FF) {
        if let Some(d) = nl.pin_net(c, "DI") {
            readers.entry(d).or_default().push(c);
        }
    }
    let take_reader =
        |net: Option<NetId>, paired: &mut FxHashMap<CellId, bool>| -> Option<CellId> {
            let ff = readers.get(&net?)?.iter().copied().find(|f| !paired.contains_key(f))?;
            paired.insert(ff, true);
            Some(ff)
        };
    for &c in &ccus {
        if has_prev.contains_key(&c) {
            continue;
        }
        let mut chain = Vec::new();
        let mut cur = Some(c);
        while let Some(cell) = cur {
            let ff = [
                take_reader(nl.pin_net(cell, "S0"), &mut paired_ff),
                take_reader(nl.pin_net(cell, "S1"), &mut paired_ff),
            ];
            chain.push(CarrySlice { ccu: cell, ff });
            cur = next_of.get(&cell).copied();
        }
        p.chains.push(chain);
    }
    for c in nl.live_cells().filter(|&c| nl.cell_kind(c) == LUT4) {
        let ff = take_reader(nl.pin_net(c, "Z"), &mut paired_ff);
        p.lcs.push(Lc { lut: Some(c), ff });
    }
    for c in nl.live_cells().filter(|&c| nl.cell_kind(c) == FF) {
        if !paired_ff.contains_key(&c) {
            p.lcs.push(Lc { lut: None, ff: Some(c) });
        }
    }
    p.blocks =
        nl.live_cells().filter(|&c| matches!(nl.cell_kind(c), DP16KD | MULT18X18D)).collect();
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use synth_netlist::Dir;

    #[test]
    fn flip_flops_share_the_cells_of_the_luts_they_read() {
        let mut nl = Netlist::new();
        let (a, z, q, clk, z2) = (
            nl.add_net("a"),
            nl.add_net("z"),
            nl.add_net("q"),
            nl.add_net("clk"),
            nl.add_net("z2"),
        );
        let lut = nl.add_cell("lut", LUT4);
        nl.add_pin(lut, "A", Dir::Input, Some(a)).unwrap();
        nl.add_pin(lut, "Z", Dir::Output, Some(z)).unwrap();
        let lut2 = nl.add_cell("lut2", LUT4);
        nl.add_pin(lut2, "A", Dir::Input, Some(z)).unwrap();
        nl.add_pin(lut2, "Z", Dir::Output, Some(z2)).unwrap();
        // Two flip-flops reading `z`: one shares the LUT's cell.
        for (i, d) in [z, z].into_iter().enumerate() {
            let ff = nl.add_cell(&format!("ff{i}"), FF);
            nl.add_pin(ff, "DI", Dir::Input, Some(d)).unwrap();
            nl.add_pin(ff, "CLK", Dir::Input, Some(clk)).unwrap();
            let out = if i == 0 { Some(q) } else { None };
            nl.add_pin(ff, "Q", Dir::Output, out).unwrap();
        }
        let p = pack(&nl).unwrap();
        assert_eq!(p.lcs.len(), 3);
        assert!(p.lcs.iter().any(|l| l.lut == Some(lut) && l.ff.is_some()));
        assert!(p.lcs.iter().any(|l| l.lut == Some(lut2) && l.ff.is_none()));
        assert_eq!(p.tile_ctrls.len(), 1);
    }
}
