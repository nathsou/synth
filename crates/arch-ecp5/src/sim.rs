//! Simulation models of the ECP5 primitives, as RTL cells (ADR 0008).
//!
//! From the Lattice FPGA library and Yosys' models of it: a `LUT4` looks
//! `D C B A` up in `INIT`; a `CCU2C` half is a LUT4 `P` and the generate
//! signal `G` of the table's first four entries (inputs `A` and `B`), with
//! `S = P ^ CI` and `CO = P ? CI : G`, or with `INJECT1` `S = P` and
//! `CO = P & CI`; `PFUMX` selects `ALUT` when `C0` is high, `L6MUX21`
//! `D1` when `SD` is high. A `TRELLIS_FF` captures `DI` on the `CLKMUX`
//! edge when `CEMUX` enables it; its set/reset (`LSRMUX`) loads the
//! `REGSET` value, synchronously over the enable (`LSR_OVER_CE`) or at once
//! (`ASYNC`), and it powers up at that value (configuration initialises
//! every flip-flop, whether the global set/reset reaches it or not, as
//! Yosys' initial values rely on). A
//! `DP16KD` is 1024 words of 18 bits behind two ports of 18, 9, 4, 2 or 1
//! bits (see [`crate::bram`]), each reading on its clock edge when enabled
//! and selected, its reset clearing the output; in the pseudo dual port
//! mode (`PDPW16KD`, a 36-bit port A), port A writes pairs of words with
//! byte enables and port B reads. A `MULT18X18D` (without registers)
//! multiplies `A` by `B`, each signed where its `SIGNED` input is high.

use synth_core::LogicVec;
use synth_netlist::{CellId, Netlist, Param};
use synth_rtl::{
    BinaryOp, CellKind, Clock, Control, Dff, MemRead, MemWrite, Memory, Module, Sig, SigBit,
};

use crate::cells::{CCU2C, DP16KD, FF, L6MUX21, LUT4, MULT18X18D, PFUMX};
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
                    init: value,
                })),
            );
        }
        DP16KD => dp16kd(m, nl, c, pin, &name)?,
        MULT18X18D => {
            // P = A * B, each signed where its SIGNED input is high.
            let operand = |m: &mut Module, p: &str| -> Sig {
                let bits: Vec<SigBit> =
                    (0..18).map(|i| pin(&format!("{p}{i}")).unwrap_or(SigBit::ZERO)).collect();
                let signed = pin(&format!("SIGNED{p}")).unwrap_or(SigBit::ZERO);
                let sign = m.and1(bits[17], signed);
                Sig(bits.into_iter().chain(std::iter::repeat_n(sign, 18)).collect())
            };
            let (a, b) = (operand(m, "A"), operand(m, "B"));
            let product = m.binary(BinaryOp::Mul, false, a, b);
            for i in 0..36 {
                drive(m, pin(&format!("P{i}")), product.0[i]);
            }
        }
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

/// A block RAM port's inputs, and whether it is enabled (clock enable
/// and chip selects).
struct EbrPort {
    clk: Clock,
    ad: Vec<SigBit>,
    di: Vec<SigBit>,
    oce: SigBit,
    we: SigBit,
    rst: SigBit,
    enabled: SigBit,
}

