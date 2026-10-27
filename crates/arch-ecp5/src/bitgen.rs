//! The chip configuration of a placed and routed design, as nextpnr-ecp5
//! writes it (`ecp5/bitstream.cc`): the device's empty-design base, every
//! switch the routes take, the logic slices, the I/O and the banks.

use std::collections::BTreeMap;

use synth_core::{FxHashMap, FxHashSet};
use synth_netlist::{CellId, INOUT_PORT, INPUT_PORT, NetId, Netlist, OUTPUT_PORT, Param};

use crate::arch::Arch;
use crate::cells::{CCU2C, DP16KD, FF, LUT4};
use crate::config::{ChipConfig, TileConfig, TileGroup};
use crate::graph::NO_TILE;
use crate::lpf::Constraints;
use crate::pack::{ff_from_lut, lut_init};
use crate::place::Placement;
use crate::route::{PadSites, Routing, edge_source};

const SLICES: [char; 4] = ['A', 'B', 'C', 'D'];
const LUT_PINS: [&str; 4] = ["A", "B", "C", "D"];

fn bits(v: u64, n: usize) -> Vec<bool> {
    (0..n).map(|i| (v >> i) & 1 == 1).collect()
}

fn str_param<'a>(nl: &'a Netlist, c: CellId, name: &str, default: &'a str) -> &'a str {
    match nl.param(c, name) {
        Some(Param::Str(s)) => s.as_str(),
        _ => default,
    }
}

/// The table of a LUT whose logical inputs arrive on physical inputs
/// `pins` (unconnected logical inputs read 0; the table does not depend on
/// them).
pub fn permute(init: u16, pins: [u8; 4], connected: [bool; 4]) -> u16 {
    (0..16u16)
        .map(|i| {
            let j: u16 = (0..4).filter(|&l| connected[l]).map(|l| ((i >> pins[l]) & 1) << l).sum();
            ((init >> j) & 1) << i
        })
        .sum()
}

/// The pad tile (`PIO*`) and the I/O logic tile (`PIC*`) of a pad (as
/// nextpnr's `get_pio_tile` and `get_pic_tile`).
pub(crate) fn io_tiles(
    arch: &Arch,
    x: usize,
    y: usize,
    l: char,
) -> Result<(String, String), String> {
    let (max_x, max_y) = (arch.dev.info.max_col, arch.dev.info.max_row);
    let find = |row: usize, col: usize, types: &[&str]| -> Result<String, String> {
        arch.dev
            .tile_of_type(row, col, types)
            .map(|t| t.name.clone())
            .ok_or_else(|| format!("no {types:?} tile at R{row}C{col}"))
    };
    let ab = matches!(l, 'A' | 'B');
    if y == 0 {
        let (dx, pio, pic) = if l == 'A' { (0, "PIOT0", "PICT0") } else { (1, "PIOT1", "PICT1") };
        Ok((find(0, x + dx, &[pio])?, find(1, x + dx, &[pic])?))
    } else if y == max_y {
        let a = ["PICB0", "EFB0_PICB0", "EFB2_PICB0", "SPICB0"];
        let b = ["PICB1", "EFB1_PICB1", "EFB3_PICB1"];
        let t = if l == 'A' { find(y, x, &a)? } else { find(y, x + 1, &b)? };
        Ok((t.clone(), t))
    } else if x == 0 || x == max_x {
        let (pio, picab, piccd): (&[&str], &[&str], &[&str]) = if x == 0 {
            (
                &["PICL1", "PICL1_DQS0", "PICL1_DQS3"],
                &["PICL0", "PICL0_DQS2"],
                &["PICL2", "PICL2_DQS1", "MIB_CIB_LR"],
            )
        } else {
            (
                &["PICR1", "PICR1_DQS0", "PICR1_DQS3"],
                &["PICR0", "PICR0_DQS2"],
                &["PICR2", "PICR2_DQS1", "MIB_CIB_LR_A"],
            )
        };
        let pic = if ab { find(y, x, picab)? } else { find(y + 2, x, piccd)? };
        Ok((find(y + 1, x, pio)?, pic))
    } else {
        Err(format!("no pad at R{y}C{x}"))
    }
}

