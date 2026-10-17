//! Packing: rewrites a netlist of iCE40 primitives (`SB_LUT4`, `SB_DFF*`,
//! `SB_CARRY`, ...) into cells that map one-to-one onto bels:
//!
//! - `ICESTORM_LC`: LUT4 + carry + flip-flop of one logic cell;
//! - `SB_IO` (top-level ports get one inserted automatically);
//! - `ICESTORM_RAM`, `SB_GB`.
//!
//! It also forms carry chains, folds constant LUT inputs into truth tables,
//! drives remaining constants from dedicated logic cells, and promotes clocks
//! and high-fanout control nets to global buffers.

use synth_core::{FxHashMap, FxHashSet};
use synth_netlist::{
    CellId, Const, Dir, INOUT_PORT, INPUT_PORT, NetId, Netlist, OUTPUT_PORT, Param, PinRef,
};

use crate::cells::{dff_kind, port_base};

pub const LC: &str = "ICESTORM_LC";
pub const RAM: &str = "ICESTORM_RAM";
pub const IO: &str = "SB_IO";
pub const GB: &str = "SB_GB";

/// Logic cell input pins, in order.
pub const LC_INPUTS: [&str; 4] = ["I0", "I1", "I2", "I3"];

/// The result of packing besides the rewritten netlist.
#[derive(Clone, Debug, Default)]
pub struct Packed {
    /// Carry chains: logic cells that must occupy consecutive positions,
    /// first (least significant) element first.
    pub chains: Vec<Vec<CellId>>,
}

/// Packing statistics and warnings, for reporting.
#[derive(Clone, Debug, Default)]
pub struct PackReport {
    pub messages: Vec<String>,
}

fn pin_ref(nl: &Netlist, cell: CellId, pin: &str) -> Option<PinRef> {
    let sym = nl.names.get(pin)?;
    nl.cells[cell].pin_index(sym).map(|i| PinRef { cell, pin: i as u32 })
}

fn net_of(nl: &Netlist, cell: CellId, pin: &str) -> Option<NetId> {
    nl.pin_net(cell, pin)
}

fn const_of(nl: &Netlist, net: Option<NetId>) -> Option<Const> {
    net.and_then(|n| nl.nets[n].constant)
}

/// Creates a logic cell with all its pins, unconnected.
fn new_lc(nl: &mut Netlist, name: &str) -> CellId {
    let c = nl.add_cell(name, LC);
    for p in LC_INPUTS.iter().chain(&["CIN", "CLK", "CEN", "SR"]) {
        nl.add_pin(c, p, Dir::Input, None).unwrap();
    }
    for p in ["O", "COUT"] {
        nl.add_pin(c, p, Dir::Output, None).unwrap();
    }
    for p in [
        "LUT_INIT",
        "DFF_ENABLE",
        "NEG_CLK",
        "CARRY_ENABLE",
        "SET_NORESET",
        "ASYNC_SR",
        "CIN_CONST",
        "CIN_SET",
    ] {
        nl.set_param(c, p, Param::Int(0));
    }
    c
}

fn set_int(nl: &mut Netlist, c: CellId, p: &str, v: u64) {
    nl.set_param(c, p, Param::Int(v as i64));
}

fn get_int(nl: &Netlist, c: CellId, p: &str) -> u64 {
    nl.param(c, p).and_then(Param::as_u64).unwrap_or(0)
}

/// Moves the connection of `from.from_pin` onto `to.to_pin` (which must be
/// unconnected).
fn move_pin(nl: &mut Netlist, from: CellId, from_pin: &str, to: CellId, to_pin: &str) {
    let Some(src) = pin_ref(nl, from, from_pin) else { return };
    let Some(net) = nl.cells[from].pins[src.pin as usize].net else { return };
    nl.disconnect(src);
    let dst = pin_ref(nl, to, to_pin).expect("destination pin exists");
    nl.connect(dst, net).expect("connect moved pin");
}

/// Connects `to.pin` to the same net as `from.from_pin`, keeping the original.
fn copy_pin(nl: &mut Netlist, from: CellId, from_pin: &str, to: CellId, to_pin: &str) {
    if let Some(net) = net_of(nl, from, from_pin) {
        let dst = pin_ref(nl, to, to_pin).expect("destination pin exists");
        nl.connect(dst, net).expect("connect copied pin");
    }
}

