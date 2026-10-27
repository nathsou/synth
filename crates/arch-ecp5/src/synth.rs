//! Synthesis for ECP5: maps word-level RTL to ECP5 primitives (`LUT4`,
//! `CCU2C`, `TRELLIS_FF`, `DP16KD`) for the packer.
//!
//! The pipeline is iCE40's (a LUT4 architecture with a carry chain): RTL
//! optimisation, memories, lowering to an AIG with flip-flops and carry
//! chains, LUT mapping with restructuring rounds. What differs is the
//! primitives:
//!
//! - an ECP5 flip-flop's set/reset (`LSR`) overrides its enable and sets
//!   both its reset value and its power-up value (`REGSET`, applied by the
//!   global set/reset at the end of configuration): a reset that should act
//!   only when enabled is gated with the enable, and a synchronous reset
//!   to a value other than the power-up value goes into the data path;
//! - a `CCU2C` is two bits of carry chain, each half a LUT4 computing the
//!   propagate signal `P` (its `C` and `D` inputs tied high) whose first
//!   four entries also give the generate signal `G` used where `P` is low:
//!   `S = P ^ CI`, `CO = P ? CI : G`. Operand inversions and constants
//!   fold into the tables. A chain starts with a half generating its carry
//!   in (`P = 0`, `G` the carry in), and a carry out read by logic leaves
//!   through a half passing it to its sum (`P = 0`), as nextpnr builds them.

use synth_core::time::Instant;

use synth_core::{FxHashMap, Logic};
use synth_logic::aig::{Lit, Node};
use synth_logic::ff::FfRules;
use synth_logic::lower::LowerOptions;
use synth_logic::lutmap::{self, MapOptions};
use synth_logic::net::{Bit, LogicNet};
use synth_netlist::{Bit as PBit, CellId, Const, Dir, NetId, Netlist, Param};
use synth_rtl::{Attr, MemId, Module, PortDir, dff, mem, opt};

use crate::cells::{CCU2C, FF, LUT4, port_dir};

#[derive(Clone, Debug)]
pub struct SynthOptions {
    pub lower: LowerOptions,
    pub map: MapOptions,
    /// Rounds of restructuring and remapping the mapped LUTs.
    pub remap_rounds: usize,
    /// Memories with fewer bits are built from flip-flops.
    pub bram_min_bits: u64,
    /// The most multipliers (`MULT18X18D`) to use (`None`: no limit).
    pub multipliers: Option<usize>,
}

impl Default for SynthOptions {
    fn default() -> Self {
        SynthOptions {
            lower: LowerOptions::default(),
            map: MapOptions { lut_delay: LEVEL, ..MapOptions::default() },
            remap_rounds: 6,
            bram_min_bits: 256,
            multipliers: None,
        }
    }
}