/// The block RAM (`EBR<z>`) whose bel a tile of type `ttype` holds.
pub(crate) fn ebr_z(ttype: &str) -> Option<usize> {
    match ttype {
        "MIB_EBR0" | "EBR_CMUX_UR" | "EBR_CMUX_LR" | "EBR_CMUX_LR_25K" => Some(0),
        "MIB_EBR2" => Some(1),
        "MIB_EBR4" => Some(2),
        "MIB_EBR6" => Some(3),
        _ => None,
    }
}

/// The tiles holding the settings of block RAM `z` at `(x, y)` (as
/// nextpnr's `get_bram_tiles`).
pub(crate) fn ebr_tiles(arch: &Arch, x: usize, y: usize, z: usize) -> Result<Vec<String>, String> {
    const EBR0: &[&str] = &["MIB_EBR0", "EBR_CMUX_UR", "EBR_CMUX_LR", "EBR_CMUX_LR_25K"];
    const EBR8: &[&str] = &[
        "MIB_EBR8",
        "EBR_SPINE_UL1",
        "EBR_SPINE_UR1",
        "EBR_SPINE_LL1",
        "EBR_CMUX_UL",
        "EBR_SPINE_LL0",
        "EBR_CMUX_LL",
        "EBR_SPINE_LR0",
        "EBR_SPINE_LR1",
        "EBR_CMUX_LL_25K",
        "EBR_SPINE_UL2",
        "EBR_SPINE_UL0",
        "EBR_SPINE_UR2",
        "EBR_SPINE_LL2",
        "EBR_SPINE_LR2",
        "EBR_SPINE_UR0",
    ];
    let types: [&[&str]; 3] = match z {
        0 => [EBR0, &["MIB_EBR1"], &[]],
        1 => [&["MIB_EBR2"], &["MIB_EBR3"], &["MIB_EBR4"]],
        2 => [&["MIB_EBR4"], &["MIB_EBR5"], &["MIB_EBR6"]],
        _ => [&["MIB_EBR6"], &["MIB_EBR7"], EBR8],
    };
    let mut tiles = Vec::new();
    for (dx, types) in types.iter().enumerate() {
        if types.is_empty() {
            continue;
        }
        let t = arch
            .dev
            .tile_of_type(y, x + dx, types)
            .ok_or_else(|| format!("no {types:?} tile at R{y}C{}", x + dx))?;
        tiles.push(t.name.clone());
    }
    Ok(tiles)
}

/// Whether a wire is a routing tile's input to the cells, which the tile
/// can tie to a constant (`J[A-D]n`, `JCEn`, `JLSRn`, `JCLKn`).
fn is_cib_input(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('J') else { return false };
    let base = rest.trim_end_matches(|c: char| c.is_ascii_digit());
    rest.len() == base.len() + 1
        && rest.ends_with(|c: char| ('0'..='7').contains(&c))
        && matches!(base, "A" | "B" | "C" | "D" | "CE" | "LSR" | "CLK")
}

