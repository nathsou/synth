//! RAMB18E1: port A reads, port B writes, using SDP for 36-bit words. Replicate for
//! multiple read ports and split width/depth to preserve write enables.
use synth_core::{FxHashMap, Logic, LogicVec};
use synth_rtl::{
    Attr, BinaryOp, CellId, CellKind, Clock, Control, Dff, Instance, MemId, Memory, Module,
    PortDir, Sig, SigBit,
};

pub(crate) fn eligible(mem: &Memory) -> bool {
    mem.width as u64 * mem.depth as u64 >= 512
        && mem.writes.len() <= 1
        && !mem.reads.is_empty()
        && mem.reads.iter().all(|r| r.clk.is_some())
}

pub(crate) fn map(m: &mut Module, mi: MemId, read_init: &FxHashMap<SigBit, Logic>) -> usize {
    let mem = m.memories[mi].clone();
    let width = mem.width as usize;
    let depth = mem.depth as usize;
    let wr = mem.writes.first().cloned();
    let (mode, wdepth) = [36usize, 18, 9, 4, 2, 1]
        .into_iter()
        .filter(|&w| {
            wr.as_ref().is_none_or(|wr| (0..width).all(|b| wr.en.0[b] == wr.en.0[b - b % w]))
        })
        .map(|w| {
            (
                w,
                if w == 36 {
                    512
                } else if w == 18 {
                    1024
                } else if w == 9 {
                    2048
                } else {
                    16384 / w
                },
            )
        })
        .min_by_key(|&(w, d)| (width.div_ceil(w) * depth.div_ceil(d), w))
        .unwrap();
    let sdp = mode == 36;
    let data_bits = if sdp {
        32
    } else if mode == 18 {
        16
    } else if mode == 9 {
        8
    } else {
        mode
    };
    let abits = wdepth.trailing_zeros() as usize;
    let banks = depth.div_ceil(wdepth);
    let bank_bits = banks.next_power_of_two().trailing_zeros() as usize;
    let index = |m: &mut Module, addr: &Sig| {
        let w = abits + bank_bits;
        let a = addr.extend(w.max(addr.width()), false);
        let a = if mem.offset == 0 {
            a
        } else {
            let off = Sig::from_const(&LogicVec::from_i64(mem.offset, a.width()));
            m.binary(BinaryOp::Sub, false, a, off)
        };
        Sig(a.0[..w].to_vec())
    };
    let address = |idx: &Sig| {
        // Low address bits are ignored according to the configured width.
        let mut a = vec![SigBit::ZERO; 14 - abits];
        a.extend_from_slice(&idx.0[..abits]);
        Sig(a)
    };
    let windex = wr.as_ref().map(|w| index(m, &w.addr));
    // Preserve writes outside the declared array: they must not wrap into
    // a valid location when high address bits are removed for physical pins.
    let write_valid = wr.as_ref().map(|w| {
        let bits = w.addr.width().max(abits + bank_bits);
        let addr = w.addr.extend(bits, false);
        let offset = Sig::from_const(&LogicVec::from_i64(mem.offset, bits));
        let relative = m.binary(BinaryOp::Sub, false, addr, offset);
        let limit = Sig::from_const(&LogicVec::from_u64(mem.depth as u64, bits + 1));
        m.binary(BinaryOp::Lt, false, relative.extend(bits + 1, false), limit).0[0]
    });
    let mut count = 0;
    for (ri, rd) in mem.reads.iter().enumerate() {
        let clk = rd.clk.unwrap();
        let rindex = index(m, &rd.addr);
        let mut bank_data = Vec::new();
        for bank in 0..banks {
            let mut data = Sig::new();
            for col in 0..width.div_ceil(mode) {
                let lo = col * mode;
                let n = (width - lo).min(mode);
                let dout = m.new_sig(if sdp { 32 } else { 16 });
                let pout = m.new_sig(if sdp { 4 } else { 2 });
                for b in 0..n {
                    data.0.push(if b < data_bits { dout.0[b] } else { pout.0[b - data_bits] });
                }
                let mut conns = vec![
                    ("DOADO".into(), PortDir::Output, Sig(dout.0[..16].to_vec())),
                    ("DOPADOP".into(), PortDir::Output, Sig(pout.0[..2].to_vec())),
                    ("ADDRARDADDR".into(), PortDir::Input, address(&rindex)),
                    ("CLKARDCLK".into(), PortDir::Input, Sig::bit(clk.sig)),
                    ("ENARDEN".into(), PortDir::Input, Sig::bit(rd.en)),
                ];
                if sdp {
                    conns.extend([
                        ("DOBDO".into(), PortDir::Output, Sig(dout.0[16..].to_vec())),
                        ("DOPBDOP".into(), PortDir::Output, Sig(pout.0[2..].to_vec())),
                    ]);
                }
                let zero = |n| Sig(vec![SigBit::ZERO; n]);
                for (pin, size) in [
                    ("WEA", 2),
                    ("REGCEAREGCE", 1),
                    ("REGCEB", 1),
                    ("RSTRAMARSTRAM", 1),
                    ("RSTRAMB", 1),
                    ("RSTREGARSTREG", 1),
                    ("RSTREGB", 1),
                ] {
                    conns.push((pin.into(), PortDir::Input, zero(size)));
                }
                let mut din = vec![SigBit::ZERO; if sdp { 32 } else { 16 }];
                let mut pin = vec![SigBit::ZERO; if sdp { 4 } else { 2 }];
                let (we, wclk, waddr) = if let (Some(w), Some(wi)) = (&wr, &windex) {
                    for b in 0..n {
                        if b < data_bits {
                            din[b] = w.data.0[lo + b];
                        } else {
                            pin[b - data_bits] = w.data.0[lo + b];
                        }
                    }
                    let in_bank = if banks == 1 {
                        SigBit::ONE
                    } else {
                        m.eq_const(
                            Sig(wi.0[abits..].to_vec()),
                            &LogicVec::from_u64(bank as u64, bank_bits),
                        )
                    };
                    {
                        let enabled = m.and1(w.en.0[lo], in_bank);
                        (m.and1(enabled, write_valid.unwrap()), w.clk.sig, address(wi))
                    }
                } else {
                    (SigBit::ZERO, clk.sig, zero(14))
                };
                // SDP consumes both data buses: low data/parity on A, high on B.
                let (da, pa, db, pb) = if sdp {
                    (
                        Sig(din[..16].to_vec()),
                        Sig(pin[..2].to_vec()),
                        Sig(din[16..].to_vec()),
                        Sig(pin[2..].to_vec()),
                    )
                } else {
                    (zero(16), zero(2), Sig(din), Sig(pin))
                };
                conns.extend([
                    ("DIADI".into(), PortDir::Input, da),
                    ("DIPADIP".into(), PortDir::Input, pa),
                    ("DIBDI".into(), PortDir::Input, db),
                    ("DIPBDIP".into(), PortDir::Input, pb),
                    ("ADDRBWRADDR".into(), PortDir::Input, waddr),
                    ("CLKBWRCLK".into(), PortDir::Input, Sig::bit(wclk)),
                    ("ENBWREN".into(), PortDir::Input, Sig::bit(SigBit::ONE)),
                    (
                        "WEBWE".into(),
                        PortDir::Input,
                        if sdp {
                            Sig(vec![we; 4])
                        } else {
                            Sig(vec![we, we, SigBit::ZERO, SigBit::ZERO])
                        },
                    ),
                ]);
                let val = |n| Attr::Const(LogicVec::from_u64(n, 32));
                let mut output_init = [vec![Logic::Zero; 18], vec![Logic::Zero; 18]];
                for b in 0..n {
                    // Each INIT port packs 16 data bits then two parity bits;
                    // SDP interleaves the two ports' data and parity separately.
                    let (port, bit) = if sdp && b >= 32 {
                        ((b - 32) / 2, 16 + (b - 32) % 2)
                    } else if sdp {
                        (b / 16, b % 16)
                    } else {
                        (0, b)
                    };
                    output_init[port][bit] =
                        read_init.get(&rd.data.0[lo + b]).copied().unwrap_or(Logic::Zero);
                }
                let mut params = vec![
                    ("INIT_A".into(), Attr::Const(LogicVec::from_bits(&output_init[0]))),
                    ("INIT_B".into(), Attr::Const(LogicVec::from_bits(&output_init[1]))),
                    ("RAM_MODE".into(), Attr::Str(if sdp { "SDP" } else { "TDP" }.into())),
                    ("SIM_DEVICE".into(), Attr::Str("7SERIES".into())),
                    ("READ_WIDTH_A".into(), val(mode as u64)),
                    ("READ_WIDTH_B".into(), val(0)),
                    ("WRITE_WIDTH_A".into(), val(0)),
                    ("WRITE_WIDTH_B".into(), val(mode as u64)),
                    ("DOA_REG".into(), val(0)),
                    ("DOB_REG".into(), val(0)),
                    ("WRITE_MODE_A".into(), Attr::Str("READ_FIRST".into())),
                    ("WRITE_MODE_B".into(), Attr::Str("READ_FIRST".into())),
                    ("IS_CLKARDCLK_INVERTED".into(), val((!clk.posedge) as u64)),
                    (
                        "IS_CLKBWRCLK_INVERTED".into(),
                        val(wr.as_ref().is_some_and(|w| !w.clk.posedge) as u64),
                    ),
                ];
                let mut init = vec![Logic::Zero; 16384];
                let mut parity = vec![Logic::Zero; 2048];
                for word in 0..wdepth {
                    let wd = bank * wdepth + word;
                    if wd >= depth {
                        break;
                    }
                    for b in 0..n {
                        let v = mem.init[wd].get(lo + b);
                        if b < data_bits {
                            init[word * data_bits + b] = v;
                        } else {
                            parity[word * (mode - data_bits) + b - data_bits] = v;
                        }
                    }
                }
                for (prefix, bits) in [("INIT", init), ("INITP", parity)] {
                    for (k, bits) in bits.chunks(256).enumerate() {
                        params.push((
                            format!("{prefix}_{k:02X}"),
                            Attr::Const(LogicVec::from_bits(bits)),
                        ));
                    }
                }
                m.add_cell(
                    format!("{}$bram{ri}_{bank}_{col}", mem.name),
                    CellKind::Instance(Box::new(Instance {
                        module: "RAMB18E1".into(),
                        params,
                        conns,
                    })),
                );
                count += 1;
            }
            bank_data.push(data);
        }
        let value = if banks == 1 {
            bank_data.pop().unwrap()
        } else {
            let q = m.new_sig(bank_bits);
            m.add_cell(
                format!("{}$bank{ri}", mem.name),
                CellKind::Dff(Box::new(Dff {
                    clk,
                    d: Sig(rindex.0[abits..].to_vec()),
                    q: q.clone(),
                    en: if rd.en == SigBit::ONE {
                        None
                    } else {
                        Some(Control { sig: rd.en, pol: true })
                    },
                    srst: None,
                    en_over_srst: false,
                    arst: None,
                    init: LogicVec::xs(bank_bits),
                })),
            );
            let mut v = bank_data[0].clone();
            for (b, d) in bank_data.iter().enumerate().skip(1) {
                let hit = m.eq_const(q.clone(), &LogicVec::from_u64(b as u64, bank_bits));
                v = m.mux(hit, v, d.clone());
            }
            v
        };
        m.connect(rd.data.clone(), value);
    }
    m.memories[mi].alive = false;
    count
}

