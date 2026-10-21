//! Simulation models of the iCE40 primitives, as RTL cells (ADR 0008).
//!
//! Written from the iCE40 technology library documentation: `SB_LUT4`
//! looks its inputs up in `LUT_INIT`; `SB_CARRY` is a full adder's carry;
//! `SB_DFF*` flip-flops power up at 0, their synchronous set or reset acts
//! only when enabled and the asynchronous ones always; `SB_RAM40_4K` is
//! 256 words of 16 bits with registered reads, used 16, 8, 4 or 2 bits at a
//! time (`READ_MODE`/`WRITE_MODE` 0 to 3): in the narrow modes each group of
//! 2, 4 or 8 bits of a word holds one data bit per address, on a fixed data
//! pin of the group, and address bits 8 and up select the bit in the group.
//! I/O cells are modelled unregistered and global buffers as wires; PLLs
//! pass their reference clock through.

use synth_core::{Logic, LogicVec};
use synth_netlist::{Bit, CellId, Netlist};
use synth_rtl::{CellKind, Clock, Control, Dff, MemRead, MemWrite, Memory, Module, Sig, SigBit};

use crate::cells::dff_kind;

fn param_bits(nl: &Netlist, c: CellId, name: &str, width: usize) -> LogicVec {
    let bits: Vec<Logic> = match nl.param(c, name) {
        Some(p) => p
            .to_bits(width)
            .into_iter()
            .map(|b| if b == Bit::One { Logic::One } else { Logic::Zero })
            .collect(),
        None => vec![Logic::Zero; width],
    };
    LogicVec::from_bits(&bits)
}

fn param_u64(nl: &Netlist, c: CellId, name: &str) -> u64 {
    nl.param(c, name).and_then(|p| p.as_u64()).unwrap_or(0)
}

/// A bus of pins `name[0]..name[width - 1]` (missing ones read `missing`).
fn bus(pin: &dyn Fn(&str) -> Option<SigBit>, name: &str, width: usize, missing: SigBit) -> Sig {
    Sig((0..width).map(|i| pin(&format!("{name}[{i}]")).unwrap_or(missing)).collect())
}

/// Drives `out` (if connected) from `v`.
fn drive(m: &mut Module, out: Option<SigBit>, v: SigBit) {
    if let Some(o) = out {
        m.connect(Sig::bit(o), Sig::bit(v));
    }
}