/// The routing tile inputs feeding wires `ties` (each searched uphill, as
/// nextpnr's `tie_cib_signal`), with the values to tie them to.
fn cib_ties(arch: &Arch, ties: &[(u32, bool)]) -> Result<Vec<(u32, bool)>, String> {
    let g = &arch.graph;
    let mut out = Vec::new();
    // Each search's frontier, by node.
    let mut frontier: FxHashMap<u32, Vec<usize>> = FxHashMap::default();
    let mut done = vec![false; ties.len()];
    for (i, &(w, value)) in ties.iter().enumerate() {
        if is_cib_input(g.name(w)) {
            out.push((w, value));
            done[i] = true;
        } else {
            frontier.entry(w).or_default().push(i);
        }
    }
    for _ in 0..4 {
        if frontier.is_empty() {
            break;
        }
        let mut next: FxHashMap<u32, Vec<usize>> = FxHashMap::default();
        for v in 0..g.num_nodes() as u32 {
            for e in g.edges(v) {
                let Some(searches) = frontier.get(&g.edge_dst[e]) else { continue };
                for &i in searches {
                    if done[i] {
                        continue;
                    }
                    if is_cib_input(g.name(v)) {
                        out.push((v, ties[i].1));
                        done[i] = true;
                    } else {
                        next.entry(v).or_default().push(i);
                    }
                }
            }
        }
        frontier = next;
    }
    if let Some(i) = done.iter().position(|d| !d) {
        return Err(format!("no routing tile input feeds {}", g.full_name(ties[i].0)));
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// The settings of block RAM `c` at `s` with write identifier `wid` (as
/// nextpnr's `write_bram`), and its initial contents as 2048 9-bit units.
/// Inputs nothing drives are tied (added to `ties`): clocks, write
/// enables and resets high and inverted, clock enables high, chip selects
/// high with the inversion folded into their decode, and address and data
/// inputs to their `<pin>MUX` parameter's value.
fn bram_config(
    arch: &Arch,
    nl: &Netlist,
    c: CellId,
    s: &crate::place::Site,
    wid: usize,
    ties: &mut Vec<(u32, bool)>,
) -> Result<(TileGroup, Vec<u16>), String> {
    let param = |name: &str, default: &str| -> String {
        match nl.param(c, name) {
            Some(Param::Str(v)) => v.clone(),
            Some(p @ Param::Int(_)) | Some(p @ Param::Bits(_)) => {
                p.to_bits(32).iter().rev().fold(0u64, |a, b| a << 1 | b.is_one() as u64).to_string()
            }
            _ => default.to_string(),
        }
    };
    let ebr = format!("EBR{}", s.z);
    let mut cfg = TileConfig::default();
    cfg.add_enum(&format!("{ebr}.MODE"), "DP16KD");
    for p in ["A", "B"] {
        cfg.add_enum(
            &format!("{ebr}.DP16KD.DATA_WIDTH_{p}"),
            &param(&format!("DATA_WIDTH_{p}"), "18"),
        );
        cfg.add_enum(
            &format!("{ebr}.DP16KD.WRITEMODE_{p}"),
            &param(&format!("WRITEMODE_{p}"), "NORMAL"),
        );
        cfg.add_enum(&format!("{ebr}.REGMODE_{p}"), &param(&format!("REGMODE_{p}"), "NOREG"));
    }
    cfg.add_enum(&format!("{ebr}.RESETMODE"), &param("RESETMODE", "SYNC"));
    cfg.add_enum(&format!("{ebr}.ASYNC_RESET_RELEASE"), &param("ASYNC_RESET_RELEASE", "SYNC"));
    cfg.add_enum(&format!("{ebr}.GSR"), &param("GSR", "DISABLED"));
    let wid_rev: usize = (0..9).filter(|i| wid >> i & 1 == 1).map(|i| 1 << (8 - i)).sum();
    cfg.add_word(&format!("{ebr}.WID"), bits(wid_rev as u64, 9));
    let mut muxes: FxHashMap<String, String> = FxHashMap::default();
    let mut csd: FxHashMap<&str, [bool; 3]> = FxHashMap::default();
    for p in ["A", "B"] {
        let v = param(&format!("CSDECODE_{p}"), "0b000");
        let v = v.trim_start_matches("0b").as_bytes().to_vec();
        // `0bXYZ`: Z decodes chip select 0.
        let bit = |i: usize| v.len() > i && v[v.len() - 1 - i] == b'1';
        csd.insert(p, [bit(0), bit(1), bit(2)]);
    }
    let mut pins: Vec<String> = Vec::new();
    for p in ["A", "B"] {
        pins.extend((0..14).map(|i| format!("AD{p}{i}")));
        pins.extend((0..18).map(|i| format!("DI{p}{i}")));
        pins.extend((0..3).map(|i| format!("CS{p}{i}")));
        pins.extend(["CLK", "CE", "OCE", "WE", "RST"].map(|k| format!("{k}{p}")));
    }
    for pin in &pins {
        if nl.pin_net(c, pin).is_some() {
            continue;
        }
        let wire = arch.node(s.x, s.y, &format!("J{pin}_EBR"))?;
        let mux = format!("{pin}MUX");
        let base = pin.trim_end_matches(['A', 'B']);
        if ["CLK", "WE", "RST"].contains(&base) {
            ties.push((wire, true));
            muxes.entry(mux).or_insert_with(|| "INV".into());
        } else if ["CE", "OCE"].contains(&base) {
            ties.push((wire, true));
        } else if let Some(i) = pin.strip_prefix("CS").and_then(|r| r[1..].parse::<usize>().ok()) {
            ties.push((wire, true));
            let d = csd.get_mut(&pin[2..3]).expect("port");
            d[i] = !d[i];
        } else {
            ties.push((wire, param(&mux, "0") == "1"));
        }
    }
    for p in ["A", "B"] {
        for k in ["CLK", "RST", "WE", "CE", "OCE"] {
            let name = format!("{k}{p}MUX");
            let v = muxes.get(&name).cloned().unwrap_or_else(|| param(&name, &format!("{k}{p}")));
            cfg.add_enum(&format!("{ebr}.{name}"), &v);
        }
        // Written most significant bit first.
        let d = csd[p];
        cfg.add_word(&format!("{ebr}.CSDECODE_{p}"), vec![d[2], d[1], d[0]]);
    }
    let mut units = vec![0u16; 2048];
    for k in 0..64 {
        let Some(p) = nl.param(c, &format!("INITVAL_{k:02X}")) else { continue };
        let v = p.to_bits(320);
        for j in 0..16 {
            for t in 0..18 {
                if v[20 * j + t].is_one() {
                    units[2 * (16 * k + j) + t / 9] |= 1 << (t % 9);
                }
            }
        }
    }
    let tiles = ebr_tiles(arch, s.x, s.y, s.z)?;
    Ok((TileGroup { tiles, config: cfg }, units))
}

/// The bank voltage of an I/O standard.
fn vccio(iotype: &str) -> Option<&'static str> {
    let t = iotype.trim_end_matches('D');
    Some(match t {
        "LVCMOS33" | "LVTTL33" => "3V3",
        "LVCMOS25" => "2V5",
        "LVCMOS18" | "SSTL18_I" | "SSTL18_II" => "1V8",
        "LVCMOS15" | "SSTL15_I" | "SSTL15_II" => "1V5",
        "LVCMOS12" | "HSUL12" => "1V2",
        "SSTL135_I" | "SSTL135_II" => "1V35",
        _ => return None,
    })
}

/// Writes the configuration of a placed and routed design.
pub fn config(
    arch: &Arch,
    nl: &Netlist,
    placement: &Placement,
    pads: &PadSites,
    routing: &Routing,
    cons: &Constraints,
) -> Result<ChipConfig, String> {
    let g = &arch.graph;
    let mut cc = crate::base::config(&arch.dev.info.name)?;
    cc.comments.push(format!("Part: {}-{}{}", arch.dev.info.name, arch.speed.grade, arch.package));
    let prefix = crate::graph::size_prefix(&arch.dev.info.name);
    // Switches.
    let all = routing.nets.values().chain(routing.dccs.iter().map(|d| &d.edges));
    let mut nodes_of: FxHashMap<NetId, FxHashSet<u32>> = FxHashMap::default();
    for (n, edges) in &routing.nets {
        nodes_of.insert(*n, edges.iter().map(|&e| g.edge_dst[e as usize]).collect());
    }
    for d in &routing.dccs {
        nodes_of.entry(d.net).or_default().extend(d.edges.iter().map(|&e| g.edge_dst[e as usize]));
    }
    for edges in all {
        for &e in edges {
            let t = g.edge_tile[e as usize];
            if t == NO_TILE {
                continue;
            }
            let tile = &arch.dev.tiles[t as usize];
            let bits = arch.db.tile_bits(&tile.ttype)?;
            let named = |n: u32| {
                let base = g.name_in_tile(n, tile.col, tile.row);
                [base.clone(), format!("{prefix}{base}")]
            };
            let (sinks, sources) = (named(g.edge_dst[e as usize]), named(edge_source(g, e)));
            let found = sinks.iter().find_map(|s| {
                let mux = bits.muxes.get(s)?;
                sources.iter().find_map(|src| {
                    mux.arcs.get(src).map(|b| (s.clone(), src.clone(), b.0.is_empty()))
                })
            });
            match found {
                Some((_, _, true)) => {}
                Some((sink, source, false)) => cc.tile(&tile.name).add_arc(&sink, &source),
                None => {
                    return Err(format!("{}: no switch {} -> {}", tile.name, sources[0], sinks[0]));
                }
            }
        }
    }
    // The tile wire (0 or 1) a net takes into a tile's shared clock or
    // set/reset, from the nodes its route reaches there.
    let tile_wire = |net: Option<NetId>, x: usize, y: usize, base: &str| -> Option<usize> {
        let nodes = nodes_of.get(&net?)?;
        (0..2).find(|i| g.node(x, y, &format!("{base}{i}")).is_some_and(|n| nodes.contains(&n)))
    };
    // Logic.
    let plc = |x: usize, y: usize| -> Result<String, String> {
        arch.dev
            .tile_of_type(y, x, &["PLC2"])
            .map(|t| t.name.clone())
            .ok_or_else(|| format!("no logic tile at R{y}C{x}"))
    };
    let mut luts: FxHashMap<(usize, usize, usize), CellId> = FxHashMap::default();
    for (&c, s) in &placement.sites {
        match nl.cell_kind(c) {
            LUT4 => {
                luts.insert((s.x, s.y, s.z), c);
            }
            CCU2C => {
                luts.insert((s.x, s.y, s.z), c);
                luts.insert((s.x, s.y, s.z + 1), c);
            }
            _ => {}
        }
    }
    let mut cells: Vec<(&CellId, &crate::place::Site)> =
        placement.sites.iter().filter(|(c, _)| nl.cell_kind(**c) != DP16KD).collect();
    cells.sort_by_key(|(c, _)| **c);
    for (&c, s) in cells {
        let tname = plc(s.x, s.y)?;
        let slice = format!("SLICE{}", SLICES[s.z / 2]);
        let k = s.z % 2;
        let t = cc.tile(&tname);
        match nl.cell_kind(c) {
            LUT4 => {
                let connected: [bool; 4] =
                    std::array::from_fn(|i| nl.pin_net(c, LUT_PINS[i]).is_some());
                let pins = routing.lut_pins.get(&c).copied().unwrap_or([0, 1, 2, 3]);
                let init = permute(lut_init(nl, c, "INIT"), pins, connected);
                t.add_enum(&format!("{slice}.MODE"), "LOGIC");
                t.add_word(&format!("{slice}.K{k}.INIT"), bits(init as u64, 16));
                let used: Vec<u8> = (0..4).filter(|&l| connected[l]).map(|l| pins[l]).collect();
                for (p, name) in LUT_PINS.iter().enumerate() {
                    if !used.contains(&(p as u8)) {
                        t.add_enum(&format!("{slice}.{name}{k}MUX"), "1");
                    }
                }
            }
            CCU2C => {
                t.add_enum(&format!("{slice}.MODE"), "CCU2");
                for h in 0..2 {
                    t.add_word(
                        &format!("{slice}.K{h}.INIT"),
                        bits(lut_init(nl, c, &format!("INIT{h}")) as u64, 16),
                    );
                    let inject = str_param(nl, c, &format!("INJECT1_{h}"), "YES");
                    t.add_enum(&format!("{slice}.CCU2.INJECT1_{h}"), inject);
                    for p in LUT_PINS {
                        if nl.pin_net(c, &format!("{p}{h}")).is_none() {
                            t.add_enum(&format!("{slice}.{p}{h}MUX"), "1");
                        }
                    }
                }
            }
            FF => {
                let from_lut = ff_from_lut(nl, c, luts.get(&(s.x, s.y, s.z)).copied());
                t.add_enum(&format!("{slice}.GSR"), str_param(nl, c, "GSR", "ENABLED"));
                t.add_enum(&format!("{slice}.REG{k}.SD"), if from_lut { "1" } else { "0" });
                t.add_enum(&format!("{slice}.REG{k}.REGSET"), str_param(nl, c, "REGSET", "RESET"));
                t.add_enum(&format!("{slice}.REG{k}.LSRMODE"), "LSR");
                let ce = if nl.pin_net(c, "CE").is_some() {
                    str_param(nl, c, "CEMUX", "CE")
                } else {
                    "1"
                };
                t.add_enum(&format!("{slice}.CEMUX"), ce);
                let lsr = nl.pin_net(c, "LSR");
                if let Some(i) = tile_wire(lsr, s.x, s.y, "LSR") {
                    let t = cc.tile(&tname);
                    t.add_enum(
                        &format!("LSR{i}.SRMODE"),
                        str_param(nl, c, "SRMODE", "LSR_OVER_CE"),
                    );
                    t.add_enum(&format!("LSR{i}.LSRMUX"), "LSR");
                }
                if let Some(i) = tile_wire(nl.pin_net(c, "CLK"), s.x, s.y, "CLK") {
                    cc.tile(&tname)
                        .add_enum(&format!("CLK{i}.CLKMUX"), str_param(nl, c, "CLKMUX", "CLK"));
                }
            }
            other => return Err(format!("cannot configure a {other}")),
        }
    }
    // I/O.
    let mut banks: BTreeMap<u8, &str> = BTreeMap::new();
    let mut ties: Vec<(u32, bool)> = Vec::new();
    let mut ports: Vec<(&CellId, &(usize, usize, char))> = pads.iter().collect();
    ports.sort_by_key(|(c, _)| **c);
    for (&c, &(x, y, l)) in ports {
        let name = nl.str(nl.cells[c].name).to_string();
        let attrs = cons.iobuf.get(&name);
        let attr = |k: &str| attrs.and_then(|a| a.get(k)).map(String::as_str);
        let iotype = attr("IO_TYPE").unwrap_or("LVCMOS33");
        let dir = match nl.cell_kind(c) {
            INPUT_PORT => "INPUT",
            OUTPUT_PORT => "OUTPUT",
            INOUT_PORT => return Err(format!("{name}: bidirectional pads are not supported yet")),
            k => return Err(format!("{name}: not a port ({k})")),
        };
        let pio = format!("PIO{l}");
        let (pio_tile, pic_tile) = io_tiles(arch, x, y, l)?;
        let base = format!("{dir}_{iotype}");
        cc.tile(&pio_tile).add_enum(&format!("{pio}.BASE_TYPE"), &base);
        cc.tile(&pic_tile).add_enum(&format!("{pio}.BASE_TYPE"), &base);
        if dir != "INPUT" {
            let v =
                vccio(iotype).ok_or_else(|| format!("{name}: unsupported I/O type {iotype}"))?;
            let pin = crate::db::Pin { row: y, col: x, pio: l };
            if let Some(bank) = arch.bank(&pin) {
                if banks.get(&bank).is_some_and(|&b| b != v) {
                    return Err(format!(
                        "{name}: bank {bank} has outputs at {} and {v}",
                        banks[&bank]
                    ));
                }
                banks.insert(bank, v);
            }
            ties.push((arch.node(x, y, &format!("JPADDT{l}"))?, false));
        } else {
            cc.tile(&pio_tile)
                .add_enum(&format!("{pio}.HYSTERESIS"), attr("HYSTERESIS").unwrap_or("ON"));
        }
        for k in ["SLEWRATE", "PULLMODE", "CLAMP", "OPENDRAIN", "DIFFRESISTOR"] {
            if let Some(v) = attr(k) {
                cc.tile(&pio_tile).add_enum(&format!("{pio}.{k}"), v);
            }
        }
        if let Some(v) = attr("DRIVE")
            && iotype == "LVCMOS33"
        {
            cc.tile(&pio_tile).add_enum(&format!("{pio}.DRIVE"), v);
        }
    }
    // Block RAMs.
    let mut brams: Vec<(&CellId, &crate::place::Site)> =
        placement.sites.iter().filter(|(c, _)| nl.cell_kind(**c) == DP16KD).collect();
    brams.sort_by_key(|(c, _)| **c);
    for (wid, (&c, s)) in brams.into_iter().enumerate() {
        let (group, units) = bram_config(arch, nl, c, s, wid, &mut ties)?;
        cc.bram_init.insert(wid as u16, units);
        cc.groups.push(group);
    }
    // Output enables tied low (driving) where no route drives them, and
    // block RAM inputs nothing drives: the CIB input feeding each, tied.
    for (cib, value) in cib_ties(arch, &ties)? {
        let (x, y) = (g.node_x[cib as usize] as usize, g.node_y[cib as usize] as usize);
        let tile = arch
            .dev
            .tiles_at(y, x)
            .find(|t| t.ttype.starts_with("CIB") || t.ttype.starts_with("VCIB"))
            .ok_or_else(|| format!("no CIB tile at R{y}C{x}"))?;
        let name = g.name(cib);
        // Clock, set/reset and enable inputs only tie with one setting,
        // `0` for the first two, which reads as high.
        let setting = match (name.starts_with("JCLK") || name.starts_with("JLSR"), value) {
            (true, true) => "0",
            (true, false) => return Err(format!("cannot tie {name} low")),
            (false, v) => {
                if name.starts_with("JCE") && !v {
                    return Err(format!("cannot tie {name} low"));
                }
                if v { "1" } else { "0" }
            }
        };
        cc.tile(&tile.name.clone()).add_enum(&format!("CIB.{name}MUX"), setting);
    }
    for t in &arch.dev.tiles {
        if let Some(bank) = t.ttype.strip_prefix("BANKREF").and_then(|b| b.parse::<u8>().ok())
            && let Some(v) = banks.get(&bank)
        {
            cc.tile(&t.name.clone()).add_enum("BANK.VCCIO", v);
        }
    }
    for (k, v) in &cons.sysconfig {
        cc.sysconfig.insert(k.clone(), v.clone());
    }
    cc.tiles.retain(|_, t: &mut TileConfig| !t.is_empty());
    Ok(cc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_follow_their_inputs() {
        // A & !B, its inputs swapped onto B and A.
        let and_not_b: u16 = (0..16).map(|i| u16::from(i & 1 == 1 && i & 2 == 0) << i).sum();
        let swapped = permute(and_not_b, [1, 0, 2, 3], [true, true, false, false]);
        for i in 0..16u16 {
            let (b_phys, a_phys) = (i & 1 == 1, i & 2 == 2);
            assert_eq!((swapped >> i) & 1 == 1, a_phys && !b_phys);
        }
    }
}