/// Moves flip-flops that only register a block RAM's read data into the
/// RAM's output register (DOA_REG, and DOB_REG for the upper half of a
/// 36-bit SDP word), with their clock enable and synchronous reset. Every used
/// output bit must feed exactly one such flip-flop, with one shared clock (the
/// read clock) and set of controls, and no asynchronous reset. The RAM output
/// latch and the register share INIT, so their power-on values must agree.
/// Returns the number of block RAMs changed.
pub(crate) fn absorb_output_registers(m: &mut Module) -> usize {
    let uses = synth_rtl::mem::use_counts(m);
    let by_d = synth_rtl::mem::ff_bits(m, true);
    let mut drops: FxHashMap<CellId, Vec<usize>> = FxHashMap::default();
    let mut n = 0;
    for ci in m.live_cells().collect::<Vec<_>>() {
        let CellKind::Instance(inst) = &m.cells[ci].kind else { continue };
        let param = |name: &str| inst.params.iter().find(|(p, _)| p == name).map(|(_, v)| v);
        let int = |name: &str| match param(name) {
            Some(Attr::Const(v)) => v.to_u64(),
            _ => None,
        };
        let sdp = matches!(param("RAM_MODE"), Some(Attr::Str(s)) if s == "SDP");
        if inst.module != "RAMB18E1" || int("DOA_REG") != Some(0) || int("DOB_REG") != Some(0) {
            continue;
        }
        let conn = |pin: &str| inst.conns.iter().find(|(p, _, _)| p == pin).map(|(_, _, s)| s);
        let Some(clk) = conn("CLKARDCLK") else { continue };
        // Each read output: its pin and bit, and its INIT/SRVAL port and bit
        // (SDP packs both ports' data, then both ports' parity).
        let pins: &[(&str, usize, usize)] = if sdp {
            &[("DOADO", 0, 0), ("DOBDO", 1, 0), ("DOPADOP", 0, 16), ("DOPBDOP", 1, 16)]
        } else {
            &[("DOADO", 0, 0), ("DOPADOP", 0, 16)]
        };
        let mut outputs = Vec::new();
        for &(pin, port, base) in pins {
            let Some(sig) = conn(pin) else { continue };
            for (i, &b) in sig.iter().enumerate() {
                outputs.push((pin, i, port, base + i, b));
            }
        }
        let clock = Clock { sig: clk.0[0], posedge: int("IS_CLKARDCLK_INVERTED") != Some(1) };
        let latch_init = |name: &str| match param(name) {
            Some(Attr::Const(v)) => v.bits().collect::<Vec<_>>(),
            _ => vec![Logic::Zero; 18],
        };
        let mut init = [latch_init("INIT_A"), latch_init("INIT_B")];
        // (output index, flip-flop cell, bit) for each used output.
        let mut found = Vec::new();
        let mut only_ffs = true;
        for (k, &(_, _, _, _, b)) in outputs.iter().enumerate() {
            match (uses.get(&b).copied().unwrap_or(0), by_d.get(&b)) {
                (0, _) => {}
                (1, Some(&(f, j))) => found.push((k, f, j)),
                _ => only_ffs = false,
            }
        }
        if !only_ffs || found.is_empty() {
            continue;
        }
        let dff = |f: CellId| match &m.cells[f].kind {
            CellKind::Dff(d) => d.as_ref(),
            _ => unreachable!(),
        };
        let first = dff(found[0].1);
        let same = |d: &Dff| {
            d.clk == clock
                && d.arst.is_none()
                && d.en == first.en
                && d.srst.as_ref().map(|s| s.0) == first.srst.as_ref().map(|s| s.0)
                && (d.en_over_srst == first.en_over_srst || d.en.is_none() || d.srst.is_none())
        };
        if !found.iter().all(|&(_, f, _)| same(dff(f))) {
            continue;
        }
        let mut srval = [vec![Logic::Zero; 18], vec![Logic::Zero; 18]];
        let mut q: Vec<SigBit> = outputs.iter().map(|o| o.4).collect();
        let mut agree = true;
        for &(k, f, j) in &found {
            let (_, _, port, bit, _) = outputs[k];
            let d = dff(f);
            match (d.init.get(j), init[port][bit]) {
                (r, _) if !r.is_known() => {}
                (r, l) if !l.is_known() => init[port][bit] = r,
                (r, l) => agree &= r == l,
            }
            if let Some((_, v)) = &d.srst {
                srval[port][bit] = if v.get(j).is_known() { v.get(j) } else { Logic::Zero };
            }
            q[k] = d.q.0[j];
        }
        if !agree {
            continue;
        }
        let (en, srst, en_over_srst) =
            (first.en, first.srst.as_ref().map(|s| s.0), first.en_over_srst);
        let regce = match en {
            None => SigBit::ONE,
            Some(c) if c.pol => c.sig,
            Some(c) => m.not1(c.sig),
        };
        // A register may span several block RAMs: drop its bits at the end,
        // keeping the indices found here valid.
        for &(_, f, j) in &found {
            drops.entry(f).or_default().push(j);
        }
        let CellKind::Instance(inst) = &mut m.cells[ci].kind else { unreachable!() };
        for (pin, _, s) in inst.conns.iter_mut() {
            for (k, o) in outputs.iter().enumerate() {
                if o.0 == pin.as_str() {
                    s.0[o.1] = q[k];
                }
            }
            match pin.as_str() {
                "REGCEAREGCE" => *s = Sig::bit(regce),
                "RSTREGARSTREG" => *s = Sig::bit(srst.map_or(SigBit::ZERO, |c| c.sig)),
                _ => {}
            }
        }
        let priority =
            if en_over_srst && en.is_some() && srst.is_some() { "REGCE" } else { "RSTREG" };
        let set = |inst: &mut Instance, name: &str, v: Attr| {
            inst.params.retain(|(p, _)| p != name);
            inst.params.push((name.into(), v));
        };
        let one = Attr::Const(LogicVec::from_u64(1, 32));
        set(inst, "DOA_REG", one.clone());
        if sdp {
            set(inst, "DOB_REG", one);
        }
        for (port, ab) in ["A", "B"].iter().enumerate() {
            set(inst, &format!("INIT_{ab}"), Attr::Const(LogicVec::from_bits(&init[port])));
            set(inst, &format!("SRVAL_{ab}"), Attr::Const(LogicVec::from_bits(&srval[port])));
        }
        set(inst, "RSTREG_PRIORITY_A", Attr::Str(priority.into()));
        let inverted = srst.is_some_and(|c| !c.pol) as u64;
        set(inst, "IS_RSTREGARSTREG_INVERTED", Attr::Const(LogicVec::from_u64(inverted, 32)));
        n += 1;
    }
    for (f, js) in drops {
        synth_rtl::mem::drop_ff_bits(m, f, &js);
    }
    n
}

