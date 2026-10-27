//! Block RAM (`DP16KD`) mapping.
//!
//! A `DP16KD` holds 1024 words of 18 bits, as 2048 units of 9 bits (the
//! layout of its initial contents, `INITVAL_00` to `3F`: sixteen 18-bit
//! words in 20 bits each). Memories map to blocks in the 18-bit mode
//! (1024 words, addressed by `AD[13:4]`, with `AD[1:0]` enabling the
//! writes of the two 9-bit halves) or the 9-bit mode (2048 words,
//! `AD[13:3]`), whichever takes fewer blocks: the data splits into lanes of
//! at most 9 bits that share a write enable, a block holding two lanes or
//! one. Port A writes, port B reads; each read port gets blocks of its
//! own, banked by the address bits above a block's depth like
//! `SB_RAM40_4K`s on iCE40.

use synth_core::{Logic, LogicVec};
use synth_rtl::{
    Attr, BinaryOp, CellKind, Clock, Control, Dff, Instance, MemId, Memory, Module, PortDir, Sig,
    SigBit,
};

use crate::cells::DP16KD;

/// Words of a block in the 18-bit mode.
const WORDS_18: usize = 1024;

/// Whether memory `mem` goes to block RAM: at least `min_bits`, one write
/// port at most, and synchronous reads.
pub fn fits(mem: &Memory, min_bits: u64) -> bool {
    mem.width as u64 * mem.depth as u64 >= min_bits
        && mem.writes.len() <= 1
        && !mem.reads.is_empty()
        && mem.reads.iter().all(|r| r.clk.is_some())
}

/// The data bits split into lanes `(first bit, bits)` of at most 9 bits
/// sharing a write enable.
fn lanes(mem: &Memory) -> Vec<(usize, usize)> {
    let width = mem.width as usize;
    let en = mem.writes.first().map(|w| &w.en);
    let mut lanes = Vec::new();
    let mut b = 0;
    while b < width {
        let mut n = 1;
        while b + n < width && n < 9 && en.is_none_or(|e| e.0[b + n] == e.0[b]) {
            n += 1;
        }
        lanes.push((b, n));
        b += n;
    }
    lanes
}

/// The INITVAL parameters of a block's 2048 9-bit units.
fn initvals(units: &[u16]) -> Vec<(String, Attr)> {
    (0..64)
        .map(|k| {
            let mut bits = vec![Logic::Zero; 320];
            for i in 0..16 {
                let word = 16 * k + i;
                let v = units[2 * word] as u32 | (units[2 * word + 1] as u32) << 9;
                for t in 0..18 {
                    if v >> t & 1 == 1 {
                        bits[20 * i + t] = Logic::One;
                    }
                }
            }
            (format!("INITVAL_{k:02X}"), Attr::Const(LogicVec::from_bits(&bits)))
        })
        .collect()
}