/// Packs `nl` in place.
pub fn pack(nl: &mut Netlist) -> Result<(Packed, PackReport), String> {
    let mut report = PackReport::default();
    pack_io(nl)?;
    pack_logic(nl)?;
    let chains = pack_carries(nl, &mut report)?;
    pack_ram(nl)?;
    fold_lut_constants(nl);
    drive_constants(nl)?;
    promote_globals(nl, &mut report)?;
    nl.check().map_err(|e| e.0)?;
    for c in nl.live_cells() {
        let k = nl.cell_kind(c);
        if !matches!(k, LC | RAM | IO | GB) {
            return Err(format!("cell {} of type {k} is not supported", nl.str(nl.cells[c].name)));
        }
    }
    Ok((Packed { chains }, report))
}

/// Gives every top-level port an `SB_IO` (absorbing explicitly instantiated
/// ones) and removes the port pseudo-cells. The port name is kept in the
/// `PORT` attribute for pin constraints.
fn pack_io(nl: &mut Netlist) -> Result<(), String> {
    let ports: Vec<CellId> = nl
        .live_cells()
        .filter(|&c| matches!(nl.cell_kind(c), INPUT_PORT | OUTPUT_PORT | INOUT_PORT))
        .collect();
    for port in ports {
        let name = nl.str(nl.cells[port].name).to_string();
        let kind = nl.cell_kind(port).to_string();
        let net = net_of(nl, port, "PAD");
        // An SB_IO already attached to the pad absorbs the port.
        let existing = net.and_then(|n| {
            let net = &nl.nets[n];
            net.driver.iter().chain(&net.users).copied().find(|p| {
                matches!(nl.cell_kind(p.cell), "SB_IO" | "SB_GB_IO")
                    && nl.str(nl.cells[p.cell].pins[p.pin as usize].name) == "PACKAGE_PIN"
            })
        });
        if let Some(io) = existing {
            nl.remove_cell(port);
            let sio = io.cell;
            nl.disconnect(io);
            let port_attr = nl.sym("PORT");
            nl.cells[sio].attrs.insert(port_attr, Param::Str(name));
            if nl.cell_kind(sio) == "SB_GB_IO" {
                let k = nl.sym(IO);
                nl.cells[sio].kind = k;
                let g = nl.sym("GLOBAL");
                nl.cells[sio].attrs.insert(g, Param::Int(1));
                // Its global output becomes a separate SB_GB fed from D_IN_0
                // if not placed on a global-capable pad later; keep the pin.
            }
            continue;
        }
        let io = nl.add_cell(&format!("{name}$sb_io"), IO);
        for p in [
            "D_OUT_0",
            "D_OUT_1",
            "OUTPUT_ENABLE",
            "CLOCK_ENABLE",
            "INPUT_CLK",
            "OUTPUT_CLK",
            "LATCH_INPUT_VALUE",
        ] {
            nl.add_pin(io, p, Dir::Input, None).unwrap();
        }
        for p in ["D_IN_0", "D_IN_1"] {
            nl.add_pin(io, p, Dir::Output, None).unwrap();
        }
        let port_attr = nl.sym("PORT");
        nl.cells[io].attrs.insert(port_attr, Param::Str(name.clone()));
        match kind.as_str() {
            INPUT_PORT => {
                set_int(nl, io, "PIN_TYPE", 0b000001);
                move_pin(nl, port, "PAD", io, "D_IN_0");
            }
            OUTPUT_PORT => {
                set_int(nl, io, "PIN_TYPE", 0b011000);
                move_pin(nl, port, "PAD", io, "D_OUT_0");
            }
            _ => {
                return Err(format!(
                    "bidirectional port {name} needs an explicit SB_IO instance (tristate inference is not supported yet)"
                ));
            }
        }
        nl.remove_cell(port);
    }
    Ok(())
}

