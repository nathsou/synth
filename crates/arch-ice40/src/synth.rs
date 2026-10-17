//! Synthesis for iCE40: maps word-level RTL to iCE40 primitives
//! (`SB_LUT4`, `SB_CARRY`, `SB_DFF*`, `SB_RAM40_4K`) for the packer.

use std::time::Instant;

use synth_core::{FxHashMap, Logic, LogicVec};
use synth_logic::aig::{Lit, Node};
use synth_logic::ff::FfRules;
use synth_logic::lower::LowerOptions;
use synth_logic::lutmap::{self, MapOptions};
use synth_logic::net::{Bit, LogicNet};
use synth_netlist::{Bit as PBit, CellId, Const, Dir, NetId, Netlist, Param};
use synth_rtl::{
    Attr, BinaryOp, CellKind, Clock, Control, Dff, Instance, MemId, Memory, Module, PortDir, Sig,
    SigBit, dff, mem, opt,
};

use crate::cells::port_dir;

#[derive(Clone, Debug)]
pub struct SynthOptions {
    pub lower: LowerOptions,
    pub map: MapOptions,
    /// Memories with fewer bits are built from flip-flops.
    pub bram_min_bits: u64,
}

impl Default for SynthOptions {
    fn default() -> Self {
        SynthOptions {
            lower: LowerOptions::default(),
            map: MapOptions::default(),
            bram_min_bits: 256,
        }
    }
}

/// Synthesises `m` into a netlist of iCE40 primitives.
pub fn synthesize(mut m: Module, opts: &SynthOptions) -> Result<(Netlist, Vec<String>), String> {
    let mut log = Vec::new();
    let t = Instant::now();
    opt::clean(&mut m)?;
    let n = dff::extract_controls(&mut m);
    log.push(format!("{n} flip-flop bits with enable/reset"));
    mem::merge_write_ports(&mut m);
    let n = mem::merge_read_ffs(&mut m);
    if n > 0 {
        log.push(format!("{n} memory read ports made synchronous"));
    }
    let mut brams = 0;
    for mi in (0..m.memories.len()).map(MemId::new) {
        if !m.memories[mi].alive {
            continue;
        }
        if bram_ok(&m.memories[mi], opts) {
            mem::emulate_transparency(&mut m, mi);
            brams += map_bram(&mut m, mi);
        } else {
            log.push(format!("memory {} built from flip-flops", m.memories[mi].name));
            mem::to_registers(&mut m, mi)?;
        }
    }
    if brams > 0 {
        log.push(format!("{brams} SB_RAM40_4K"));
    }
    opt::clean(&mut m)?;
    let n = dff::extract_controls(&mut m);
    let _ = n;
    opt::clean(&mut m)?;

    let mut net = synth_logic::lower::lower(&m, &opts.lower)?;
    synth_logic::ff::legalize(
        &mut net,
        &FfRules { init_one: false, srst_with_arst: false, srst_over_en: false },
    );
    net.cleanup();
    log.push(format!(
        "lowered in {:.2}s: {} AND nodes, {} flip-flops, {} carry chains",
        t.elapsed().as_secs_f64(),
        net.aig.num_ands(),
        net.ffs.len(),
        net.carries.len()
    ));
    let t = Instant::now();
    let mapping = lutmap::map(&net.aig, &opts.map);
    log.push(format!(
        "mapped in {:.2}s: {} LUTs, depth {}",
        t.elapsed().as_secs_f64(),
        mapping.luts.len(),
        mapping.depth
    ));
    let nl = emit(&net, &mapping)?;
    Ok((nl, log))
}

// ---------------------------------------------------------------- BRAM

fn bram_ok(mem: &Memory, opts: &SynthOptions) -> bool {
    mem.width as u64 * mem.depth as u64 >= opts.bram_min_bits
        && mem.writes.len() <= 1
        && !mem.reads.is_empty()
        && mem.reads.iter().all(|r| r.clk.is_some_and(|c| c.posedge))
        && mem.writes.iter().all(|w| w.clk.posedge)
}