/// Synthesises `m` into a netlist of ECP5 primitives.
pub fn synthesize(mut m: Module, opts: &SynthOptions) -> Result<(Netlist, Vec<String>), String> {
    let mut log = Vec::new();
    let t = Instant::now();
    opt::clean(&mut m)?;
    if mem::constant_index_to_registers(&mut m)? > 0 {
        opt::clean(&mut m)?;
    }
    for _ in 0..8 {
        if opt::reduce_widths(&mut m) == 0 {
            break;
        }
        opt::clean(&mut m)?;
    }
    if opt::shift_by_multiple(&mut m) > 0 {
        opt::clean(&mut m)?;
    }
    let merged = opt::merge_identical(&mut m);
    let shared = opt::share_arith(&mut m);
    let trees = opt::chains_to_trees(&mut m);
    let fsm = synth_rtl::fsm::simplify_state_compares(&mut m);
    opt::clean(&mut m)?;
    opt::merge_identical(&mut m);
    opt::clean(&mut m)?;
    log.push(format!(
        "{merged} duplicate cells merged, {shared} arithmetic operations shared, {trees} mux chains as trees, {fsm} state comparisons simplified"
    ));
    let n = dff::extract_controls(&mut m);
    log.push(format!("{n} flip-flop bits with enable/reset"));
    if mem::remove_feedback_writes(&mut m) > 0 {
        opt::clean(&mut m)?;
    }
    mem::merge_write_ports(&mut m);
    opt::clean(&mut m)?;
    let n = mem::merge_read_ffs(&mut m);
    if n > 0 {
        log.push(format!("{n} memory read ports made synchronous"));
    }
    let mut brams = 0;
    for mi in (0..m.memories.len()).map(MemId::new) {
        if !m.memories[mi].alive {
            continue;
        }
        if crate::bram::fits(&m.memories[mi], opts.bram_min_bits) {
            mem::emulate_transparency(&mut m, mi);
            brams += crate::bram::map(&mut m, mi)?;
        } else {
            log.push(format!("memory {} built from flip-flops", m.memories[mi].name));
            mem::to_registers(&mut m, mi)?;
        }
    }
    if brams > 0 {
        log.push(format!("{brams} DP16KD"));
    }
    opt::clean(&mut m)?;
    dff::extract_controls(&mut m);
    opt::clean(&mut m)?;
    let mults = crate::dsp::map(&mut m, opts.multipliers);
    if mults > 0 {
        log.push(format!("{mults} MULT18X18D"));
        opt::clean(&mut m)?;
    }

    let mut net = synth_logic::lower::lower(&m, &opts.lower)?;
    synth_logic::ff::legalize(
        &mut net,
        &FfRules { init_one: true, srst_with_arst: false, srst_over_en: true },
    );
    let resets = synth_logic::seq::extract_sync_resets(&mut net);
    let folded = legalize_resets(&mut net);
    net.cleanup();
    log.push(format!(
        "lowered in {:.2}s: {} AND nodes, {} flip-flops ({} synchronous resets extracted, {} into logic for their power-up value), {} carry chains",
        t.elapsed().as_secs_f64(),
        net.aig.num_ands(),
        net.ffs.len(),
        resets,
        folded,
        net.carries.len()
    ));
    let mapping = synth_logic::mapping::optimize_observed(
        &mut net,
        &opts.map,
        opts.remap_rounds,
        chain_timing,
        netlist_luts,
        &mut log,
        |_| {},
    )?;
    let nl = emit(&net, &mapping)?;
    Ok((nl, log))
}

/// Makes the flip-flops' resets what an ECP5 flip-flop does: the set/reset
/// overrides the enable, and loads the power-up value. Returns the number
/// of synchronous resets moved into the data path.
fn legalize_resets(net: &mut LogicNet) -> usize {
    let mut folded = 0;
    for i in 0..net.ffs.len() {
        let g = &mut net.aig;
        let ff = &mut net.ffs[i];
        // A reset acting only when enabled: reset when both.
        if ff.en_over_srst
            && let (Some(en), Some((r, v))) = (ff.en, ff.srst)
        {
            let gated = g.and(g.outputs[en], g.outputs[r]);
            ff.srst = Some((g.output(gated), v));
            ff.en_over_srst = false;
        }
        let init = match ff.init {
            Logic::Zero => Some(false),
            Logic::One => Some(true),
            _ => None,
        };
        if let (Some((r, v)), Some(init)) = (ff.srst, init)
            && v != init
        {
            // D = reset ? value : D, and enabled whenever resetting (the
            // reset overrode the enable).
            let (rl, dl) = (g.outputs[r], g.outputs[ff.d]);
            let val = if v { Lit::TRUE } else { Lit::FALSE };
            let nd = g.mux(rl, val, dl);
            ff.d = g.output(nd);
            if let Some(e) = ff.en {
                let ne = g.or(g.outputs[e], rl);
                ff.en = Some(g.output(ne));
            }
            ff.srst = None;
            folded += 1;
        }
    }
    folded
}

