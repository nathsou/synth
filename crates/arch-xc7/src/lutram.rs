//! Distributed RAM: RAM32M/RAM64M in SLICEM LUTs for shallow memories.
//!
//! A RAM64M is one SLICEM: four 64x1 LUT RAMs written together at port D's
//! address, each read at its own address. RAM32M fractures each LUT into two
//! 32x1 halves sharing an address (2 bits per port). Port D is read at the
//! write address, so it serves a read port only when the addresses agree.
use synth_core::{Logic, LogicVec};
use synth_rtl::{Attr, BinaryOp, CellKind, Instance, MemId, Memory, Module, PortDir, Sig, SigBit};

/// A memory of at most 64 words fits in one LUT RAM per bit and port, with
/// no read multiplexers. A RAMB18E1 occupies the area of roughly 200 LUTs
/// (100 blocks against 20,800 LUTs on the 35T), while a RAM64M stores three
/// read-port bits in four LUTs, so such memories never favour block RAM.
pub(crate) fn eligible(mem: &Memory) -> bool {
    mem.depth <= 64
        && mem.writes.len() == 1
        && !mem.reads.is_empty()
        && mem.reads.iter().all(|r| r.clk.is_none())
}

/// One storage slot of a primitive: a read port and the memory bits it serves
/// (one bit for RAM64M, up to two for RAM32M).
type Slot = Option<(usize, Vec<usize>)>;

/// Replaces memory `mi` by LUT RAM primitives. Returns how many were used.
pub(crate) fn map(m: &mut Module, mi: MemId) -> usize {
    let mem = m.memories[mi].clone();
    let wr = mem.writes[0].clone();
    let small = mem.depth <= 32;
    let abits = if small { 5 } else { 6 };
    let per_slot = if small { 2 } else { 1 };
    // Word index relative to the first word; high bits beyond the primitive's
    // address alias, as reads outside the array are undefined.
    let index = |m: &mut Module, addr: &Sig| {
        let a = addr.extend(abits.max(addr.width()), false);
        let a = if mem.offset == 0 {
            a
        } else {
            let off = Sig::from_const(&LogicVec::from_i64(mem.offset, a.width()));
            m.binary(BinaryOp::Sub, false, a, off)
        };
        Sig(a.0[..abits].to_vec())
    };
    let waddr = index(m, &wr.addr);
    // Writes outside the array must not alias into a valid word.
    let bits = wr.addr.width().max(abits);
    let relative = {
        let addr = wr.addr.extend(bits, false);
        let offset = Sig::from_const(&LogicVec::from_i64(mem.offset, bits));
        m.binary(BinaryOp::Sub, false, addr, offset)
    };
    let limit = Sig::from_const(&LogicVec::from_u64(mem.depth as u64, bits + 1));
    let valid = m.binary(BinaryOp::Lt, false, relative.extend(bits + 1, false), limit).0[0];
    let raddrs: Vec<Sig> = mem.reads.iter().map(|r| index(m, &r.addr)).collect();
    // The primitive has a single write enable: one group per enable signal.
    let mut groups: Vec<(SigBit, Vec<usize>)> = Vec::new();
    for b in 0..mem.width as usize {
        let en = wr.en.0[b];
        match groups.iter_mut().find(|(e, _)| *e == en) {
            Some((_, bits)) => bits.push(b),
            None => groups.push((en, vec![b])),
        }
    }
    let mut count = 0;
    for (en, bits) in groups {
        let we = m.and1(en, valid);
        // Read ports at the write address may also use port D.
        let (mut shared, mut other) = (Vec::new(), Vec::new());
        for (r, ra) in raddrs.iter().enumerate() {
            for chunk in bits.chunks(per_slot) {
                let demand = (r, chunk.to_vec());
                if *ra == waddr { shared.push(demand) } else { other.push(demand) }
            }
        }
        let n = other.len().div_ceil(3).max((shared.len() + other.len()).div_ceil(4));
        let mut prims: Vec<[Slot; 4]> = vec![Default::default(); n];
        let mut demands = other.into_iter().chain(shared.iter().cloned());
        for slot in 0..3 {
            for p in prims.iter_mut() {
                p[slot] = demands.next();
            }
        }
        // Whatever remains is at the write address; port D reads it.
        let rest: Vec<_> = demands.collect();
        for (p, d) in prims.iter_mut().filter(|p| p[3].is_none()).zip(rest) {
            p[3] = Some(d);
        }
        for (k, slots) in prims.iter().enumerate() {
            emit(m, &mem, count + k, slots, small, &raddrs, &waddr, we);
        }
        count += prims.len();
    }
    m.memories[mi].alive = false;
    count
}

#[allow(clippy::too_many_arguments)]
fn emit(
    m: &mut Module,
    mem: &Memory,
    k: usize,
    slots: &[Slot; 4],
    small: bool,
    raddrs: &[Sig],
    waddr: &Sig,
    we: SigBit,
) {
    let wr = &mem.writes[0];
    let (abits, per_slot) = if small { (5, 2) } else { (6, 1) };
    let mut conns = Vec::new();
    let mut params = Vec::new();
    for (i, slot) in slots.iter().enumerate() {
        let port = char::from(b'A' + i as u8);
        let addr = if i == 3 {
            waddr.clone()
        } else {
            slot.as_ref().map_or_else(|| Sig::zeros(abits), |(r, _)| raddrs[*r].clone())
        };
        let mut din = vec![SigBit::ZERO; per_slot];
        let dout = m.new_sig(per_slot);
        let mut init = vec![Logic::Zero; 64];
        if let Some((r, bits)) = slot {
            for (j, &b) in bits.iter().enumerate() {
                din[j] = wr.data.0[b];
                m.connect(Sig::bit(mem.reads[*r].data.0[b]), Sig::bit(dout.0[j]));
                for (word, value) in mem.init.iter().enumerate() {
                    // RAM32M interleaves its two bits per address.
                    init[word * per_slot + j] = match value.get(b) {
                        Logic::One => Logic::One,
                        _ => Logic::Zero,
                    };
                }
            }
        }
        conns.push((format!("ADDR{port}"), PortDir::Input, addr));
        conns.push((format!("DI{port}"), PortDir::Input, Sig(din)));
        conns.push((format!("DO{port}"), PortDir::Output, dout));
        params.push((format!("INIT_{port}"), Attr::Const(LogicVec::from_bits(&init))));
    }
    conns.push(("WE".into(), PortDir::Input, Sig::bit(we)));
    conns.push(("WCLK".into(), PortDir::Input, Sig::bit(wr.clk.sig)));
    params.push((
        "IS_WCLK_INVERTED".into(),
        Attr::Const(LogicVec::from_u64(u64::from(!wr.clk.posedge), 1)),
    ));
    let module = if small { "RAM32M" } else { "RAM64M" };
    m.add_cell(
        format!("{}$lutram{k}", mem.name),
        CellKind::Instance(Box::new(Instance { module: module.into(), params, conns })),
    );
}
