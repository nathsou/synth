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
//! with the global set/reset enabled it powers up at that value too.

use synth_core::LogicVec;
use synth_netlist::{CellId, Netlist, Param};
use synth_rtl::{BinaryOp, CellKind, Clock, Control, Dff, Module, Sig, SigBit};

use crate::cells::{CCU2C, FF, L6MUX21, LUT4, PFUMX};
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
        _ => return Ok(false),
    }
    Ok(true)
}