/// Replaces memory `mi` with `DP16KD`s; returns their number.
pub fn map(m: &mut Module, mi: MemId) -> Result<usize, String> {
    let mem = m.memories[mi].clone();
    let depth = mem.depth as usize;
    let lanes = lanes(&mem);
    // Lanes per block and words per block of each mode, the fewer blocks
    // (then banks) the better.
    let cost = |per: usize, words: usize| {
        let banks = depth.div_ceil(words);
        (lanes.len().div_ceil(per) * banks, banks)
    };
    let (per, words) =
        if cost(2, WORDS_18) <= cost(1, 2 * WORDS_18) { (2, WORDS_18) } else { (1, 2 * WORDS_18) };
    let width = if per == 2 { 18 } else { 9 };
    let abits = words.trailing_zeros() as usize;
    let banks = depth.div_ceil(words);
    let bank_bits = banks.next_power_of_two().trailing_zeros() as usize;
    let wr = mem.writes.first().cloned();
    // Word index (address minus offset), `abits + bank_bits` wide.
    let index = |m: &mut Module, addr: &Sig| -> Sig {
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
    // The 14 address pins of a word index (and the write enables of the
    // halves in the 18-bit mode).
    let address = |index: &Sig, be: [SigBit; 2]| -> Vec<SigBit> {
        let mut pins = vec![SigBit::ZERO; 14];
        for i in 0..abits {
            pins[14 - abits + i] = index.0[i];
        }
        if per == 2 {
            pins[0] = be[0];
            pins[1] = be[1];
        }
        pins
    };
    let windex = wr.as_ref().map(|w| index(m, &w.addr));
    let mut count = 0;
    for (r, rd) in mem.reads.iter().enumerate() {
        let clk = rd.clk.expect("synchronous read");
        let rindex = index(m, &rd.addr);
        let raddr = address(&Sig(rindex.0[..abits].to_vec()), [SigBit::ZERO; 2]);
        let mut bank_data: Vec<Vec<(usize, SigBit)>> = Vec::new();
        for bank in 0..banks {
            let mut data = Vec::new();
            for (col, group) in lanes.chunks(per).enumerate() {
                let dob = m.new_sig(18);
                let mut conns: Vec<(String, PortDir, Sig)> = Vec::new();
                let mut pin = |name: String, dir: PortDir, b: SigBit| {
                    conns.push((name, dir, Sig::bit(b)));
                };
                for (h, &(lo, n)) in group.iter().enumerate() {
                    for b in 0..n {
                        data.push((lo + b, dob.0[9 * h + b]));
                    }
                }
                for (i, &b) in dob.0.iter().enumerate() {
                    pin(format!("DOB{i}"), PortDir::Output, b);
                }
                for (i, &b) in raddr.iter().enumerate() {
                    pin(format!("ADB{i}"), PortDir::Input, b);
                }
                pin("CLKB".into(), PortDir::Input, clk.sig);
                pin("CEB".into(), PortDir::Input, rd.en);
                pin("OCEB".into(), PortDir::Input, SigBit::ONE);
                pin("WEB".into(), PortDir::Input, SigBit::ZERO);
                let mut params: Vec<(String, Attr)> = vec![
                    ("DATA_WIDTH_A".into(), Attr::Str(width.to_string())),
                    ("DATA_WIDTH_B".into(), Attr::Str(width.to_string())),
                    ("CLKBMUX".into(), Attr::Str(if clk.posedge { "CLKB" } else { "INV" }.into())),
                ];
                if let (Some(w), Some(wi)) = (&wr, &windex) {
                    let in_bank = if banks == 1 {
                        SigBit::ONE
                    } else {
                        let hi = Sig(wi.0[abits..].to_vec());
                        m.eq_const(hi, &LogicVec::from_u64(bank as u64, bank_bits))
                    };
                    let mut be = [SigBit::ZERO; 2];
                    for (h, &(lo, n)) in group.iter().enumerate() {
                        be[h] = w.en.0[lo];
                        for b in 0..n {
                            pin(format!("DIA{}", 9 * h + b), PortDir::Input, w.data.0[lo + b]);
                        }
                    }
                    let any = if group.len() == 2 && be[0] != be[1] {
                        m.or1(be[0], be[1])
                    } else {
                        be[0]
                    };
                    let we = m.and1(any, in_bank);
                    for (i, &b) in address(&Sig(wi.0[..abits].to_vec()), be).iter().enumerate() {
                        pin(format!("ADA{i}"), PortDir::Input, b);
                    }
                    pin("CLKA".into(), PortDir::Input, w.clk.sig);
                    pin("CEA".into(), PortDir::Input, SigBit::ONE);
                    pin("WEA".into(), PortDir::Input, we);
                    params.push((
                        "CLKAMUX".into(),
                        Attr::Str(if w.clk.posedge { "CLKA" } else { "INV" }.into()),
                    ));
                }
                // Initial contents of the block's units.
                let mut units = vec![0u16; 2 * WORDS_18];
                for wd in 0..words {
                    let word = bank * words + wd;
                    if word >= depth {
                        break;
                    }
                    for (h, &(lo, n)) in group.iter().enumerate() {
                        let unit = &mut units[per * wd + h];
                        for b in 0..n {
                            if mem.init[word].get(lo + b) == Logic::One {
                                *unit |= 1 << b;
                            }
                        }
                    }
                }
                params.extend(initvals(&units));
                m.add_cell(
                    format!("{}$dp16kd{r}_{bank}_{col}", mem.name),
                    CellKind::Instance(Box::new(Instance { module: DP16KD.into(), params, conns })),
                );
                count += 1;
            }
            data.sort_by_key(|d| d.0);
            bank_data.push(data);
        }
        let bits = |d: &[(usize, SigBit)]| Sig(d.iter().map(|b| b.1).collect());
        let value = if banks == 1 {
            bits(&bank_data[0])
        } else {
            // The bank of the word being read, registered like the data.
            let sel = Sig(rindex.0[abits..].to_vec());
            let q = m.new_sig(bank_bits);
            m.add_cell(
                format!("{}$bank{r}", mem.name),
                CellKind::Dff(Box::new(Dff {
                    clk: Clock { sig: clk.sig, posedge: clk.posedge },
                    d: sel,
                    q: q.clone(),
                    en: (rd.en != SigBit::ONE).then_some(Control { sig: rd.en, pol: true }),
                    srst: None,
                    en_over_srst: false,
                    arst: None,
                    init: LogicVec::xs(bank_bits),
                })),
            );
            let mut v = bits(&bank_data[0]);
            for (b, d) in bank_data.iter().enumerate().skip(1) {
                let hit = m.eq_const(q.clone(), &LogicVec::from_u64(b as u64, bank_bits));
                v = m.mux(hit, v, bits(d));
            }
            v
        };
        m.connect(rd.data.clone(), value);
    }
    m.memories[mi].alive = false;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_contents_pack_two_units_per_word() {
        let mut units = vec![0u16; 2048];
        units[0] = 0x1ff;
        units[3] = 0x001;
        let p = initvals(&units);
        let Attr::Const(v) = &p[0].1 else { panic!() };
        // Word 0: low unit all ones; word 1: bit 9 (its high unit's bit 0).
        assert!((0..9).all(|i| v.get(i) == Logic::One));
        assert_eq!(v.get(9), Logic::Zero);
        assert_eq!(v.get(20 + 9), Logic::One);
        assert_eq!(p[63].0, "INITVAL_3F");
    }
}