/// Adds the behaviour of iCE40 primitive `c` to `m`. Returns `Ok(false)`
/// for a cell kind that is not an iCE40 primitive.
pub fn add_cell(
    m: &mut Module,
    nl: &Netlist,
    c: CellId,
    pin: &dyn Fn(&str) -> Option<SigBit>,
) -> Result<bool, String> {
    let kind = nl.cell_kind(c).to_string();
    let name = nl.str(nl.cells[c].name).to_string();
    let input = |p: &str, default: SigBit| pin(p).unwrap_or(default);
    match kind.as_str() {
        "SB_LUT4" => {
            let init = param_bits(nl, c, "LUT_INIT", 16);
            let idx =
                Sig(["I0", "I1", "I2", "I3"].iter().map(|p| input(p, SigBit::ZERO)).collect());
            if let Some(o) = pin("O") {
                m.add_cell(
                    name,
                    CellKind::Shiftx {
                        a: Sig::from_const(&init),
                        b: idx,
                        b_signed: false,
                        offset: 0,
                        y: Sig::bit(o),
                    },
                );
            }
        }
        "SB_CARRY" => {
            let (a, b, ci) =
                (input("I0", SigBit::ZERO), input("I1", SigBit::ZERO), input("CI", SigBit::ZERO));
            let ab = m.and1(a, b);
            let a_or_b = m.or1(a, b);
            let carried = m.and1(ci, a_or_b);
            let co = m.or1(ab, carried);
            drive(m, pin("CO"), co);
        }
        k if dff_kind(k).is_some() => {
            let dk = dff_kind(k).unwrap();
            let Some(q) = pin("Q") else { return Ok(true) };
            let ctl = |p: &str| Control { sig: input(p, SigBit::ZERO), pol: true };
            let sr = dk.has_sr.then(|| {
                let c = ctl(if dk.set { "S" } else { "R" });
                (c, LogicVec::from_bool(dk.set))
            });
            let (srst, arst) = if dk.async_sr { (None, sr) } else { (sr, None) };
            m.add_cell(
                name,
                CellKind::Dff(Box::new(Dff {
                    clk: Clock { sig: input("C", SigBit::ZERO), posedge: !dk.neg_clk },
                    d: Sig::bit(input("D", SigBit::ZERO)),
                    q: Sig::bit(q),
                    en: dk.enable.then(|| ctl("E")),
                    srst,
                    en_over_srst: true,
                    arst,
                    init: LogicVec::zeros(1),
                })),
            );
        }
        "SB_RAM40_4K" | "SB_RAM40_4KNR" | "SB_RAM40_4KNW" | "SB_RAM40_4KNRNW" => {
            ram(m, nl, c, pin, &kind, &name)?;
        }
        "SB_IO" | "SB_GB_IO" => {
            let pad = pin("PACKAGE_PIN");
            let pin_type = param_u64(nl, c, "PIN_TYPE");
            let (input_mode, output_mode) = (pin_type & 3, pin_type >> 2 & 15);
            if input_mode != 1 && pin("D_IN_0").is_some() {
                return Err(format!("`{name}`: registered SB_IO inputs are not simulated"));
            }
            if let Some(p) = pad {
                drive(m, pin("D_IN_0"), p);
                if kind == "SB_GB_IO" {
                    drive(m, pin("GLOBAL_BUFFER_OUTPUT"), p);
                }
            }
            if output_mode != 0 {
                if !matches!(output_mode, 6 | 10 | 14) {
                    return Err(format!("`{name}`: registered SB_IO outputs are not simulated"));
                }
                let d = input("D_OUT_0", SigBit::ZERO);
                let v = if output_mode == 6 {
                    d
                } else {
                    // Tristate: the pad floats when the output is disabled.
                    let oe = input("OUTPUT_ENABLE", SigBit::ZERO);
                    m.mux(oe, Sig::bit(SigBit::Const(Logic::Z)), Sig::bit(d)).0[0]
                };
                if let Some(p) = pad
                    && !matches!(p, SigBit::Const(_))
                {
                    m.connect(Sig::bit(p), Sig::bit(v));
                }
            }
        }
        "SB_GB" => {
            drive(m, pin("GLOBAL_BUFFER_OUTPUT"), input("USER_SIGNAL_TO_GLOBAL_BUFFER", SigBit::X))
        }
        k if k.starts_with("SB_PLL40") => {
            let refclk = pin("REFERENCECLK").or_else(|| pin("PACKAGEPIN")).unwrap_or(SigBit::ZERO);
            for out in [
                "PLLOUTCORE",
                "PLLOUTGLOBAL",
                "PLLOUTCOREA",
                "PLLOUTCOREB",
                "PLLOUTGLOBALA",
                "PLLOUTGLOBALB",
            ] {
                drive(m, pin(out), refclk);
            }
            drive(m, pin("LOCK"), SigBit::ONE);
        }
        "SB_WARMBOOT" => {}
        _ => return Ok(false),
    }
    Ok(true)
}