/// Packs LUTs and flip-flops into logic cells.
fn pack_logic(nl: &mut Netlist) -> Result<(), String> {
    let cells: Vec<CellId> = nl.live_cells().collect();
    let mut lut_of_dff: FxHashMap<CellId, CellId> = FxHashMap::default();
    let mut paired: FxHashSet<CellId> = FxHashSet::default();
    for &c in &cells {
        if dff_kind(nl.cell_kind(c)).is_none() {
            continue;
        }
        let Some(d) = net_of(nl, c, "D") else { continue };
        let net = &nl.nets[d];
        if net.users.len() != 1 {
            continue;
        }
        if let Some(drv) = net.driver
            && nl.cell_kind(drv.cell) == "SB_LUT4"
            && !paired.contains(&drv.cell)
        {
            paired.insert(drv.cell);
            lut_of_dff.insert(c, drv.cell);
        }
    }
    for &c in &cells {
        let kind = nl.cell_kind(c).to_string();
        if kind == "SB_LUT4" {
            if paired.contains(&c) {
                continue; // packed with its flop below
            }
            let name = nl.str(nl.cells[c].name).to_string();
            let lc = new_lc(nl, &format!("{name}$lc"));
            for p in LC_INPUTS {
                move_pin(nl, c, p, lc, p);
            }
            move_pin(nl, c, "O", lc, "O");
            let init = get_int(nl, c, "LUT_INIT") & 0xFFFF;
            set_int(nl, lc, "LUT_INIT", init);
            nl.remove_cell(c);
        } else if let Some(k) = dff_kind(&kind) {
            let name = nl.str(nl.cells[c].name).to_string();
            let lc = new_lc(nl, &format!("{name}$lc"));
            if let Some(&lut) = lut_of_dff.get(&c) {
                for p in LC_INPUTS {
                    move_pin(nl, lut, p, lc, p);
                }
                let init = get_int(nl, lut, "LUT_INIT") & 0xFFFF;
                set_int(nl, lc, "LUT_INIT", init);
                nl.remove_cell(lut);
            } else {
                // Flop only: the LUT passes I0 through.
                move_pin(nl, c, "D", lc, "I0");
                set_int(nl, lc, "LUT_INIT", 0xAAAA);
            }
            set_int(nl, lc, "DFF_ENABLE", 1);
            set_int(nl, lc, "NEG_CLK", k.neg_clk as u64);
            move_pin(nl, c, "C", lc, "CLK");
            if k.enable {
                match const_of(nl, net_of(nl, c, "E")) {
                    Some(Const::One) | Some(Const::Undef) => {}
                    _ => move_pin(nl, c, "E", lc, "CEN"),
                }
            }
            if k.has_sr {
                let port = if k.set { "S" } else { "R" };
                match const_of(nl, net_of(nl, c, port)) {
                    Some(Const::Zero) | Some(Const::Undef) => {}
                    _ => {
                        move_pin(nl, c, port, lc, "SR");
                        set_int(nl, lc, "SET_NORESET", k.set as u64);
                        set_int(nl, lc, "ASYNC_SR", k.async_sr as u64);
                    }
                }
            }
            move_pin(nl, c, "Q", lc, "O");
            if let Some(init) = nl.cells[c]
                .attrs
                .iter()
                .find(|(k, _)| nl.str(**k) == "init")
                .map(|(_, v)| v.clone())
                && init.as_u64() == Some(1)
                && !k.set
            {
                // iCE40 flops power up as 0; an init of 1 cannot be honoured.
            }
            nl.remove_cell(c);
        }
    }
    Ok(())
}

