//! Simulation models of the ECP5 primitives, as RTL cells (ADR 0008).
//!
//! From the Lattice FPGA library and Yosys' models of it: a `LUT4` looks
//! `D C B A` up in `INIT`; a `CCU2C` half is a LUT4 `P` and the generate
//! signal `G` of the table's first four entries (inputs `A` and `B`), with
//! `S = P ^ CI` and `CO = P ? CI : G`, or with `INJECT1` `S = P` and
//! `CO = P & CI`; `PFUMX` selects `ALUT` when `C0` is high, `L6MUX21`
//! `D1` when `SD` is high. A `TRELLIS_FF` captures `DI` on the `CLKMUX`
//! edge when `CEMUX` enables it; its set/reset (`LSRMUX`) loads the
//! `REGSET` value,
//! synchronously over the enable (`LSR_OVER_CE`) or at once (`ASYNC`), and
//! with the global set/reset enabled it powers up at that value too. A
//! `DP16KD` is 1024 words of 18 bits behind two ports of 18, 9, 4, 2 or 1
//! bits (see [`crate::bram`]), each reading on its clock edge when enabled
//! and selected, its reset clearing the output.

use synth_core::LogicVec;
use synth_netlist::{CellId, Netlist, Param};
use synth_rtl::{
    BinaryOp, CellKind, Clock, Control, Dff, MemRead, MemWrite, Memory, Module, Sig, SigBit,
};

use crate::cells::{CCU2C, DP16KD, FF, L6MUX21, LUT4, PFUMX};
use crate::pack::lut_init;

fn str_param<'a>(nl: &'a Netlist, c: CellId, name: &str, default: &'a str) -> &'a str {
    match nl.param(c, name) {
        Some(Param::Str(s)) => s.as_str(),
        _ => default,
    }
}

/// `s ? b : a`.
fn mux1(m: &mut Module, s: SigBit, a: SigBit, b: SigBit) -> SigBit {
    m.mux(s, Sig::bit(a), Sig::bit(b)).0[0]
}

fn xor1(m: &mut Module, a: SigBit, b: SigBit) -> SigBit {
    m.binary(BinaryOp::Xor, false, Sig::bit(a), Sig::bit(b)).0[0]
}

/// Drives `out` (if connected) from `v`.
fn drive(m: &mut Module, out: Option<SigBit>, v: SigBit) {
    if let Some(o) = out {
        m.connect(Sig::bit(o), Sig::bit(v));
    }
}

/// `table[idx]` for a 16-entry table.
fn lookup(m: &mut Module, table: u16, idx: Sig, y: SigBit, name: String) {
    let init = LogicVec::from_u64(table as u64, 16);
    m.add_cell(
        name,
        CellKind::Shiftx {
            a: Sig::from_const(&init),
            b: idx,
            b_signed: false,
            offset: 0,
            y: Sig::bit(y),
        },
    );
}

/// Adds the behaviour of ECP5 primitive `c` to `m`. Returns `Ok(false)`
/// for a cell kind that is not one.
pub fn add_cell(
    m: &mut Module,
    nl: &Netlist,
    c: CellId,
    pin: &dyn Fn(&str) -> Option<SigBit>,
) -> Result<bool, String> {
    let name = nl.str(nl.cells[c].name).to_string();
    // Unconnected logic inputs read high, as the fabric ties them.
    let input = |p: &str| pin(p).unwrap_or(SigBit::ONE);
    match nl.cell_kind(c) {
        LUT4 => {
            if let Some(z) = pin("Z") {
                let idx = Sig(["A", "B", "C", "D"].iter().map(|p| input(p)).collect());
                lookup(m, lut_init(nl, c, "INIT"), idx, z, name);
            }
        }
        CCU2C => {
            let mut carry = pin("CIN").unwrap_or(SigBit::ZERO);
            for h in 0..2 {
                let init = lut_init(nl, c, &format!("INIT{h}"));
                let ins = |p: &str| input(&format!("{p}{h}"));
                let p = m.new_sig(1).0[0];
                lookup(
                    m,
                    init,
                    Sig(vec![ins("A"), ins("B"), ins("C"), ins("D")]),
                    p,
                    format!("{name}$p{h}"),
                );
                let g = m.new_sig(1).0[0];
                lookup(m, init & 0xF, Sig(vec![ins("A"), ins("B")]), g, format!("{name}$g{h}"));
                let inject = str_param(nl, c, &format!("INJECT1_{h}"), "YES") == "YES";
                let (s, co) = if inject {
                    (p, m.and1(p, carry))
                } else {
                    (xor1(m, p, carry), mux1(m, p, g, carry))
                };
                drive(m, pin(&format!("S{h}")), s);
                carry = co;
            }
            drive(m, pin("COUT"), carry);
        }
        PFUMX => {
            let v = mux1(m, input("C0"), input("BLUT"), input("ALUT"));
            drive(m, pin("Z"), v);
        }
        L6MUX21 => {
            let v = mux1(m, input("SD"), input("D0"), input("D1"));
            drive(m, pin("Z"), v);
        }
        FF => {
            let Some(q) = pin("Q") else { return Ok(true) };
            let set = str_param(nl, c, "REGSET", "RESET") == "SET";
            let value = LogicVec::from_bool(set);
            let lsr_pol = str_param(nl, c, "LSRMUX", "LSR") != "INV";
            let lsr = pin("LSR").map(|s| Control { sig: s, pol: lsr_pol });
            let async_reset = str_param(nl, c, "SRMODE", "LSR_OVER_CE") == "ASYNC";
            let (srst, arst) = match lsr {
                Some(l) if async_reset => (None, Some((l, value.clone()))),
                Some(l) => (Some((l, value.clone())), None),
                None => (None, None),
            };
            let en = match str_param(nl, c, "CEMUX", "CE") {
                "1" => None,
                "0" => Some(Control { sig: SigBit::ZERO, pol: true }),
                "INV" => pin("CE").map(|s| Control { sig: s, pol: false }),
                _ => pin("CE").map(|s| Control { sig: s, pol: true }),
            };
            let gsr = str_param(nl, c, "GSR", "ENABLED") == "ENABLED";
            m.add_cell(
                name,
                CellKind::Dff(Box::new(Dff {
                    clk: Clock {
                        sig: pin("CLK").unwrap_or(SigBit::ZERO),
                        posedge: str_param(nl, c, "CLKMUX", "CLK") != "INV",
                    },
                    d: Sig::bit(pin("DI").unwrap_or(SigBit::ZERO)),
                    q: Sig::bit(q),
                    en,
                    srst,
                    en_over_srst: false,
                    arst,
                    init: if gsr { value } else { LogicVec::zeros(1) },
                })),
            );
        }
        DP16KD => dp16kd(m, nl, c, pin, &name)?,
        _ => return Ok(false),
    }
    Ok(true)
}