/// `SB_RAM40_4K`: a 256 × 16 memory with a write port and a registered read
/// port, and the data pin wiring of the mode.
fn ram(
    m: &mut Module,
    nl: &Netlist,
    c: CellId,
    pin: &dyn Fn(&str) -> Option<SigBit>,
    kind: &str,
    name: &str,
) -> Result<(), String> {
    let input = |p: &str, default: SigBit| pin(p).unwrap_or(default);
    let read_mode = param_u64(nl, c, "READ_MODE") as usize;
    let write_mode = param_u64(nl, c, "WRITE_MODE") as usize;
    if read_mode > 3 || write_mode > 3 {
        return Err(format!("`{name}`: bad READ_MODE or WRITE_MODE"));
    }
    // INIT_k holds words 16k..16k + 15, 16 bits each.
    let mut init = Vec::with_capacity(256);
    for k in 0..16 {
        let bits = param_bits(nl, c, &format!("INIT_{k:X}"), 256);
        for j in 0..16 {
            init.push(bits.slice(16 * j as isize, 16));
        }
    }
    let raddr = bus(pin, "RADDR", 11, SigBit::ZERO);
    let waddr = bus(pin, "WADDR", 11, SigBit::ZERO);
    let wdata = bus(pin, "WDATA", 16, SigBit::ZERO);
    let mask = bus(pin, "MASK", 16, SigBit::ZERO);
    let rclk = Clock { sig: input("RCLK", SigBit::ZERO), posedge: !kind.contains("NR") };
    let wclk = Clock { sig: input("WCLK", SigBit::ZERO), posedge: !kind.ends_with("NW") };
    let we = {
        let a = input("WE", SigBit::ZERO);
        let b = input("WCLKE", SigBit::ONE);
        m.and1(a, b)
    };
    let re = {
        let a = input("RE", SigBit::ZERO);
        let b = input("RCLKE", SigBit::ONE);
        m.and1(a, b)
    };
    // Write: in a narrow mode, group g of the word takes the data pin of the
    // group at the bit the high address bits select.
    let group = |mode: usize| 1usize << mode;
    let data_pin = |mode: usize, g: usize| match mode {
        0 => g,
        1 => 2 * g,
        2 => 4 * g + 1,
        _ => 8 * g + 3,
    };
    let (wd, wen): (Sig, Sig) = if write_mode == 0 {
        let en: Vec<SigBit> = mask
            .iter()
            .map(|&b| {
                let nb = m.not1(b);
                m.and1(nb, we)
            })
            .collect();
        (wdata.clone(), Sig(en))
    } else {
        let gsize = group(write_mode);
        let sel = Sig(waddr.0[8..8 + write_mode].to_vec());
        let mut d = Vec::with_capacity(16);
        let mut en = Vec::with_capacity(16);
        let hits: Vec<SigBit> = (0..gsize)
            .map(|k| {
                let hit = m.eq_const(sel.clone(), &LogicVec::from_u64(k as u64, write_mode));
                m.and1(hit, we)
            })
            .collect();
        for bit in 0..16 {
            let (g, k) = (bit / gsize, bit % gsize);
            d.push(wdata.0[data_pin(write_mode, g)]);
            en.push(hits[k]);
        }
        (Sig(d), Sig(en))
    };
    let word = m.new_sig(16);
    m.memories.push(Memory {
        name: name.to_string(),
        width: 16,
        depth: 256,
        offset: 0,
        init,
        reads: vec![MemRead {
            clk: Some(rclk),
            en: re,
            addr: Sig(raddr.0[..8].to_vec()),
            data: word.clone(),
            transparent: vec![false],
        }],
        writes: vec![MemWrite { clk: wclk, en: wen, addr: Sig(waddr.0[..8].to_vec()), data: wd }],
        alive: true,
    });
    // Read: the data pin of each group shows the bit the address selected
    // at the read; the other pins read 0.
    let rdata: Vec<Option<SigBit>> = (0..16).map(|i| pin(&format!("RDATA[{i}]"))).collect();
    if read_mode == 0 {
        for (i, o) in rdata.iter().enumerate() {
            drive(m, *o, word.0[i]);
        }
        return Ok(());
    }
    let gsize = group(read_mode);
    let sel = m.new_sig(read_mode);
    m.add_cell(
        format!("{name}$rsel"),
        CellKind::Dff(Box::new(Dff {
            clk: rclk,
            d: Sig(raddr.0[8..8 + read_mode].to_vec()),
            q: sel.clone(),
            en: Some(Control { sig: re, pol: true }),
            srst: None,
            en_over_srst: false,
            arst: None,
            init: LogicVec::zeros(read_mode),
        })),
    );
    for (i, o) in rdata.iter().enumerate() {
        let Some(o) = *o else { continue };
        let g = (0..16 / gsize).find(|&g| data_pin(read_mode, g) == i);
        let v = match g {
            Some(g) => {
                let bits = Sig(word.0[g * gsize..(g + 1) * gsize].to_vec());
                m.shiftx(bits, sel.clone(), false, 0, 1).0[0]
            }
            None => SigBit::ZERO,
        };
        m.connect(Sig::bit(o), Sig::bit(v));
    }
    Ok(())
}