/// Merges `SB_CARRY` cells into logic cells and builds carry chains.
fn pack_carries(nl: &mut Netlist, report: &mut PackReport) -> Result<Vec<Vec<CellId>>, String> {
    let carries: Vec<CellId> = nl.live_cells().filter(|&c| nl.cell_kind(c) == "SB_CARRY").collect();
    let mut used: FxHashSet<CellId> = FxHashSet::default();
    let mut carry_only = 0;
    for &ca in &carries {
        let i0 = net_of(nl, ca, "I0");
        let i1 = net_of(nl, ca, "I1");
        let ci = net_of(nl, ca, "CI");
        // Candidate LCs read I0 on I1 and I1 on I2.
        let users_on = |net: Option<NetId>, pin: &str| -> FxHashSet<CellId> {
            let Some(n) = net else { return FxHashSet::default() };
            nl.nets[n]
                .users
                .iter()
                .filter(|p| {
                    nl.cell_kind(p.cell) == LC
                        && nl.str(nl.cells[p.cell].pins[p.pin as usize].name) == pin
                })
                .map(|p| p.cell)
                .filter(|c| !used.contains(c))
                .collect()
        };
        let a = users_on(i0, "I1");
        let b = users_on(i1, "I2");
        let mut cands: Vec<CellId> = a.intersection(&b).copied().collect();
        cands.sort();
        // Prefer the LC whose I3 is the carry input (the sum LUT of an adder).
        let pick = cands
            .iter()
            .copied()
            .find(|&c| ci.is_some() && net_of(nl, c, "I3") == ci && const_of(nl, ci).is_none())
            .or_else(|| (cands.len() == 1).then(|| cands[0]));
        let lc = match pick {
            Some(lc) => lc,
            None => {
                let name = nl.str(nl.cells[ca].name).to_string();
                let lc = new_lc(nl, &format!("{name}$carry"));
                copy_pin(nl, ca, "I0", lc, "I1");
                copy_pin(nl, ca, "I1", lc, "I2");
                carry_only += 1;
                lc
            }
        };
        used.insert(lc);
        set_int(nl, lc, "CARRY_ENABLE", 1);
        match const_of(nl, ci) {
            Some(c) => {
                set_int(nl, lc, "CIN_CONST", 1);
                set_int(nl, lc, "CIN_SET", (c == Const::One) as u64);
            }
            None => move_pin(nl, ca, "CI", lc, "CIN"),
        }
        move_pin(nl, ca, "CO", lc, "COUT");
        nl.remove_cell(ca);
    }
    if carry_only > 0 {
        report.messages.push(format!("{carry_only} logic cells used for carry only"));
    }

    // Build chains: follow COUT -> CIN links.
    let lcs: Vec<CellId> = nl
        .live_cells()
        .filter(|&c| nl.cell_kind(c) == LC && get_int(nl, c, "CARRY_ENABLE") == 1)
        .collect();
    let next_of = |nl: &Netlist, c: CellId| -> Option<CellId> {
        let n = net_of(nl, c, "COUT")?;
        nl.nets[n]
            .users
            .iter()
            .find(|p| {
                nl.cell_kind(p.cell) == LC
                    && nl.str(nl.cells[p.cell].pins[p.pin as usize].name) == "CIN"
            })
            .map(|p| p.cell)
    };
    let mut is_successor: FxHashSet<CellId> = FxHashSet::default();
    for &c in &lcs {
        if let Some(n) = next_of(nl, c) {
            is_successor.insert(n);
        }
    }
    let mut chains = Vec::new();
    let mut feed_ins = 0;
    let mut feed_outs = 0;
    let mut tails: FxHashSet<CellId> = FxHashSet::default();
    for &start in &lcs {
        if is_successor.contains(&start) {
            continue;
        }
        let mut chain = Vec::new();
        // A CIN driven from the fabric needs a feed-in cell: with I1 = I2 =
        // signal and a constant-0 carry input, COUT equals the signal.
        if let Some(cin) = net_of(nl, start, "CIN") {
            let name = nl.str(nl.cells[start].name).to_string();
            let feed = new_lc(nl, &format!("{name}$feedin"));
            let p = pin_ref(nl, start, "CIN").unwrap();
            nl.disconnect(p);
            let i1 = pin_ref(nl, feed, "I1").unwrap();
            let i2 = pin_ref(nl, feed, "I2").unwrap();
            nl.connect(i1, cin).map_err(|e| e.0)?;
            nl.connect(i2, cin).map_err(|e| e.0)?;
            let link = nl.add_net(&format!("{name}$feedin_net"));
            nl.connect(pin_ref(nl, feed, "COUT").unwrap(), link).map_err(|e| e.0)?;
            nl.connect(p, link).map_err(|e| e.0)?;
            set_int(nl, feed, "CARRY_ENABLE", 1);
            set_int(nl, feed, "CIN_CONST", 1);
            chain.push(feed);
            feed_ins += 1;
        }
        let mut cur = Some(start);
        while let Some(c) = cur {
            chain.push(c);
            let next = next_of(nl, c);
            // COUT users other than the next cell's CIN, or the next cell's
            // I3 (reachable through the dedicated cout -> in_3 pip), need a
            // feed-out cell whose LUT passes I3 through to the fabric.
            if let Some(cout) = net_of(nl, c, "COUT") {
                let external: Vec<PinRef> = nl.nets[cout]
                    .users
                    .iter()
                    .copied()
                    .filter(|p| {
                        let pin = nl.str(nl.cells[p.cell].pins[p.pin as usize].name);
                        !(Some(p.cell) == next && (pin == "CIN" || pin == "I3"))
                    })
                    .collect();
                // At the end of a chain, a single plain cell reading the carry
                // on I3 (the top sum LUT of an adder without carry out) goes
                // in the next position instead of behind a feed-out cell.
                if next.is_none()
                    && let [p] = external[..]
                    && nl.str(nl.cells[p.cell].pins[p.pin as usize].name) == "I3"
                    && get_int(nl, p.cell, "CARRY_ENABLE") != 1
                    && net_of(nl, p.cell, "CIN").is_none()
                    && net_of(nl, p.cell, "COUT").is_none()
                    && tails.insert(p.cell)
                {
                    chain.push(p.cell);
                    break;
                }
                if !external.is_empty() {
                    let name = nl.str(nl.cells[c].name).to_string();
                    let feed = new_lc(nl, &format!("{name}$feedout"));
                    nl.connect(pin_ref(nl, feed, "I3").unwrap(), cout).map_err(|e| e.0)?;
                    set_int(nl, feed, "LUT_INIT", 0xFF00);
                    let out = nl.add_net(&format!("{name}$feedout_net"));
                    nl.connect(pin_ref(nl, feed, "O").unwrap(), out).map_err(|e| e.0)?;
                    for p in external {
                        nl.disconnect(p);
                        nl.connect(p, out).map_err(|e| e.0)?;
                    }
                    if let Some(n) = next {
                        // Keep the carry going through the feed-out cell:
                        // COUT = maj(0, 1, CIN) = CIN.
                        set_int(nl, feed, "CARRY_ENABLE", 1);
                        let p = pin_ref(nl, n, "CIN").unwrap();
                        nl.disconnect(p);
                        let i2 = pin_ref(nl, feed, "I2").unwrap();
                        let vcc = nl.add_const_net(Const::One);
                        nl.connect(i2, vcc).map_err(|e| e.0)?;
                        let link = nl.add_net(&format!("{name}$feedout_carry"));
                        nl.connect(pin_ref(nl, feed, "COUT").unwrap(), link).map_err(|e| e.0)?;
                        nl.connect(p, link).map_err(|e| e.0)?;
                        // The next cell's I3 must read the carry from its
                        // immediate predecessor, now the feed-out cell.
                        if net_of(nl, n, "I3") == Some(cout) {
                            let i3 = pin_ref(nl, n, "I3").unwrap();
                            nl.disconnect(i3);
                            nl.connect(i3, link).map_err(|e| e.0)?;
                        }
                    }
                    // The feed-out reads the carry through CIN.
                    let cin = pin_ref(nl, feed, "CIN").unwrap();
                    nl.connect(cin, cout).map_err(|e| e.0)?;
                    chain.push(feed);
                    feed_outs += 1;
                }
            }
            cur = next;
        }
        chains.push(chain);
    }
    if feed_ins + feed_outs > 0 {
        report
            .messages
            .push(format!("{feed_ins} carry feed-in and {feed_outs} feed-out cells inserted"));
    }
    Ok(chains)
}