fn ebr_port(
    m: &mut Module,
    nl: &Netlist,
    c: CellId,
    pin: &dyn Fn(&str) -> Option<SigBit>,
    p: &str,
    name: &str,
) -> Result<EbrPort, String> {
    let input = |m: &mut Module, n: &str, d: SigBit| ebr_input(m, nl, c, pin, n, d);
    let clk = Clock {
        sig: input(m, &format!("CLK{p}"), SigBit::ZERO),
        posedge: str_param(nl, c, &format!("CLK{p}MUX"), "") != "INV",
    };
    let ad = (0..14).map(|i| input(m, &format!("AD{p}{i}"), SigBit::ZERO)).collect();
    let di = (0..18).map(|i| input(m, &format!("DI{p}{i}"), SigBit::ZERO)).collect();
    let cs = Sig((0..3).map(|i| input(m, &format!("CS{p}{i}"), SigBit::ZERO)).collect());
    let ce = input(m, &format!("CE{p}"), SigBit::ONE);
    let oce = input(m, &format!("OCE{p}"), SigBit::ONE);
    let we = input(m, &format!("WE{p}"), SigBit::ZERO);
    let rst = input(m, &format!("RST{p}"), SigBit::ZERO);
    let decode = str_param(nl, c, &format!("CSDECODE_{p}"), "0b000");
    let decode = u64::from_str_radix(decode.trim_start_matches("0b"), 2)
        .map_err(|_| format!("`{name}`: bad CSDECODE_{p} {decode}"))?;
    let selected = m.eq_const(cs, &LogicVec::from_u64(decode, 3));
    let enabled = m.and1(ce, selected);
    Ok(EbrPort { clk, ad, di, oce, we, rst, enabled })
}

/// A flip-flop of a block RAM model.
fn ebr_reg(m: &mut Module, name: String, clk: Clock, d: Sig, en: Option<SigBit>) -> Sig {
    let q = m.new_sig(d.width());
    m.add_cell(
        name,
        CellKind::Dff(Box::new(Dff {
            clk,
            d,
            q: q.clone(),
            en: en.map(|sig| Control { sig, pol: true }),
            srst: None,
            en_over_srst: false,
            arst: None,
            init: LogicVec::zeros(q.width()),
        })),
    );
    q
}