/// Whether a shallow memory with asynchronous reads should go to block RAM
/// rather than LUT RAM: each read's data passes through two register stages
/// (each bit read only by a flip-flop, whose output is again read only by a
/// flip-flop, on one clock, with no asynchronous reset). The block RAM then
/// absorbs both, its output latch and its output register, which LUT RAM
/// leaves as flip-flops, and its registered output is fast.
pub(crate) fn absorbs_read_pipeline(m: &Module, mi: MemId) -> bool {
    let mem = &m.memories[mi];
    if (mem.width as u64 * mem.depth as u64) < 512 || mem.writes.len() > 1 || mem.reads.is_empty() {
        return false;
    }
    let uses = synth_rtl::mem::use_counts(m);
    let by_d = synth_rtl::mem::ff_bits(m, true);
    let stage = |b: &SigBit| -> Option<(SigBit, &Dff)> {
        if uses.get(b).copied() != Some(1) {
            return None;
        }
        let &(f, j) = by_d.get(b)?;
        let CellKind::Dff(d) = &m.cells[f].kind else { return None };
        d.arst.is_none().then_some((d.q.0[j], d.as_ref()))
    };
    // The first stage merges into the read port as merge_read_ffs does: one
    // clock and enable, no reset.
    mem.reads.iter().all(|rd| {
        let mut first: Option<(Clock, Option<Control>)> = None;
        rd.data.iter().all(|b| {
            let Some((q, d1)) = stage(b) else { return false };
            let Some((_, d2)) = stage(&q) else { return false };
            d1.srst.is_none()
                && d1.clk == d2.clk
                && *first.get_or_insert((d1.clk, d1.en)) == (d1.clk, d1.en)
        })
    })
}