/// Renames block RAMs and records clock polarities.
fn pack_ram(nl: &mut Netlist) -> Result<(), String> {
    let rams: Vec<CellId> =
        nl.live_cells().filter(|&c| nl.cell_kind(c).starts_with("SB_RAM40_4K")).collect();
    for c in rams {
        let kind = nl.cell_kind(c).to_string();
        let (nr, nw) = match kind.as_str() {
            "SB_RAM40_4K" => (false, false),
            "SB_RAM40_4KNR" => (true, false),
            "SB_RAM40_4KNW" => (false, true),
            "SB_RAM40_4KNRNW" => (true, true),
            other => return Err(format!("unsupported RAM type {other}")),
        };
        let k = nl.sym(RAM);
        nl.cells[c].kind = k;
        set_int(nl, c, "NEG_CLK_R", nr as u64);
        set_int(nl, c, "NEG_CLK_W", nw as u64);
        // Unused clock enables and enables default to active.
        for p in ["RCLKE", "WCLKE", "RE", "WE"] {
            if pin_ref(nl, c, p).is_none() {
                let one = nl.add_const_net(Const::One);
                nl.add_pin(c, p, Dir::Input, Some(one)).map_err(|e| e.0)?;
            }
        }
    }
    Ok(())
}

/// Folds LUT inputs tied to constants into the truth table (except carry
/// operands, which must be physically present on I1/I2).
fn fold_lut_constants(nl: &mut Netlist) {
    let lcs: Vec<CellId> = nl.live_cells().filter(|&c| nl.cell_kind(c) == LC).collect();
    for c in lcs {
        let carry = get_int(nl, c, "CARRY_ENABLE") == 1;
        let mut init = get_int(nl, c, "LUT_INIT") as u16;
        for (k, pin) in LC_INPUTS.iter().enumerate() {
            if carry && (k == 1 || k == 2) {
                continue;
            }
            let Some(v) = const_of(nl, net_of(nl, c, pin)) else { continue };
            let one = v == Const::One;
            // Replace the truth table by its cofactor with input k = v.
            let mut new = 0u16;
            for i in 0..16 {
                let src = if one { i | (1 << k) } else { i & !(1 << k) };
                if init >> src & 1 != 0 {
                    new |= 1 << i;
                }
            }
            init = new;
            let p = pin_ref(nl, c, pin).unwrap();
            nl.disconnect(p);
        }
        set_int(nl, c, "LUT_INIT", init as u64);
    }
}