/// A read port's output: the data read, cleared by the reset until the
/// next read (at the clock edge, whether enabled or not, or at once), and
/// registered again in `OUTREG` mode.
#[allow(clippy::too_many_arguments)]
fn ebr_output(
    m: &mut Module,
    nl: &Netlist,
    c: CellId,
    name: &str,
    p: &str,
    port: &EbrPort,
    read_en: SigBit,
    data: Vec<SigBit>,
) -> Vec<SigBit> {
    let cleared = m.new_sig(1);
    let reset = Some((Control { sig: port.rst, pol: true }, LogicVec::ones(1)));
    let async_reset = str_param(nl, c, "RESETMODE", "SYNC") == "ASYNC";
    m.add_cell(
        format!("{name}$cleared{p}"),
        CellKind::Dff(Box::new(Dff {
            clk: port.clk,
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
    let out: Vec<SigBit> = data.into_iter().map(|b| m.and1(b, keep)).collect();
    if str_param(nl, c, &format!("REGMODE_{p}"), "NOREG") == "OUTREG" {
        ebr_reg(m, format!("{name}$q{p}"), port.clk, Sig(out), Some(port.oce)).0
    } else {
        out
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
    let width = |p: &str| -> Result<usize, String> {
        match str_param(nl, c, &format!("DATA_WIDTH_{p}"), "18") {
            "36" => Ok(36),
            "18" => Ok(18),
            "9" => Ok(9),
            "4" => Ok(4),
            "2" => Ok(2),
            "1" => Ok(1),
            w => Err(format!("`{name}`: bad DATA_WIDTH_{p} {w}")),
        }
    };
    let ports = [ebr_port(m, nl, c, pin, "A", name)?, ebr_port(m, nl, c, pin, "B", name)?];
    let widths = [width("A")?, width("B")?];
    // In the pseudo dual port mode (36-bit port A), port A writes two
    // words at once, with the byte enables `ADA[3:0]`, and port B reads
    // (36 bits on `DOA` then `DOB`, as Yosys connects them).
    let pdp = widths[0] == 36;
    let mut reads = Vec::new();
    let mut writes = Vec::new();
    let mut outputs: Vec<(&str, Vec<SigBit>)> = Vec::new();
    // The word of a 36-bit port's word pair.
    let pair = |ad: &[SigBit], h: usize| -> Sig {
        Sig(std::iter::once(if h == 1 { SigBit::ONE } else { SigBit::ZERO })
            .chain(ad[5..14].iter().copied())
            .collect())
    };
    if pdp {
        let a = &ports[0];
        for h in 0..2 {
            let bytes: Vec<SigBit> = (0..2).map(|k| m.and1(a.enabled, a.ad[2 * h + k])).collect();
            let data = if h == 0 { a.di.clone() } else { ports[1].di.clone() };
            writes.push(MemWrite {
                clk: a.clk,
                en: Sig((0..18).map(|b| bytes[b / 9]).collect()),
                addr: pair(&a.ad, h),
                data: Sig(data),
            });
        }
    }
    for (k, p) in ["A", "B"].iter().enumerate() {
        let (port, width) = (&ports[k], widths[k]);
        if pdp && k == 0 {
            continue;
        }
        let write = if pdp { SigBit::ZERO } else { m.and1(port.enabled, port.we) };
        // The word, and the unit of it a narrower port addresses.
        let word = Sig(port.ad[4..14].to_vec());
        let low = match width {
            36 | 18 => 4,
            9 => 3,
            w => w.trailing_zeros() as usize,
        };
        let sub = Sig(port.ad[low..4].to_vec());
        let units = 1usize << (4 - low);
        if !pdp {
            let mut en = vec![SigBit::ZERO; 18];
            let mut data = vec![SigBit::ZERO; 18];
            for s in 0..units {
                let at = if units == 1 {
                    write
                } else {
                    let h = m.eq_const(sub.clone(), &LogicVec::from_u64(s as u64, 4 - low));
                    m.and1(write, h)
                };
                for (i, b) in unit_bits(width, s).into_iter().enumerate() {
                    data[b] = port.di[i];
                    // In the 18-bit mode, `AD[1:0]` enable the halves.
                    en[b] = if width == 18 { m.and1(at, port.ad[b / 9]) } else { at };
                }
            }
            writes.push(MemWrite {
                clk: port.clk,
                en: Sig(en),
                addr: word.clone(),
                data: Sig(data),
            });
        }
        // Reads: in NORMAL mode not while writing; WRITETHROUGH shows the
        // written data.
        let mode = str_param(nl, c, &format!("WRITEMODE_{p}"), "NORMAL");
        let read_en = if !pdp && mode == "NORMAL" {
            let nw = m.not1(port.we);
            m.and1(port.enabled, nw)
        } else {
            port.enabled
        };
        let mut transparent = vec![false, false];
        transparent[k] = !pdp && mode == "WRITETHROUGH";
        let mut read = |m: &mut Module, addr: Sig| {
            let q = m.new_sig(18);
            reads.push(MemRead {
                clk: Some(port.clk),
                en: read_en,
                addr,
                data: q.clone(),
                transparent: transparent.clone(),
            });
            q
        };
        if width == 36 {
            for (h, out) in [(0, "A"), (1, "B")] {
                let q = read(m, pair(&port.ad, h));
                let data = ebr_output(m, nl, c, name, &format!("{p}{h}"), port, read_en, q.0);
                outputs.push((out, data));
            }
            continue;
        }
        let q = read(m, word);
        let mut data: Vec<SigBit> = if units == 1 {
            unit_bits(width, 0).into_iter().map(|b| q.0[b]).collect()
        } else {
            let sel = ebr_reg(m, format!("{name}$unit{p}"), port.clk, sub.clone(), Some(read_en));
            let mut v = Sig(unit_bits(width, 0).into_iter().map(|b| q.0[b]).collect());
            for s in 1..units {
                let at = m.eq_const(sel.clone(), &LogicVec::from_u64(s as u64, 4 - low));
                let u = Sig(unit_bits(width, s).into_iter().map(|b| q.0[b]).collect());
                v = m.mux(at, v, u);
            }
            v.0
        };
        data.resize(18, SigBit::ZERO);
        outputs.push((p, ebr_output(m, nl, c, name, p, port, read_en, data)));
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