/// A `DP16KD` input: its pin (else `default`, or high where its `<pin>MUX`
/// parameter ties it so), inverted where that parameter is `INV`.
fn ebr_input(
    m: &mut Module,
    nl: &Netlist,
    c: CellId,
    pin: &dyn Fn(&str) -> Option<SigBit>,
    name: &str,
    default: SigBit,
) -> SigBit {
    let mux = str_param(nl, c, &format!("{name}MUX"), "");
    let v = pin(name).unwrap_or(if mux == "1" { SigBit::ONE } else { default });
    if mux == "INV" { m.not1(v) } else { v }
}

/// The bits of an 18-bit word unit `s` of a `width`-bit port takes: the
/// halves of the 9-bit mode; in the narrower modes, units of the 16 data
/// bits (skipping each half's ninth bit).
fn unit_bits(width: usize, s: usize) -> Vec<usize> {
    match width {
        18 => (0..18).collect(),
        9 => (9 * s..9 * s + 9).collect(),
        _ => (s * width..(s + 1) * width).map(|d| d + usize::from(d >= 8)).collect(),
    }
}

fn dp16kd(
    m: &mut Module,
    nl: &Netlist,
    c: CellId,
    pin: &dyn Fn(&str) -> Option<SigBit>,
    name: &str,
) -> Result<(), String> {
    let init: Vec<LogicVec> = (0..1024)
        .map(|w| {
            let bits = nl
                .param(c, &format!("INITVAL_{:02X}", w / 16))
                .map(|p| p.to_bits(320))
                .unwrap_or_default();
            let v: u64 = (0..18)
                .filter(|t| bits.get(20 * (w % 16) + t).is_some_and(|b| b.is_one()))
                .map(|t| 1 << t)
                .sum();
            LogicVec::from_u64(v, 18)
        })
        .collect();
    let async_reset = str_param(nl, c, "RESETMODE", "SYNC") == "ASYNC";
    let mut reads = Vec::new();
    let mut writes = Vec::new();
    let mut outputs = Vec::new();
    for (k, p) in ["A", "B"].iter().enumerate() {
        let width: usize = match str_param(nl, c, &format!("DATA_WIDTH_{p}"), "18") {
            "18" => 18,
            "9" => 9,
            "4" => 4,
            "2" => 2,
            "1" => 1,
            w => return Err(format!("`{name}`: bad DATA_WIDTH_{p} {w}")),
        };
        let input = |m: &mut Module, n: &str, d: SigBit| ebr_input(m, nl, c, pin, n, d);
        let clk = Clock {
            sig: input(m, &format!("CLK{p}"), SigBit::ZERO),
            posedge: str_param(nl, c, &format!("CLK{p}MUX"), "") != "INV",
        };
        let ad: Vec<SigBit> =
            (0..14).map(|i| input(m, &format!("AD{p}{i}"), SigBit::ZERO)).collect();
        let di: Vec<SigBit> =
            (0..18).map(|i| input(m, &format!("DI{p}{i}"), SigBit::ZERO)).collect();
        let cs = Sig((0..3).map(|i| input(m, &format!("CS{p}{i}"), SigBit::ZERO)).collect());
        let ce = input(m, &format!("CE{p}"), SigBit::ONE);
        let oce = input(m, &format!("OCE{p}"), SigBit::ONE);
        let we = input(m, &format!("WE{p}"), SigBit::ZERO);
        let rst = input(m, &format!("RST{p}"), SigBit::ZERO);
        // Selected when the chip selects match the decode.
        let decode = str_param(nl, c, &format!("CSDECODE_{p}"), "0b000");
        let decode = u64::from_str_radix(decode.trim_start_matches("0b"), 2)
            .map_err(|_| format!("`{name}`: bad CSDECODE_{p} {decode}"))?;
        let selected = m.eq_const(cs, &LogicVec::from_u64(decode, 3));
        let enabled = m.and1(ce, selected);
        let write = m.and1(enabled, we);
        // The word, and the unit of it a narrower port addresses.
        let word = Sig(ad[4..14].to_vec());
        let low = match width {
            18 => 4,
            9 => 3,
            w => w.trailing_zeros() as usize,
        };
        let sub = Sig(ad[low..4].to_vec());
        let units = 1usize << (4 - low);
        let hit = |m: &mut Module, s: usize| -> SigBit {
            if units == 1 {
                SigBit::ONE
            } else {
                m.eq_const(sub.clone(), &LogicVec::from_u64(s as u64, 4 - low))
            }
        };
        let mut en = vec![SigBit::ZERO; 18];
        let mut data = vec![SigBit::ZERO; 18];
        for s in 0..units {
            let h = hit(m, s);
            let at = m.and1(write, h);
            for (i, b) in unit_bits(width, s).into_iter().enumerate() {
                data[b] = di[i];
                // In the 18-bit mode, `AD[1:0]` enable the halves.
                en[b] = if width == 18 { m.and1(at, ad[b / 9]) } else { at };
            }
        }
        writes.push(MemWrite { clk, en: Sig(en), addr: word.clone(), data: Sig(data) });
        // Reads: in NORMAL mode not while writing; WRITETHROUGH shows the
        // written data.
        let mode = str_param(nl, c, &format!("WRITEMODE_{p}"), "NORMAL");
        let read_en = if mode == "NORMAL" {
            let nw = m.not1(we);
            m.and1(enabled, nw)
        } else {
            enabled
        };
        let q = m.new_sig(18);
        let mut transparent = vec![false, false];
        transparent[k] = mode == "WRITETHROUGH";
        reads.push(MemRead {
            clk: Some(clk),
            en: read_en,
            addr: word,
            data: q.clone(),
            transparent,
        });
        let reg = |m: &mut Module, d: Sig, en: Option<SigBit>, tag: &str| {
            let out = m.new_sig(d.width());
            m.add_cell(
                format!("{name}${tag}{p}"),
                CellKind::Dff(Box::new(Dff {
                    clk,
                    d,
                    q: out.clone(),
                    en: en.map(|sig| Control { sig, pol: true }),
                    srst: None,
                    en_over_srst: false,
                    arst: None,
                    init: LogicVec::zeros(out.width()),
                })),
            );
            out
        };
        // The unit a read took.
        let mut out: Vec<SigBit> = if units == 1 {
            unit_bits(width, 0).into_iter().map(|b| q.0[b]).collect()
        } else {
            let sel = reg(m, sub.clone(), Some(read_en), "unit");
            let mut v = Sig(unit_bits(width, 0).into_iter().map(|b| q.0[b]).collect());
            for s in 1..units {
                let at = m.eq_const(sel.clone(), &LogicVec::from_u64(s as u64, 4 - low));
                let u = Sig(unit_bits(width, s).into_iter().map(|b| q.0[b]).collect());
                v = m.mux(at, v, u);
            }
            v.0
        };
        out.resize(18, SigBit::ZERO);
        // The reset clears the output until the next read: at the clock
        // edge (whether enabled or not), or at once.
        let cleared = m.new_sig(1);
        let reset = Some((Control { sig: rst, pol: true }, LogicVec::ones(1)));
        m.add_cell(
            format!("{name}$cleared{p}"),
            CellKind::Dff(Box::new(Dff {
                clk,
                d: Sig::bit(SigBit::ZERO),
                q: cleared.clone(),
                en: Some(Control { sig: read_en, pol: true }),
                srst: if async_reset { None } else { reset.clone() },
                en_over_srst: false,
                arst: if async_reset { reset } else { None },
                init: LogicVec::zeros(1),
            })),
        );
        let keep = m.not1(cleared.0[0]);
        out = out.into_iter().map(|b| m.and1(b, keep)).collect();
        if str_param(nl, c, &format!("REGMODE_{p}"), "NOREG") == "OUTREG" {
            out = reg(m, Sig(out), Some(oce), "q").0;
        }
        outputs.push((p, out));
    }
    m.memories.push(Memory {
        name: name.to_string(),
        width: 18,
        depth: 1024,
        offset: 0,
        init,
        reads,
        writes,
        alive: true,
    });
    for (p, out) in outputs {
        for (i, b) in out.into_iter().enumerate() {
            drive(m, pin(&format!("DO{p}{i}")), b);
        }
    }
    Ok(())
}