/// Removes constant connections that the hardware provides by default and
/// drives the remaining ones from constant-generating logic cells.
fn drive_constants(nl: &mut Netlist) -> Result<(), String> {
    let const_nets: Vec<NetId> =
        nl.live_nets().filter(|&n| nl.nets[n].constant.is_some()).collect();
    let mut drivers: FxHashMap<bool, NetId> = FxHashMap::default();
    for n in const_nets {
        let value = nl.nets[n].constant.unwrap() == Const::One;
        let users = nl.nets[n].users.clone();
        for p in users {
            let kind = nl.cell_kind(p.cell).to_string();
            let pin = nl.str(nl.cells[p.cell].pins[p.pin as usize].name).to_string();
            let default_ok = match (kind.as_str(), pin.as_str()) {
                // Unconnected LC data inputs read 0, CEN reads 1, SR reads 0.
                (LC, "I0" | "I1" | "I2" | "I3") => !value,
                (LC, "CEN") => value,
                (LC, "SR") => !value,
                _ => false,
            };
            nl.disconnect(p);
            if default_ok {
                continue;
            }
            let net = match drivers.get(&value) {
                Some(&net) => net,
                None => {
                    let lc = new_lc(nl, if value { "$PACKER_VCC" } else { "$PACKER_GND" });
                    set_int(nl, lc, "LUT_INIT", if value { 0xFFFF } else { 0 });
                    let net = nl.add_net(if value { "$PACKER_VCC_NET" } else { "$PACKER_GND_NET" });
                    nl.connect(pin_ref(nl, lc, "O").unwrap(), net).map_err(|e| e.0)?;
                    drivers.insert(value, net);
                    net
                }
            };
            nl.connect(p, net).map_err(|e| e.0)?;
        }
        nl.nets[n].alive = false;
    }
    Ok(())
}

/// Which global networks can drive a control pin directly.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GlobalClass {
    /// Clock pins: any global network.
    Clock,
    /// Set/reset-type pins: even global networks.
    Reset,
    /// Clock-enable-type pins: odd global networks.
    Enable,
}