/// Delays of the paths through carry chains, in hundredths of a LUT level
/// (a LUT and a typical net, about 0.65 ns on speed grade 6): a net into
/// the chain, the operand to the carry out of its half, 6 per bit of carry
/// propagation (71 ps a `CCU2C`), and the carry into a sum (Trellis
/// timing of `SCCU2C`).
const CHAIN_NET: u32 = 60;
const CHAIN_ENTRY: u32 = 70;
const CHAIN_BIT: u32 = 6;
const CHAIN_SUM: u32 = 70;
const LEVEL: u32 = 100;

/// Mapping constraints for the carry chains of `net`: every operand bit
/// reaches the sums at and above it, and the carry out.
fn chain_timing(net: &LogicNet) -> lutmap::Timing {
    let mut t = lutmap::Timing::default();
    for c in &net.carries {
        let w = c.a.len();
        let mut arc = |from: usize, to: u32, delay: u32| {
            t.arcs.push(lutmap::BoxArc { from, to: to as usize, delay })
        };
        for j in 0..w {
            for &op in [c.a[j], c.b[j]].iter() {
                for (i, &s) in c.sum.iter().enumerate().skip(j) {
                    let d = if i == j {
                        CHAIN_NET + CHAIN_SUM
                    } else {
                        CHAIN_NET + CHAIN_ENTRY + (i - 1 - j) as u32 * CHAIN_BIT + CHAIN_SUM
                    };
                    arc(op, s, d);
                }
                if let Some(co) = c.cout {
                    arc(
                        op,
                        co,
                        CHAIN_NET + CHAIN_ENTRY + (w - 1 - j) as u32 * CHAIN_BIT + CHAIN_SUM,
                    );
                }
            }
        }
        for (i, &s) in c.sum.iter().enumerate() {
            arc(c.cin, s, CHAIN_NET + CHAIN_ENTRY + i as u32 * CHAIN_BIT + CHAIN_SUM);
        }
        if let Some(co) = c.cout {
            arc(c.cin, co, CHAIN_NET + CHAIN_ENTRY + w as u32 * CHAIN_BIT + CHAIN_SUM);
        }
    }
    t
}

/// Number of LUT4s in the netlist of a mapping of `aig` (the graph of
/// `net`), counting LUTs added for polarities.
fn netlist_luts(
    net: &LogicNet,
    aig: &synth_logic::Aig,
    m: &lutmap::Mapping,
) -> Result<usize, String> {
    let n = LogicNet { aig: aig.clone(), ..net.clone() };
    let nl = emit(&n, m)?;
    Ok(nl.live_cells().filter(|&c| nl.cell_kind(c) == LUT4).count())
}

// ---------------------------------------------------------------- netlist

struct Emitter<'a> {
    net: &'a LogicNet,
    nl: Netlist,
    src: Vec<NetId>,
    lut_of: FxHashMap<u32, (NetId, Vec<u32>, u64)>,
    /// Mapped nodes whose LUT outputs the complement (chosen when most
    /// uses outside the LUT network want the complement).
    phase: FxHashMap<u32, bool>,
    inverted: FxHashMap<u32, NetId>,
    pads: Vec<NetId>,
    zero: Option<NetId>,
    one: Option<NetId>,
}

/// `INIT` of a LUT4 (entry `D C B A`) computing `truth` of its first `k`
/// inputs.
fn lut_init(truth: u64, k: usize) -> Param {
    let bits = (0..16)
        .map(|i| if (truth >> (i & ((1 << k) - 1))) & 1 == 1 { PBit::One } else { PBit::Zero })
        .collect();
    Param::Bits(bits)
}

fn bits16(v: u16) -> Param {
    Param::Bits((0..16).map(|i| if (v >> i) & 1 == 1 { PBit::One } else { PBit::Zero }).collect())
}

/// A carry chain operand: a constant, or a net carrying the operand or its
/// complement.
#[derive(Clone, Copy)]
enum Operand {
    Const(bool),
    Net(NetId, bool),
}

