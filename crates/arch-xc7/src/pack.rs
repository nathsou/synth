//! Slice packing with shared control-set legality and explicit carry macros.
//! LUT RAM and shift registers need a SLICEM; LUT RAM and the wide-function
//! multiplexers also fix their LUTs to particular lanes of one slice.
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
/// The write clock and enable shared by a SLICEM's LUT RAM and shift
/// registers (the clock also clocks the slice's flip-flops).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Memory {
    pub clock: NetId,
    pub inverted: bool,
    pub we: NetId,
}
#[derive(Clone, Debug, Default)]
pub struct Slice {
    /// LUTs or shift registers, lane A first.
    pub luts: [Option<CellId>; 4],
    /// LUTs on the lanes' O5 outputs: a lane's LUT6 is then two LUTs of up
    /// to five shared inputs (A6 high), the O6 one in `luts`.
    pub lut5: [Option<CellId>; 4],
    /// Flip-flops: AFF-DFF, then the second bank A5FF-D5FF, whose outputs
    /// leave through the lanes' output multiplexers.
    pub ffs: [Option<CellId>; 8],
    pub carry: Option<CellId>,
    pub controls: Option<Controls>,
    /// A RAM32M or RAM64M, which takes all four LUTs.
    pub ram: Option<CellId>,
    /// MUXF7s of lanes A/B (selected by AX) and C/D (by CX).
    pub f7: [Option<CellId>; 2],
    /// The MUXF8 of the two MUXF7s (selected by BX).
    pub f8: Option<CellId>,
    pub memory: Option<Memory>,
}
impl Slice {
    /// Whether the carry's DI input of `lane` comes from the lane's O5.
    pub fn di_from_o5(&self, nl: &Netlist, lane: usize) -> bool {
        let Some(c) = self.carry else { return false };
        self.lut5[lane].is_some_and(|l| {
            nl.pin_net(l, "O").is_some()
                && nl.pin_net(l, "O") == nl.pin_net(c, &format!("DI[{lane}]"))
        })
    }
    /// Whether the slice needs a SLICEM site.
    pub fn needs_m(&self, nl: &Netlist) -> bool {
        self.ram.is_some() || self.luts.iter().flatten().any(|&c| is_srl(nl.cell_kind(c)))
    }
    fn lane_free(&self, lane: usize) -> bool {
        self.ram.is_none() && self.luts[lane].is_none() && self.lut5[lane].is_none()
    }
    /// The input nets of lane `lane`'s LUT site, O6 LUT first (physical
    /// inputs A1, A2, ... in order).
    pub fn lane_inputs(&self, nl: &Netlist, lane: usize) -> Vec<NetId> {
        let mut nets: Vec<NetId> = Vec::new();
        for c in
            self.luts[lane].iter().chain(&self.lut5[lane]).filter(|&&c| is_lut(nl.cell_kind(c)))
        {
            for i in 0..lut_size(nl.cell_kind(*c)) {
                if let Some(n) = nl.pin_net(*c, &format!("I{i}"))
                    && !nets.contains(&n)
                {
                    nets.push(n);
                }
            }
        }
        nets
    }
    /// Whether a LUT reading `inputs` fits on lane `lane`'s O5 output: the
    /// lane's LUT takes at most five inputs and both fit in A1-A5.
    fn o5_fits(&self, nl: &Netlist, lane: usize, inputs: &[NetId]) -> bool {
        if self.ram.is_some() || self.carry.is_some() || self.lut5[lane].is_some() {
            return false;
        }
        if self.f8.is_some() || self.f7[lane / 2].is_some() {
            return false;
        }
        if self.luts[lane]
            .is_some_and(|c| !is_lut(nl.cell_kind(c)) || lut_size(nl.cell_kind(c)) > 5)
        {
            return false;
        }
        let mut nets = self.lane_inputs(nl, lane);
        for &n in inputs {
            if !nets.contains(&n) {
                nets.push(n);
            }
        }
        nets.len() <= 5
    }
    /// Whether the lane's X input is free for a flip-flop bypass: carries
    /// take every DI, multiplexers their selects, RAM32M and SRL16E data.
    fn x_free(&self, nl: &Netlist, lane: usize) -> bool {
        self.carry.is_none()
            && !(lane == 0 && self.f7[0].is_some()
                || lane == 1 && self.f8.is_some()
                || lane == 2 && self.f7[1].is_some())
            && self.ram.is_none_or(|r| nl.cell_kind(r) != "RAM32M")
            && self.luts[lane].is_none_or(|c| nl.cell_kind(c) != "SRL16E")
    }
    /// Whether lane `lane`'s second flip-flop can be used: its output takes
    /// the lane's output multiplexer, which carries, wide multiplexers and
    /// RAM32M bit 0 may need.
    fn ff5_free(&self, nl: &Netlist, lane: usize) -> bool {
        self.ffs[lane + 4].is_none()
            && self.lut5[lane].is_none()
            && self.carry.is_none()
            && self.f8.is_none()
            && self.f7[lane / 2].is_none()
            && self.ram.is_none_or(|r| nl.cell_kind(r) != "RAM32M")
    }
    /// The net on lane `lane`'s X input, if a flip-flop takes it.
    fn x_net(&self, nl: &Netlist, lane: usize) -> Option<NetId> {
        [lane, lane + 4].into_iter().find_map(|k| {
            let c = self.ffs[k]?;
            (ff_mux(nl, self, k, c) == "BYPASS").then(|| nl.pin_net(c, "D")).flatten()
        })
    }
    /// Whether flip-flops with controls `ctrl` may join the slice.
    fn accepts(&self, ctrl: &Controls) -> bool {
        self.controls.as_ref().is_none_or(|x| x == ctrl)
            && self
                .memory
                .as_ref()
                .is_none_or(|m| m.clock == ctrl.clock && m.inverted == ctrl.inverted)
    }
    /// Whether LUT RAM or shift registers with `memory` may join the slice.
    fn accepts_memory(&self, memory: &Memory) -> bool {
        self.memory.as_ref().is_none_or(|m| m == memory)
            && self
                .controls
                .as_ref()
                .is_none_or(|c| c.clock == memory.clock && c.inverted == memory.inverted)
    }
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
/// The number of inputs of a LUT kind.
pub fn lut_size(k: &str) -> usize {
    k.strip_prefix("LUT").and_then(|n| n.parse().ok()).unwrap_or(0)
}
pub fn is_srl(k: &str) -> bool {
    matches!(k, "SRLC32E" | "SRL16E")
}
pub fn is_ram(k: &str) -> bool {
    matches!(k, "RAM32M" | "RAM64M")
}
/// The lane of a LUT RAM output pin (`DOA`, `DOB[1]`, ...) and whether it is
/// the O5 output (bit 0 of a RAM32M port).
pub fn ram_output(nl: &Netlist, ram: CellId, pin: &str) -> Option<(usize, bool)> {
    let rest = pin.strip_prefix("DO")?;
    let lane = usize::from(rest.as_bytes().first()?.checked_sub(b'A')?);
    let o5 = nl.cell_kind(ram) == "RAM32M" && rest.get(1..) == Some("[0]");
    (lane < 4).then_some((lane, o5))
}
/// The input multiplexer setting of flip-flop slot `k` (lane `k % 4`): a
/// lane's LUT (O6 or O5), carry sum, wide multiplexer, or the bypass (X)
/// input. The second bank can only take O5 or the bypass.
pub fn ff_mux(nl: &Netlist, slice: &Slice, k: usize, ff: CellId) -> &'static str {
    let lane = k % 4;
    let Some(d) = nl.pin_net(ff, "D").and_then(|n| nl.nets[n].driver) else { return "BYPASS" };
    let pin = nl.str(nl.cells[d.cell].pins[d.pin as usize].name);
    if k >= 4 {
        return if Some(d.cell) == slice.lut5[lane] { "O5" } else { "BYPASS" };
    }
    if Some(d.cell) == slice.luts[lane] {
        "O6"
    } else if Some(d.cell) == slice.carry {
        "XOR"
    } else if Some(d.cell) == slice.ram {
        match ram_output(nl, d.cell, pin) {
            Some((l, o5)) if l == lane => {
                if o5 {
                    "O5"
                } else {
                    "O6"
                }
            }
            _ => "BYPASS",
        }
    } else if lane == 0 && Some(d.cell) == slice.f7[0] || lane == 2 && Some(d.cell) == slice.f7[1] {
        "F7"
    } else if lane == 1 && Some(d.cell) == slice.f8 {
        "F8"
    } else {
        "BYPASS"
    }
}
/// The lane whose flip-flop can take output `pin` of `c` inside the slice.
fn internal_lane(nl: &Netlist, p: &Packed, c: CellId, pin: &str) -> Option<usize> {
    let &(s, l) = p.slots.get(&c)?;
    let slice = &p.slices[s];
    match nl.cell_kind(c) {
        "CARRY4" => pin.strip_prefix("O[")?.strip_suffix(']')?.parse().ok(),
        // Only O6 reaches the first bank.
        k if is_lut(k) || is_srl(k) => (slice.luts[l] == Some(c)).then_some(l),
        k if is_ram(k) => ram_output(nl, c, pin).map(|(l, _)| l),
        "MUXF7" => Some(if slice.f7[0] == Some(c) { 0 } else { 2 }),
        "MUXF8" => Some(1),
        _ => None,
    }
}
fn memory(nl: &Netlist, c: CellId) -> Result<Memory, String> {
    let (clk, we, inv) = if is_ram(nl.cell_kind(c)) {
        ("WCLK", "WE", "IS_WCLK_INVERTED")
    } else {
        ("CLK", "CE", "IS_CLK_INVERTED")
    };
    let name = || nl.str(nl.cells[c].name).to_string();
    Ok(Memory {
        clock: nl.pin_net(c, clk).ok_or_else(|| format!("{} has no clock", name()))?,
        inverted: param(nl, c, inv, 0) != 0,
        we: nl.pin_net(c, we).ok_or_else(|| format!("{} has no write enable", name()))?,
    })
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

// A shared propagate function may be needed at several dedicated carry
// inputs. Replicate the function into each required LUT slot; buffering its
// output would introduce an unnecessary extra LUT level and routed net.
fn replicate_lut(nl: &mut Netlist, original: CellId) -> Result<CellId, String> {
    let old = nl.cells[original].clone();
    let kind = nl.cell_kind(original).to_string();
    let c = nl.add_cell("$carry_replicated_lut", &kind);
    nl.cells[c].params = old.params;
    nl.cells[c].attrs = old.attrs;
    for pin in old.pins {
        if pin.dir == Dir::Input {
            let name = nl.str(pin.name).to_string();
            nl.add_pin(c, &name, Dir::Input, pin.net).map_err(|e| e.to_string())?;
        }
    }
    let o = nl.add_net("$carry_replicated_lut_o");
    output(nl, c, "O", o)?;
    Ok(c)
}

/// A LUT for the dedicated lane input `pin` of `c` (a carry S or a MUXF7
/// data input): its driver if that LUT is still free, else a copy of it or
/// a buffer.
fn lane_lut(nl: &mut Netlist, p: &Packed, c: CellId, pin: &str) -> Result<CellId, String> {
    let n = nl.pin_net(c, pin).ok_or_else(|| format!("missing {pin} input"))?;
    let existing = nl.nets[n].driver.map(|d| d.cell).filter(|d| is_lut(nl.cell_kind(*d)));
    Ok(match existing {
        Some(l) if !p.slots.contains_key(&l) => l,
        driver => {
            let l = match driver {
                Some(l) => replicate_lut(nl, l)?,
                None => buffer(nl, n)?,
            };
            rewire(nl, c, pin, nl.pin_net(l, "O").unwrap())?;
            l
        }
    })
}

/// A LUT for carry `c`'s DI input of `lane` on the lane's O5 output, which
/// the carry reads inside the slice instead of through the X input: the
/// constant, the DI net's own LUT (or a copy, if it has other readers), or
/// a pass-through of a net the propagate LUT already reads, if its inputs
/// fit beside the propagate LUT's in A1-A5.
fn di_lut(
    nl: &mut Netlist,
    p: &Packed,
    slice: &Slice,
    c: CellId,
    lane: usize,
) -> Result<Option<CellId>, String> {
    let Some(s) = slice.luts[lane] else { return Ok(None) };
    if !is_lut(nl.cell_kind(s)) || lut_size(nl.cell_kind(s)) > 5 {
        return Ok(None);
    }
    let pin = format!("DI[{lane}]");
    let d = nl.pin_net(c, &pin).ok_or("carry missing DI")?;
    let mut inputs = slice.lane_inputs(nl, lane);
    let driver = nl.nets[d].driver.map(|x| x.cell).filter(|&x| is_lut(nl.cell_kind(x)));
    let lut = if value(nl, d).is_some() {
        if !inputs.contains(&d) && inputs.len() == 5 {
            return Ok(None);
        }
        buffer(nl, d)?
    } else if let Some(l) = driver.filter(|&l| lut_size(nl.cell_kind(l)) <= 5) {
        for i in 0..lut_size(nl.cell_kind(l)) {
            if let Some(n) = nl.pin_net(l, &format!("I{i}"))
                && !inputs.contains(&n)
            {
                inputs.push(n);
            }
        }
        if inputs.len() > 5 {
            return Ok(None);
        }
        if !p.slots.contains_key(&l) && nl.nets[d].users.len() == 1 {
            return Ok(Some(l));
        }
        replicate_lut(nl, l)?
    } else if inputs.contains(&d) {
        buffer(nl, d)?
    } else {
        return Ok(None);
    };
    rewire(nl, c, &pin, nl.pin_net(lut, "O").unwrap())?;
    Ok(Some(lut))
}

pub fn pack(nl: &mut Netlist) -> Result<Packed, String> {
    nl.check().map_err(|e| e.to_string())?;
    // An inverter is a one-input LUT.
    let inverters: Vec<_> = nl.live_cells().filter(|&c| nl.cell_kind(c) == "INV").collect();
    for c in inverters {
        let (i, o) = (nl.pin_net(c, "I"), nl.pin_net(c, "O"));
        nl.remove_cell(c);
        let lut = nl.add_cell("$inverter", "LUT1");
        nl.set_param(lut, "INIT", Param::Int(1));
        nl.add_pin(lut, "I0", Dir::Input, i).map_err(|e| e.to_string())?;
        nl.add_pin(lut, "O", Dir::Output, o).map_err(|e| e.to_string())?;
    }
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
    // LUT RAM takes a whole SLICEM.
    let rams: Vec<_> = nl.live_cells().filter(|&c| is_ram(nl.cell_kind(c))).collect();
    for c in rams {
        p.slots.insert(c, (p.slices.len(), 0));
        p.slices.push(Slice { ram: Some(c), memory: Some(memory(nl, c)?), ..Slice::default() });
    }
    // Wide multiplexers: a MUXF8 takes a whole slice, a MUXF7 half of one.
    // Their data inputs are the O6 outputs of fixed lanes.
    let f8s: Vec<_> = nl.live_cells().filter(|&c| nl.cell_kind(c) == "MUXF8").collect();
    for c in f8s {
        let idx = p.slices.len();
        let mut slice = Slice { f8: Some(c), ..Slice::default() };
        for (half, pin) in ["I1", "I0"].into_iter().enumerate() {
            let f7 = nl
                .pin_net(c, pin)
                .and_then(|n| nl.nets[n].driver)
                .map(|d| d.cell)
                .filter(|&d| nl.cell_kind(d) == "MUXF7" && !p.slots.contains_key(&d))
                .ok_or("MUXF8 inputs must be distinct MUXF7 outputs")?;
            slice.f7[half] = Some(f7);
            p.slots.insert(f7, (idx, 2 * half));
        }
        p.slots.insert(c, (idx, 1));
        p.slices.push(slice);
    }
    let f7s: Vec<_> = nl
        .live_cells()
        .filter(|&c| nl.cell_kind(c) == "MUXF7" && !p.slots.contains_key(&c))
        .collect();
    for (i, c) in f7s.into_iter().enumerate() {
        if i % 2 == 0 {
            p.slices.push(Slice::default());
        }
        let idx = p.slices.len() - 1;
        p.slices[idx].f7[i % 2] = Some(c);
        p.slots.insert(c, (idx, 2 * (i % 2)));
    }
    for idx in 0..p.slices.len() {
        for half in 0..2 {
            let Some(f7) = p.slices[idx].f7[half] else { continue };
            // MUXF7 output = S ? I1 : I0, with I1 from lane A (C), I0 from B (D).
            for (pin, lane) in [("I1", 2 * half), ("I0", 2 * half + 1)] {
                let lut = lane_lut(nl, &p, f7, pin)?;
                p.slices[idx].luts[lane] = Some(lut);
                p.slots.insert(lut, (idx, lane));
            }
        }
    }
    let carries: Vec<_> = nl.live_cells().filter(|&c| nl.cell_kind(c) == "CARRY4").collect();
    for c in carries {
        let idx = p.slices.len();
        let mut slice = Slice { carry: Some(c), ..Slice::default() };
        p.slots.insert(c, (idx, 0));
        // A lane whose outputs, and those of the lanes above, are unused
        // needs no propagate input: its LUT site stays free for any LUT,
        // whose O6 then drives an S that nothing observes.
        let used = |pin: String| nl.pin_net(c, &pin).is_some_and(|n| !nl.nets[n].users.is_empty());
        let live: Vec<bool> = (0..4)
            .map(|lane| (lane..4).any(|k| used(format!("O[{k}]")) || used(format!("CO[{k}]"))))
            .collect();
        for lane in (0..4).filter(|&l| live[l]) {
            let lut = lane_lut(nl, &p, c, &format!("S[{lane}]"))?;
            slice.luts[lane] = Some(lut);
            p.slots.insert(lut, (idx, lane));
            if let Some(lut) = di_lut(nl, &p, &slice, c, lane)? {
                slice.lut5[lane] = Some(lut);
                p.slots.insert(lut, (idx, lane));
            }
        }
        p.slices.push(slice);
    }
    // Shift registers share their SLICEM's clock and write enable.
    let srls: Vec<_> = nl.live_cells().filter(|&c| is_srl(nl.cell_kind(c))).collect();
    for c in srls {
        let m = memory(nl, c)?;
        let loc = p.slices.iter().enumerate().find_map(|(s, slice)| {
            (slice.accepts_memory(&m) && slice.carry.is_none())
                .then(|| (0..4).find(|&l| slice.lane_free(l)).map(|l| (s, l)))
                .flatten()
        });
        let (s, l) = loc.unwrap_or_else(|| {
            p.slices.push(Slice::default());
            (p.slices.len() - 1, 0)
        });
        p.slices[s].memory = Some(m);
        p.slices[s].luts[l] = Some(c);
        p.slots.insert(c, (s, l));
    }
    let ffs: Vec<_> = nl.live_cells().filter(|&c| is_ff(nl.cell_kind(c))).collect();
    for c in ffs {
        let ctrl = controls(nl, c)?;
        let d = nl.pin_net(c, "D").ok_or("FF missing D")?;
        let driver = nl.nets[d].driver.map(|p| p.cell);
        let paired = nl.nets[d].driver.and_then(|drv| {
            let s = p.slots.get(&drv.cell)?.0;
            let pin = nl.str(nl.cells[drv.cell].pins[drv.pin as usize].name);
            let l = internal_lane(nl, &p, drv.cell, pin)?;
            (p.slices[s].ffs[l].is_none() && p.slices[s].accepts(&ctrl)).then_some((s, l))
        });
        let (s, l) = if let Some(loc) = paired {
            loc
        } else {
            let lut = driver.filter(|d| is_lut(nl.cell_kind(*d)) && !p.slots.contains_key(d));
            // A lane's X input can feed both its flip-flops the same net.
            let x_ok = |slice: &Slice, l: usize| {
                slice.x_free(nl, l) && slice.x_net(nl, l).is_none_or(|n| n == d)
            };
            // Slots 8-11 stand for the second bank fed through the lane's
            // O5 output, a LUT route-through, when the X input is taken.
            let loc = p.slices.iter().enumerate().find_map(|(s, slice)| {
                if !slice.accepts(&ctrl) {
                    return None;
                }
                (0..12)
                    .find(|&k| {
                        if lut.is_some() {
                            k < 4 && slice.ffs[k].is_none() && slice.lane_free(k)
                        } else if k < 4 {
                            slice.ffs[k].is_none() && x_ok(slice, k)
                        } else if k < 8 {
                            slice.ff5_free(nl, k - 4) && x_ok(slice, k - 4)
                        } else {
                            slice.ff5_free(nl, k - 8) && slice.o5_fits(nl, k - 8, &[d])
                        }
                    })
                    .map(|k| (s, k))
            });
            let (s, k) = loc.unwrap_or_else(|| {
                p.slices.push(Slice::default());
                (p.slices.len() - 1, 0)
            });
            if let Some(lut) = lut {
                p.slices[s].luts[k] = Some(lut);
                p.slots.insert(lut, (s, k));
            }
            if k >= 8 {
                let through = buffer(nl, d)?;
                rewire(nl, c, "D", nl.pin_net(through, "O").unwrap())?;
                p.slices[s].lut5[k - 8] = Some(through);
                p.slots.insert(through, (s, k - 8));
            }
            (s, if k >= 8 { k - 4 } else { k })
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
            .find_map(|(s, x)| (0..4).find(|&l| x.lane_free(l)).map(|l| (s, l)));
        if let Some((s, l)) = loc {
            p.slices[s].luts[l] = Some(c);
            p.slots.insert(c, (s, l));
            continue;
        }
        // With every lane taken, share a LUT site rather than open a
        // slice: on O5, beside the LUT sharing the most inputs. Its output
        // takes the lane's output multiplexer, so the second bank is free.
        let ins: Vec<NetId> = (0..lut_size(nl.cell_kind(c)))
            .filter_map(|i| nl.pin_net(c, &format!("I{i}")))
            .collect();
        let shared = |x: &Slice, l: usize| {
            let lane = x.lane_inputs(nl, l);
            ins.iter().filter(|n| lane.contains(n)).count()
        };
        // Constant sources are routed from their lane's O6 pin.
        let constant = p.constant_luts.values().any(|&k| k == c);
        let o5 = (ins.len() <= 5 && !constant)
            .then(|| {
                p.slices
                    .iter()
                    .enumerate()
                    .flat_map(|(s, x)| (0..4).map(move |l| (s, x, l)))
                    .filter(|&(_, x, l)| x.ffs[l + 4].is_none() && x.o5_fits(nl, l, &ins))
                    .max_by_key(|&(s, x, l)| (shared(x, l), std::cmp::Reverse((s, l))))
                    .map(|(s, _, l)| (s, l))
            })
            .flatten();
        if let Some((s, l)) = o5 {
            p.slices[s].lut5[l] = Some(c);
            p.slots.insert(c, (s, l));
        } else {
            p.slices.push(Slice::default());
            let s = p.slices.len() - 1;
            p.slices[s].luts[0] = Some(c);
            p.slots.insert(c, (s, 0));
        }
    }
    for c in nl.live_cells() {
        if nl.cell_kind(c) == "DSP48E1" {
            crate::physical_dsp::validate(nl, c)?;
        }
        let kind = nl.cell_kind(c);
        if !is_lut(kind)
            && !is_ff(kind)
            && !is_srl(kind)
            && !is_ram(kind)
            && !matches!(
                kind,
                "CARRY4" | "MUXF7" | "MUXF8" | "RAMB18E1" | "DSP48E1" | "$input" | "$output"
            )
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
    use super::*;

    fn cell(nl: &mut Netlist, kind: &str, ins: &[(&str, NetId)], outs: &[&str]) -> Vec<NetId> {
        let c = nl.add_cell(kind, kind);
        for &(pin, n) in ins {
            nl.add_pin(c, pin, Dir::Input, Some(n)).unwrap();
        }
        outs.iter()
            .map(|pin| {
                let n = nl.add_net(pin);
                nl.add_pin(c, pin, Dir::Output, Some(n)).unwrap();
                n
            })
            .collect()
    }

    fn lut(nl: &mut Netlist, a: NetId) -> NetId {
        let o = cell(nl, "LUT1", &[("I0", a)], &["O"])[0];
        let c = nl.nets[o].driver.unwrap().cell;
        nl.set_param(c, "INIT", Param::Int(1));
        o
    }

    fn ff(nl: &mut Netlist, clk: NetId, d: NetId) -> CellId {
        let zero = nl.add_const_net(Const::Zero);
        let one = nl.add_const_net(Const::One);
        let q = cell(nl, "FDRE", &[("C", clk), ("D", d), ("CE", one), ("R", zero)], &["Q"])[0];
        output(nl, q);
        nl.nets[q].driver.unwrap().cell
    }

    fn output(nl: &mut Netlist, n: NetId) {
        cell(nl, synth_netlist::OUTPUT_PORT, &[("PAD", n)], &[]);
    }

    #[test]
    fn wide_multiplexers_fix_their_lanes() {
        let mut nl = Netlist::new();
        let (clk, s7, s8) = (nl.add_net("clk"), nl.add_net("s7"), nl.add_net("s8"));
        let x = nl.add_net("x");
        let data: Vec<NetId> = (0..4).map(|_| lut(&mut nl, x)).collect();
        let a = cell(&mut nl, "MUXF7", &[("I0", data[1]), ("I1", data[0]), ("S", s7)], &["O"])[0];
        let b = cell(&mut nl, "MUXF7", &[("I0", data[3]), ("I1", data[2]), ("S", s7)], &["O"])[0];
        let o = cell(&mut nl, "MUXF8", &[("I0", b), ("I1", a), ("S", s8)], &["O"])[0];
        let r = ff(&mut nl, clk, o);
        // A lone MUXF7 whose inputs are the same LUT: one is replicated.
        let c = cell(&mut nl, "MUXF7", &[("I0", data[0]), ("I1", data[0]), ("S", s7)], &["O"])[0];
        output(&mut nl, c);
        let p = pack(&mut nl).unwrap();
        let (s, lane) = p.slots[&nl.nets[o].driver.unwrap().cell];
        assert_eq!(lane, 1);
        let slice = &p.slices[s];
        for (l, &d) in data.iter().enumerate() {
            assert_eq!(slice.luts[l], nl.nets[d].driver.map(|d| d.cell));
        }
        // The register takes the MUXF8 output directly, and the B bypass
        // (the MUXF8 select) is taken.
        assert_eq!(p.slots[&r], (s, 1));
        assert_eq!(ff_mux(&nl, slice, 1, r), "F8");
        assert!(!slice.x_free(&nl, 0) && !slice.x_free(&nl, 1) && !slice.x_free(&nl, 2));
        let lone = &p.slices[p.slots[&nl.nets[c].driver.unwrap().cell].0];
        let (i1, i0) = (lone.luts[0].unwrap(), lone.luts[1].unwrap());
        assert_ne!(i0, i1);
        assert_eq!(nl.cell_kind(i0), "LUT1");
        assert!(!lone.needs_m(&nl));
        nl.check().unwrap();
    }

    #[test]
    fn lut_ram_and_shift_registers_share_slicem_controls() {
        let mut nl = Netlist::new();
        let (clk, we, x) = (nl.add_net("clk"), nl.add_net("we"), nl.add_net("x"));
        let mut ins = vec![("WCLK", clk), ("WE", we)];
        let names: Vec<String> = "ABCD"
            .chars()
            .flat_map(|p| {
                (0..5)
                    .map(move |i| format!("ADDR{p}[{i}]"))
                    .chain((0..2).map(move |i| format!("DI{p}[{i}]")))
            })
            .collect();
        ins.extend(names.iter().map(|n| (n.as_str(), x)));
        let outs = cell(&mut nl, "RAM32M", &ins, &["DOA[0]", "DOA[1]", "DOB[0]"]);
        let ram = nl.nets[outs[0]].driver.unwrap().cell;
        let regs: Vec<_> = outs.iter().map(|&o| ff(&mut nl, clk, o)).collect();
        let one = nl.add_const_net(Const::One);
        let srl_ins = [
            ("CLK", clk),
            ("CE", we),
            ("D", x),
            ("A[0]", one),
            ("A[1]", one),
            ("A[2]", one),
            ("A[3]", one),
            ("A[4]", one),
        ];
        let srl = |nl: &mut Netlist, ins: &[(&str, NetId)]| {
            let q = cell(nl, "SRLC32E", ins, &["Q"])[0];
            nl.nets[q].driver.unwrap().cell
        };
        let srls = [srl(&mut nl, &srl_ins), srl(&mut nl, &srl_ins)];
        let other = [
            ("CLK", clk),
            ("CE", one),
            ("D", x),
            ("A[0]", one),
            ("A[1]", one),
            ("A[2]", one),
            ("A[3]", one),
            ("A[4]", one),
        ];
        let lone = srl(&mut nl, &other);
        let p = pack(&mut nl).unwrap();
        let s = p.slots[&ram].0;
        assert!(p.slices[s].needs_m(&nl) && p.slices[s].luts.iter().all(Option::is_none));
        // Bit 0 of port A leaves through O5 and bit 1 through O6, both to
        // lane A's register; the other registers take what they can.
        assert_eq!(p.slots[&regs[0]], (s, 0));
        assert_eq!(ff_mux(&nl, &p.slices[s], 0, regs[0]), "O5");
        assert_eq!(p.slots[&regs[2]], (s, 1));
        assert_ne!(p.slots[&regs[1]].0, s);
        // RAM32M takes the X inputs: no bypass registers there.
        assert!((0..4).all(|l| !p.slices[s].x_free(&nl, l)));
        // Shift registers with the same write enable share a slice.
        assert_eq!(p.slots[&srls[0]].0, p.slots[&srls[1]].0);
        assert_ne!(p.slots[&srls[0]].0, p.slots[&lone].0);
        assert!(p.slices[p.slots[&lone].0].needs_m(&nl));
        nl.check().unwrap();
    }

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