/// Classifies a cell pin as a global-network control input.
pub fn global_class(kind: &str, pin: &str) -> Option<GlobalClass> {
    match (kind, pin) {
        (LC, "CLK") | (RAM, "RCLK" | "WCLK") | (IO, "INPUT_CLK" | "OUTPUT_CLK") => {
            Some(GlobalClass::Clock)
        }
        (LC, "SR") | (RAM, "RE" | "WE") | (IO, "LATCH_INPUT_VALUE") => Some(GlobalClass::Reset),
        (LC, "CEN") | (RAM, "RCLKE" | "WCLKE") | (IO, "CLOCK_ENABLE") => Some(GlobalClass::Enable),
        _ => None,
    }
}

/// Inserts `SB_GB` global buffers for clocks and high-fanout control nets
/// (at most eight globals, counting explicitly instantiated ones).
///
/// A global network reaches clock pins everywhere, but only even networks
/// reach set/reset pins and only odd ones clock enables, so each promoted
/// net carries a single class of users; the others stay on fabric routing.
fn promote_globals(nl: &mut Netlist, report: &mut PackReport) -> Result<(), String> {
    let mut used = nl.live_cells().filter(|&c| nl.cell_kind(c) == GB).count();
    let mut candidates: Vec<(usize, GlobalClass, NetId)> = Vec::new();
    for n in nl.live_nets() {
        let net = &nl.nets[n];
        let Some(drv) = net.driver else { continue };
        // Constant drivers need no global network.
        let dname = nl.str(nl.cells[drv.cell].name);
        if nl.cell_kind(drv.cell) == GB || dname == "$PACKER_VCC" || dname == "$PACKER_GND" {
            continue;
        }
        let (mut clocks, mut resets, mut enables) = (0, 0, 0);
        for p in &net.users {
            let pin = port_base(nl.str(nl.cells[p.cell].pins[p.pin as usize].name));
            match global_class(nl.cell_kind(p.cell), pin) {
                Some(GlobalClass::Clock) => clocks += 1,
                Some(GlobalClass::Reset) => resets += 1,
                Some(GlobalClass::Enable) => enables += 1,
                None => {}
            }
        }
        if clocks > 0 {
            candidates.push((clocks + 1_000_000, GlobalClass::Clock, n));
        } else if resets.max(enables) > 32 {
            let class = if resets >= enables { GlobalClass::Reset } else { GlobalClass::Enable };
            candidates.push((resets.max(enables), class, n));
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.2.cmp(&b.2)));
    // Only the four even networks reach set/reset pins and the four odd
    // ones clock enables.
    let (mut resets, mut enables) = (0, 0);
    for (_, class, n) in candidates {
        let class_full = match class {
            GlobalClass::Reset => resets >= 4,
            GlobalClass::Enable => enables >= 4,
            GlobalClass::Clock => false,
        };
        if class_full {
            continue;
        }
        if used >= 8 {
            if class == GlobalClass::Clock {
                report.messages.push(format!(
                    "warning: clock {} not promoted to a global network (all 8 in use)",
                    nl.str(nl.nets[n].name)
                ));
            }
            continue;
        }
        used += 1;
        match class {
            GlobalClass::Reset => resets += 1,
            GlobalClass::Enable => enables += 1,
            GlobalClass::Clock => {}
        }
        let name = nl.str(nl.nets[n].name).to_string();
        let gb = nl.add_cell(&format!("{name}$gb"), GB);
        let gnet = nl.add_net(&format!("{name}$glb"));
        nl.add_pin(gb, "USER_SIGNAL_TO_GLOBAL_BUFFER", Dir::Input, None).unwrap();
        nl.add_pin(gb, "GLOBAL_BUFFER_OUTPUT", Dir::Output, Some(gnet)).unwrap();
        let users = nl.nets[n].users.clone();
        for p in users {
            let kind = nl.cell_kind(p.cell).to_string();
            let pin = port_base(nl.str(nl.cells[p.cell].pins[p.pin as usize].name)).to_string();
            if global_class(&kind, &pin) == Some(class) {
                nl.disconnect(p);
                nl.connect(p, gnet).map_err(|e| e.0)?;
            }
        }
        nl.connect(pin_ref(nl, gb, "USER_SIGNAL_TO_GLOBAL_BUFFER").unwrap(), n).map_err(|e| e.0)?;
        report.messages.push(format!("promoted {name} to a global network ({class:?})"));
    }
    Ok(())
}
