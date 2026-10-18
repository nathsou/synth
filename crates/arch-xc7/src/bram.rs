//! True dual-port RAMB18E1: port A reads, port B writes. Replicate for
//! multiple read ports and split width/depth to preserve write enables.
use synth_core::{FxHashMap, Logic, LogicVec};
use synth_rtl::{
    Attr, BinaryOp, CellKind, Control, Dff, Instance, MemId, Memory, Module, PortDir, Sig, SigBit,
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
    let (mode, wdepth) = [18usize, 9, 4, 2, 1]
        .into_iter()
        .filter(|&w| {
            wr.as_ref().is_none_or(|wr| (0..width).all(|b| wr.en.0[b] == wr.en.0[b - b % w]))
        })
        .map(|w| {
            (
                w,
                if w == 18 {
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
    let data_bits = if mode == 18 {
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
                let dout = m.new_sig(16);
                let pout = m.new_sig(2);
                for b in 0..n {
                    data.0.push(if b < data_bits { dout.0[b] } else { pout.0[b - data_bits] });
                }
                let mut conns = vec![
                    ("DOADO".into(), PortDir::Output, dout),
                    ("DOPADOP".into(), PortDir::Output, pout),
                    ("ADDRARDADDR".into(), PortDir::Input, address(&rindex)),
                    ("CLKARDCLK".into(), PortDir::Input, Sig::bit(clk.sig)),
                    ("ENARDEN".into(), PortDir::Input, Sig::bit(rd.en)),
                ];
                let zero = |n| Sig(vec![SigBit::ZERO; n]);
                for (pin, size) in [
                    ("WEA", 2),
                    ("DIADI", 16),
                    ("DIPADIP", 2),
                    ("REGCEAREGCE", 1),
                    ("REGCEB", 1),
                    ("RSTRAMARSTRAM", 1),
                    ("RSTRAMB", 1),
                    ("RSTREGARSTREG", 1),
                    ("RSTREGB", 1),
                ] {
                    conns.push((pin.into(), PortDir::Input, zero(size)));
                }
                let mut din = vec![SigBit::ZERO; 16];
                let mut pin = vec![SigBit::ZERO; 2];
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
                conns.extend([
                    ("DIBDI".into(), PortDir::Input, Sig(din)),
                    ("DIPBDIP".into(), PortDir::Input, Sig(pin)),
                    ("ADDRBWRADDR".into(), PortDir::Input, waddr),
                    ("CLKBWRCLK".into(), PortDir::Input, Sig::bit(wclk)),
                    ("ENBWREN".into(), PortDir::Input, Sig::bit(SigBit::ONE)),
                    ("WEBWE".into(), PortDir::Input, Sig(vec![we, we, SigBit::ZERO, SigBit::ZERO])),
                ]);
                let val = |n| Attr::Const(LogicVec::from_u64(n, 32));
                let mut output_init = vec![Logic::Zero; 18];
                for (b, init) in output_init.iter_mut().enumerate().take(n) {
                    // INIT_A packs data followed by parity for the selected mode.
                    *init = read_init.get(&rd.data.0[lo + b]).copied().unwrap_or(Logic::Zero);
                }
                let mut params = vec![
                    ("INIT_A".into(), Attr::Const(LogicVec::from_bits(&output_init))),
                    ("RAM_MODE".into(), Attr::Str("TDP".into())),
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