/// Replaces memory `mi` (synchronous, non-transparent reads; at most one
/// write port) with `SB_RAM40_4K` blocks in 256x16 mode: one set per read
/// port, banked by the address bits above 8. Returns the number of blocks.
fn map_bram(m: &mut Module, mi: MemId) -> usize {
    let mem = m.memories[mi].clone();
    let width = mem.width as usize;
    let depth = mem.depth as usize;
    let cols = width.div_ceil(16);
    let banks = depth.div_ceil(256);
    let bank_bits = banks.next_power_of_two().trailing_zeros() as usize;
    let mut count = 0;
    // Word index (address minus offset), 8 + bank_bits wide.
    let index = |m: &mut Module, addr: &Sig| -> Sig {
        let w = 8 + bank_bits;
        let a = addr.extend(w.max(addr.width()), false);
        let a = if mem.offset == 0 {
            a
        } else {
            let off = Sig::from_const(&LogicVec::from_i64(mem.offset, a.width()));
            m.binary(BinaryOp::Sub, false, a, off)
        };
        Sig(a.0[..w].to_vec())
    };
    let wr = mem.writes.first().cloned();
    let windex = wr.as_ref().map(|w| index(m, &w.addr));
    for rd in &mem.reads {
        let clk = rd.clk.unwrap();
        let rindex = index(m, &rd.addr);
        // Data of each bank, then selected by the registered bank number.
        let mut bank_data: Vec<Sig> = Vec::new();
        for bank in 0..banks {
            let mut data = Sig::new();
            for col in 0..cols {
                let lo = col * 16;
                let n = (width - lo).min(16);
                let rdata = m.new_sig(16);
                data.0.extend_from_slice(&rdata.0[..n]);
                let mut conns: Vec<(String, PortDir, Sig)> = Vec::new();
                let raddr = Sig(rindex.0[..8].to_vec()).extend(11, false);
                conns.push(("RDATA".into(), PortDir::Output, rdata));
                conns.push(("RADDR".into(), PortDir::Input, raddr));
                conns.push(("RCLK".into(), PortDir::Input, Sig::bit(clk.sig)));
                conns.push(("RCLKE".into(), PortDir::Input, Sig::bit(SigBit::ONE)));
                conns.push(("RE".into(), PortDir::Input, Sig::bit(rd.en)));
                match (&wr, &windex) {
                    (Some(w), Some(wi)) => {
                        let in_bank = if banks == 1 {
                            SigBit::ONE
                        } else {
                            let hi = Sig(wi.0[8..].to_vec());
                            m.eq_const(hi, &LogicVec::from_u64(bank as u64, bank_bits))
                        };
                        let en: Vec<SigBit> = (0..16)
                            .map(|i| if i < n { w.en.0[lo + i] } else { SigBit::ZERO })
                            .collect();
                        let uniform = en[..n].iter().all(|&e| e == en[0]);
                        let any =
                            if uniform { en[0] } else { m.reduce_bool(Sig(en[..n].to_vec())) };
                        let we = m.and1(any, in_bank);
                        let mask: Sig = if uniform {
                            Sig::zeros(16)
                        } else {
                            en.iter().map(|&e| m.not1(e)).collect()
                        };
                        let wdata: Sig = (0..16)
                            .map(|i| if i < n { w.data.0[lo + i] } else { SigBit::ZERO })
                            .collect();
                        conns.push((
                            "WADDR".into(),
                            PortDir::Input,
                            Sig(wi.0[..8].to_vec()).extend(11, false),
                        ));
                        conns.push(("WDATA".into(), PortDir::Input, wdata));
                        conns.push(("MASK".into(), PortDir::Input, mask));
                        conns.push(("WCLK".into(), PortDir::Input, Sig::bit(w.clk.sig)));
                        conns.push(("WCLKE".into(), PortDir::Input, Sig::bit(SigBit::ONE)));
                        conns.push(("WE".into(), PortDir::Input, Sig::bit(we)));
                    }
                    _ => {
                        conns.push(("WE".into(), PortDir::Input, Sig::bit(SigBit::ZERO)));
                        conns.push(("WCLKE".into(), PortDir::Input, Sig::bit(SigBit::ZERO)));
                    }
                }
                let mut params = vec![
                    ("READ_MODE".to_string(), Attr::Const(LogicVec::from_u64(0, 2))),
                    ("WRITE_MODE".to_string(), Attr::Const(LogicVec::from_u64(0, 2))),
                ];
                for k in 0..16 {
                    let mut bits = vec![Logic::Zero; 256];
                    for wd in 0..16 {
                        let word = bank * 256 + k * 16 + wd;
                        if word >= depth {
                            continue;
                        }
                        for b in 0..n {
                            if mem.init[word].get(lo + b) == Logic::One {
                                bits[wd * 16 + b] = Logic::One;
                            }
                        }
                    }
                    params.push((format!("INIT_{k:X}"), Attr::Const(LogicVec::from_bits(&bits))));
                }
                m.add_cell(
                    format!("{}$bram{}_{}", mem.name, bank, col),
                    CellKind::Instance(Box::new(Instance {
                        module: "SB_RAM40_4K".into(),
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
            // The bank of the word being read, registered like the RAM output.
            let sel = Sig(rindex.0[8..].to_vec());
            let q = m.new_sig(bank_bits);
            m.add_cell(
                format!("{}$bank", mem.name),
                CellKind::Dff(Box::new(Dff {
                    clk: Clock { sig: clk.sig, posedge: true },
                    d: sel,
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

// ---------------------------------------------------------------- netlist

struct Emitter<'a> {
    net: &'a LogicNet,
    nl: Netlist,
    src: Vec<NetId>,
    lut_of: FxHashMap<u32, (NetId, Vec<u32>, u64)>,
    inverted: FxHashMap<u32, NetId>,
    pads: Vec<NetId>,
    zero: Option<NetId>,
    one: Option<NetId>,
}

fn lut_init(truth: u64, k: usize) -> Param {
    let bits = (0..16)
        .map(|i| if (truth >> (i & ((1 << k) - 1))) & 1 == 1 { PBit::One } else { PBit::Zero })
        .collect();
    Param::Bits(bits)
}

/// LUT pins for a LUT's inputs, in order. `I1`/`I3` come first: they are
/// fed from the other half of a tile's local tracks than `I0`/`I2`, which
/// control signals compete for.
const LUT_PINS: [usize; 4] = [1, 3, 0, 2];

/// `LUT_INIT` for a truth table over inputs placed on `LUT_PINS`.
fn lut_init_pinned(truth: u64, k: usize) -> Param {
    let bits = (0..16)
        .map(|idx| {
            let t: usize = (0..k).map(|j| ((idx >> LUT_PINS[j]) & 1) << j).sum();
            if (truth >> t) & 1 == 1 { PBit::One } else { PBit::Zero }
        })
        .collect();
    Param::Bits(bits)
}

fn emit(net: &LogicNet, mapping: &lutmap::Mapping) -> Result<Netlist, String> {
    let mut e = Emitter {
        net,
        nl: Netlist::new(),
        src: Vec::new(),
        lut_of: FxHashMap::default(),
        inverted: FxHashMap::default(),
        pads: Vec::new(),
        zero: None,
        one: None,
    };
    e.nl.top = Some(e.nl.sym(&net.name));
    for i in 0..net.aig.inputs.len() {
        let n = e.nl.add_net(&format!("$s{i}"));
        e.src.push(n);
    }
    for p in 0..net.pads {
        let n = e.nl.add_net(&format!("$pad{p}"));
        e.pads.push(n);
    }
    // LUTs.
    for l in &mapping.luts {
        let o = e.nl.add_net(&format!("$n{}", l.node));
        e.lut_of.insert(l.node, (o, l.leaves.clone(), l.truth));
    }
    for l in &mapping.luts {
        let out = e.lut_of[&l.node].0;
        let ins: Vec<NetId> = l.leaves.iter().map(|&x| e.node_net(x)).collect::<Result<_, _>>()?;
        e.lut(&format!("$lut{}", l.node), &ins, l.truth, out)?;
    }
    // Ports.
    for p in &net.ports {
        let multi = p.bits.len() > 1;
        for (i, b) in p.bits.iter().enumerate() {
            let name = if multi { format!("{}[{i}]", p.name) } else { p.name.clone() };
            match (p.dir, b) {
                (PortDir::Input, Bit::Src(s)) => {
                    let c = e.nl.add_cell(&name, synth_netlist::INPUT_PORT);
                    let n = e.src[*s as usize];
                    e.pin(c, "PAD", Dir::Output, Some(n))?;
                }
                (PortDir::Output, Bit::Snk(k)) => {
                    let n = e.sink(*k)?;
                    let c = e.nl.add_cell(&name, synth_netlist::OUTPUT_PORT);
                    e.pin(c, "PAD", Dir::Input, Some(n))?;
                }
                (PortDir::Inout, Bit::Pad(q)) => {
                    let c = e.nl.add_cell(&name, synth_netlist::INOUT_PORT);
                    let n = e.pads[*q as usize];
                    e.pin(c, "PAD", Dir::Inout, Some(n))?;
                }
                _ => unreachable!(),
            }
        }
    }
    // Flip-flops.
    for (i, ff) in net.ffs.iter().enumerate() {
        let mut kind = String::from("SB_DFF");
        if !ff.posedge {
            kind.push('N');
        }
        if ff.en.is_some() {
            kind.push('E');
        }
        let sr = match (ff.srst, ff.arst) {
            (Some((s, v)), None) => Some((s, if v { "SS" } else { "SR" })),
            (None, Some((s, v))) => Some((s, if v { "S" } else { "R" })),
            (None, None) => None,
            _ => return Err("flip-flop with both resets after legalisation".into()),
        };
        if let Some((_, suffix)) = sr {
            kind.push_str(suffix);
        }
        let c = e.nl.add_cell(&format!("{}[{i}]", ff.name), &kind);
        let clk = e.sink(ff.clk)?;
        e.pin(c, "C", Dir::Input, Some(clk))?;
        let d = e.sink(ff.d)?;
        e.pin(c, "D", Dir::Input, Some(d))?;
        if let Some(en) = ff.en {
            let n = e.sink(en)?;
            e.pin(c, "E", Dir::Input, Some(n))?;
        }
        if let Some((s, suffix)) = sr {
            let n = e.sink(s)?;
            let pin = if matches!(suffix, "S" | "SS") { "S" } else { "R" };
            e.pin(c, pin, Dir::Input, Some(n))?;
        }
        let q = e.src[ff.q as usize];
        e.pin(c, "Q", Dir::Output, Some(q))?;
    }
    // Carry chains.
    let xor3: u64 = (0..16u64).map(|i| (((i >> 1) ^ (i >> 2) ^ (i >> 3)) & 1) << i).sum();
    for (ci, ch) in net.carries.iter().enumerate() {
        let mut carry = e.sink(ch.cin)?;
        let w = ch.a.len();
        for i in 0..w {
            let a = e.sink(ch.a[i])?;
            let b = e.sink(ch.b[i])?;
            if let Some(&s) = ch.sum.get(i) {
                let c = e.nl.add_cell(&format!("$alu{ci}_lut{i}"), "SB_LUT4");
                e.pin(c, "I1", Dir::Input, Some(a))?;
                e.pin(c, "I2", Dir::Input, Some(b))?;
                e.pin(c, "I3", Dir::Input, Some(carry))?;
                let out = e.src[s as usize];
                e.pin(c, "O", Dir::Output, Some(out))?;
                e.nl.set_param(c, "LUT_INIT", lut_init(xor3, 4));
            }
            let co = if i + 1 == w {
                match ch.cout {
                    Some(s) => e.src[s as usize],
                    None => break,
                }
            } else {
                e.nl.add_net(&format!("$alu{ci}_c{}", i + 1))
            };
            let c = e.nl.add_cell(&format!("$alu{ci}_carry{i}"), "SB_CARRY");
            e.pin(c, "I0", Dir::Input, Some(a))?;
            e.pin(c, "I1", Dir::Input, Some(b))?;
            e.pin(c, "CI", Dir::Input, Some(carry))?;
            e.pin(c, "CO", Dir::Output, Some(co))?;
            carry = co;
        }
    }
    // Instances.
    for inst in &net.instances {
        let c = e.nl.add_cell(&inst.name, &inst.module);
        for (name, dir, bits) in &inst.conns {
            let multi = bits.len() > 1;
            for (i, b) in bits.iter().enumerate() {
                let pname = if multi { format!("{name}[{i}]") } else { name.clone() };
                let pdir = port_dir(&inst.module, &pname).unwrap_or(match dir {
                    PortDir::Input => Dir::Input,
                    PortDir::Output => Dir::Output,
                    PortDir::Inout => Dir::Inout,
                });
                let n = match b {
                    Bit::Src(s) => Some(e.src[*s as usize]),
                    Bit::Snk(k) => Some(e.sink(*k)?),
                    Bit::Pad(p) if *p == u32::MAX => None,
                    Bit::Pad(p) => Some(e.pads[*p as usize]),
                };
                e.pin(c, &pname, pdir, n)?;
            }
        }
        for (k, v) in &inst.params {
            let p = match v {
                Attr::Const(c) => Param::Bits(
                    c.bits()
                        .map(|b| match b {
                            Logic::Zero => PBit::Zero,
                            Logic::One => PBit::One,
                            Logic::X => PBit::X,
                            Logic::Z => PBit::Z,
                        })
                        .collect(),
                ),
                Attr::Str(s) => Param::Str(s.clone()),
            };
            e.nl.set_param(c, k, p);
        }
    }
    Ok(e.nl)
}

impl Emitter<'_> {
    fn pin(&mut self, c: CellId, name: &str, dir: Dir, net: Option<NetId>) -> Result<(), String> {
        self.nl.add_pin(c, name, dir, net).map(|_| ()).map_err(|e| e.0)
    }

    fn lut(&mut self, name: &str, ins: &[NetId], truth: u64, out: NetId) -> Result<(), String> {
        let c = self.nl.add_cell(name, "SB_LUT4");
        for (j, &n) in ins.iter().enumerate() {
            self.pin(c, &format!("I{}", LUT_PINS[j]), Dir::Input, Some(n))?;
        }
        self.pin(c, "O", Dir::Output, Some(out))?;
        self.nl.set_param(c, "LUT_INIT", lut_init_pinned(truth, ins.len()));
        Ok(())
    }

    fn konst(&mut self, one: bool) -> NetId {
        let slot = if one { &mut self.one } else { &mut self.zero };
        if let Some(n) = *slot {
            return n;
        }
        let n = self.nl.add_const_net(if one { Const::One } else { Const::Zero });
        if one {
            self.one = Some(n);
        } else {
            self.zero = Some(n);
        }
        n
    }

    /// The net carrying the (positive) value of an AIG node.
    fn node_net(&mut self, node: u32) -> Result<NetId, String> {
        match self.net.aig.nodes[node as usize] {
            Node::Const => Ok(self.konst(false)),
            Node::Input(k) => Ok(self.src[k as usize]),
            Node::And(..) => match self.lut_of.get(&node) {
                Some(l) => Ok(l.0),
                None => Err(format!("AIG node {node} is used but not mapped")),
            },
        }
    }

    fn sink(&mut self, k: usize) -> Result<NetId, String> {
        let l = self.net.aig.outputs[k];
        self.lit_net(l)
    }

    fn lit_net(&mut self, l: Lit) -> Result<NetId, String> {
        if l.is_const() {
            return Ok(self.konst(l == Lit::TRUE));
        }
        let n = self.node_net(l.node())?;
        if !l.is_compl() {
            return Ok(n);
        }
        if let Some(&x) = self.inverted.get(&l.node()) {
            return Ok(x);
        }
        let out = self.nl.add_net(&format!("$inv{}", l.node()));
        match self.lut_of.get(&l.node()).cloned() {
            // A second LUT with the complemented function.
            Some((_, leaves, truth)) => {
                let ins: Vec<NetId> =
                    leaves.iter().map(|&x| self.node_net(x)).collect::<Result<_, _>>()?;
                let mask =
                    if leaves.len() >= 6 { u64::MAX } else { (1u64 << (1 << leaves.len())) - 1 };
                self.lut(&format!("$lutn{}", l.node()), &ins, !truth & mask, out)?;
            }
            None => self.lut(&format!("$not{}", l.node()), &[n], 0b01, out)?,
        }
        self.inverted.insert(l.node(), out);
        Ok(out)
    }
}