/// One half of a `CCU2C`: its operands on `A` and `B`, the table, and the
/// net of its sum.
struct Half {
    a: Option<NetId>,
    b: Option<NetId>,
    init: u16,
    sum: Option<NetId>,
}

/// The table of a half adding operands `a` and `b`: `P = a ^ b` where `C`
/// and `D` are high, `G = a` (the carry where `P` is low) in the entries
/// where they are low.
fn adder_half(a: Operand, b: Operand) -> (Option<NetId>, Option<NetId>, u16) {
    let pin = |o: Operand| match o {
        Operand::Net(n, _) => Some(n),
        Operand::Const(_) => None,
    };
    let value = |o: Operand, x: u16| match o {
        Operand::Const(c) => c,
        Operand::Net(_, inv) => (x == 1) ^ inv,
    };
    let (mut p, mut g) = (0u16, 0u16);
    for idx in 0..4u16 {
        let (va, vb) = (value(a, idx & 1), value(b, idx >> 1));
        p |= ((va ^ vb) as u16) << idx;
        g |= (va as u16) << idx;
    }
    (pin(a), pin(b), g | p << 4 | p << 8 | p << 12)
}

fn emit(net: &LogicNet, mapping: &lutmap::Mapping) -> Result<Netlist, String> {
    let mut e = Emitter {
        net,
        nl: Netlist::new(),
        src: Vec::new(),
        lut_of: FxHashMap::default(),
        phase: FxHashMap::default(),
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
    // LUTs, each in the polarity most of its sinks want (LUT users absorb
    // either).
    let mut refs: FxHashMap<u32, (u32, u32)> = FxHashMap::default();
    for o in &net.aig.outputs {
        let r = refs.entry(o.node()).or_default();
        if o.is_compl() {
            r.1 += 1;
        } else {
            r.0 += 1;
        }
    }
    for l in &mapping.luts {
        let o = e.nl.add_net(&format!("$n{}", l.node));
        e.lut_of.insert(l.node, (o, l.leaves.clone(), l.truth6()));
        let (pos, neg) = refs.get(&l.node).copied().unwrap_or_default();
        e.phase.insert(l.node, neg > pos);
    }
    for l in &mapping.luts {
        let out = e.lut_of[&l.node].0;
        let ins: Vec<NetId> = l.leaves.iter().map(|&x| e.node_net(x)).collect::<Result<_, _>>()?;
        let truth = e.net_truth(l.node, &l.leaves, l.truth6(), false);
        e.lut(&format!("$lut{}", l.node), &ins, truth, out)?;
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
        let c = e.nl.add_cell(&format!("{}[{i}]", ff.name), FF);
        let clk = e.sink(ff.clk)?;
        e.pin(c, "CLK", Dir::Input, Some(clk))?;
        let d = e.sink(ff.d)?;
        e.pin(c, "DI", Dir::Input, Some(d))?;
        e.nl.set_param(c, "CLKMUX", Param::Str(if ff.posedge { "CLK" } else { "INV" }.into()));
        match ff.en {
            Some(en) => {
                let n = e.sink(en)?;
                e.pin(c, "CE", Dir::Input, Some(n))?;
                e.nl.set_param(c, "CEMUX", Param::Str("CE".into()));
            }
            None => e.nl.set_param(c, "CEMUX", Param::Str("1".into())),
        }
        let reset = match (ff.srst, ff.arst) {
            (Some(r), None) => Some((r, "LSR_OVER_CE")),
            (None, Some(r)) => Some((r, "ASYNC")),
            (None, None) => None,
            _ => return Err("flip-flop with both resets after legalisation".into()),
        };
        // The set/reset value, else the power-up value.
        let set = match reset {
            Some(((s, v), mode)) => {
                let n = e.sink(s)?;
                e.pin(c, "LSR", Dir::Input, Some(n))?;
                e.nl.set_param(c, "SRMODE", Param::Str(mode.into()));
                v
            }
            None => ff.init == Logic::One,
        };
        e.nl.set_param(c, "REGSET", Param::Str(if set { "SET" } else { "RESET" }.into()));
        e.nl.set_param(c, "GSR", Param::Str("ENABLED".into()));
        let q = e.src[ff.q as usize];
        e.pin(c, "Q", Dir::Output, Some(q))?;
    }
    // Carry chains.
    for (ci, ch) in net.carries.iter().enumerate() {
        let mut halves: Vec<Half> = Vec::new();
        // The carry in, generated by the first half.
        let (a, init) = match e.operand(net.aig.outputs[ch.cin])? {
            Operand::Const(c) => (None, if c { 0x000F } else { 0 }),
            Operand::Net(n, inv) => (Some(n), if inv { 0x0005 } else { 0x000A }),
        };
        halves.push(Half { a, b: None, init, sum: None });
        for i in 0..ch.a.len() {
            let a = e.operand(net.aig.outputs[ch.a[i]])?;
            let b = e.operand(net.aig.outputs[ch.b[i]])?;
            let (a, b, init) = adder_half(a, b);
            let sum = ch.sum.get(i).map(|&s| e.src[s as usize]);
            halves.push(Half { a, b, init, sum });
        }
        if let Some(co) = ch.cout {
            // The carry out to logic: a half with P = 0 passes its carry in
            // to its sum.
            halves.push(Half { a: None, b: None, init: 0, sum: Some(e.src[co as usize]) });
        }
        let mut carry: Option<NetId> = None;
        let cells = halves.len().div_ceil(2);
        let mut it = halves.into_iter();
        for k in 0..cells {
            let c = e.nl.add_cell(&format!("$ccu{ci}_{k}"), CCU2C);
            let mut inits = [0u16; 2];
            for (h, half) in [it.next(), it.next()].into_iter().enumerate() {
                let Some(half) = half else { continue };
                inits[h] = half.init;
                if let Some(n) = half.a {
                    e.pin(c, &format!("A{h}"), Dir::Input, Some(n))?;
                }
                if let Some(n) = half.b {
                    e.pin(c, &format!("B{h}"), Dir::Input, Some(n))?;
                }
                if let Some(n) = half.sum {
                    e.pin(c, &format!("S{h}"), Dir::Output, Some(n))?;
                }
            }
            if let Some(cin) = carry {
                e.pin(c, "CIN", Dir::Input, Some(cin))?;
            }
            if k + 1 < cells {
                let co = e.nl.add_net(&format!("$ccu{ci}_c{k}"));
                e.pin(c, "COUT", Dir::Output, Some(co))?;
                carry = Some(co);
            }
            e.nl.set_param(c, "INIT0", bits16(inits[0]));
            e.nl.set_param(c, "INIT1", bits16(inits[1]));
            e.nl.set_param(c, "INJECT1_0", Param::Str("NO".into()));
            e.nl.set_param(c, "INJECT1_1", Param::Str("NO".into()));
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
    sweep(&mut e.nl);
    Ok(e.nl)
}

/// Removes LUTs nothing reads, until none is left.
fn sweep(nl: &mut Netlist) {
    loop {
        let dead: Vec<CellId> = nl
            .live_cells()
            .filter(|&c| {
                nl.cell_kind(c) == LUT4
                    && nl.pin_net(c, "Z").is_none_or(|n| nl.nets[n].users.is_empty())
            })
            .collect();
        if dead.is_empty() {
            break;
        }
        for c in dead {
            nl.remove_cell(c);
        }
    }
}

impl Emitter<'_> {
    fn pin(&mut self, c: CellId, name: &str, dir: Dir, net: Option<NetId>) -> Result<(), String> {
        self.nl.add_pin(c, name, dir, net).map(|_| ()).map_err(|e| e.0)
    }

    fn lut(&mut self, name: &str, ins: &[NetId], truth: u64, out: NetId) -> Result<(), String> {
        let c = self.nl.add_cell(name, LUT4);
        for (j, &n) in ins.iter().enumerate() {
            self.pin(c, ["A", "B", "C", "D"][j], Dir::Input, Some(n))?;
        }
        self.pin(c, "Z", Dir::Output, Some(out))?;
        self.nl.set_param(c, "INIT", lut_init(truth, ins.len()));
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

    fn phase(&self, node: u32) -> bool {
        self.phase.get(&node).copied().unwrap_or(false)
    }

    /// Truth table over the leaves' nets (which carry complemented values
    /// for leaves in negative phase) computing node `root`, complemented if
    /// the node's phase (xor `invert`) says so.
    fn net_truth(&self, root: u32, leaves: &[u32], truth: u64, invert: bool) -> u64 {
        let k = leaves.len();
        let mask: usize =
            leaves.iter().enumerate().map(|(j, &l)| (self.phase(l) as usize) << j).sum();
        let mut t = 0u64;
        for x in 0..(1usize << k) {
            if (truth >> (x ^ mask)) & 1 == 1 {
                t |= 1 << x;
            }
        }
        if self.phase(root) ^ invert {
            let all = if k >= 6 { u64::MAX } else { (1u64 << (1 << k)) - 1 };
            t = !t & all;
        }
        t
    }

    /// The net of an AIG node: its value, or its complement for mapped
    /// nodes in negative phase.
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

    /// A literal as a net and whether the net carries its complement.
    fn operand(&mut self, l: Lit) -> Result<Operand, String> {
        if l.is_const() {
            return Ok(Operand::Const(l == Lit::TRUE));
        }
        let n = self.node_net(l.node())?;
        Ok(Operand::Net(n, l.is_compl() != self.phase(l.node())))
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
        if l.is_compl() == self.phase(l.node()) {
            return Ok(n);
        }
        if let Some(&x) = self.inverted.get(&l.node()) {
            return Ok(x);
        }
        let out = self.nl.add_net(&format!("$inv{}", l.node()));
        match self.lut_of.get(&l.node()).cloned() {
            // A second LUT computing the other polarity.
            Some((_, leaves, truth)) => {
                let ins: Vec<NetId> =
                    leaves.iter().map(|&x| self.node_net(x)).collect::<Result<_, _>>()?;
                let t = self.net_truth(l.node(), &leaves, truth, true);
                self.lut(&format!("$lutn{}", l.node()), &ins, t, out)?;
            }
            None => self.lut(&format!("$not{}", l.node()), &[n], 0b01, out)?,
        }
        self.inverted.insert(l.node(), out);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sum and carry out of a half (`S = P ^ CI`, `CO = P ? CI : G`,
    /// with `C` and `D` high), given its pins.
    fn half(init: u16, a: bool, b: bool, ci: bool) -> (bool, bool) {
        let idx = a as u16 | (b as u16) << 1;
        let p = (init >> (12 + idx)) & 1 == 1;
        let g = (init >> idx) & 1 == 1;
        (p ^ ci, if p { ci } else { g })
    }

    #[test]
    fn adder_halves_add_their_operands() {
        let (x, y) = (NetId::new(0), NetId::new(1));
        for ia in [false, true] {
            for ib in [false, true] {
                for kb in [None, Some(false), Some(true)] {
                    let b = match kb {
                        Some(c) => Operand::Const(c),
                        None => Operand::Net(y, ib),
                    };
                    let (_, _, init) = adder_half(Operand::Net(x, ia), b);
                    for (pa, pb, ci) in (0..8).map(|v| (v & 1 == 1, v & 2 == 2, v & 4 == 4)) {
                        let a = pa ^ ia;
                        let bv = kb.unwrap_or(pb ^ ib);
                        let total = a as u8 + bv as u8 + ci as u8;
                        // An unconnected B reads high.
                        let pin_b = if kb.is_some() { true } else { pb };
                        assert_eq!(half(init, pa, pin_b, ci), (total & 1 == 1, total >= 2));
                    }
                }
            }
        }
    }
}
