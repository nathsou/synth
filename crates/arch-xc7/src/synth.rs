//! Native Artix-7 synthesis to LUT1–LUT6, CARRY4 and FDRE/FDSE/FDCE/FDPE.

use synth_core::{FxHashMap, Logic};
use synth_logic::aig::{Lit, Node};
use synth_logic::ff::FfRules;
use synth_logic::lutmap::{self, MapOptions, Truth};
use synth_logic::net::{Bit, LogicNet};
use synth_netlist::{Bit as PBit, CellId, Const, Dir, NetId, Netlist, Param};
use synth_rtl::{Attr, MemId, Module, PortDir, dff, mem, opt};

/// Synthesize a design without calling external tools.
pub fn synthesize(mut m: Module) -> Result<(Netlist, Vec<String>), String> {
    let mut log = Vec::new();
    opt::clean(&mut m)?;
    mem::constant_index_to_registers(&mut m)?;
    for _ in 0..8 {
        if opt::reduce_widths(&mut m) == 0 {
            break;
        }
        opt::clean(&mut m)?;
    }
    opt::shift_by_multiple(&mut m);
    opt::merge_identical(&mut m);
    opt::share_arith(&mut m);
    opt::chains_to_trees(&mut m);
    synth_rtl::fsm::simplify_state_compares(&mut m);
    opt::clean(&mut m)?;
    dff::extract_controls(&mut m);
    opt::clean(&mut m)?;
    if mem::remove_feedback_writes(&mut m) > 0 {
        opt::clean(&mut m)?;
    }
    mem::merge_write_ports(&mut m);
    // Shallow memories become LUT RAM with asynchronous reads; their read
    // registers stay ordinary flip-flops that pack beside the RAM outputs.
    let mut lutram = false;
    for mi in (0..m.memories.len()).map(MemId::new) {
        if m.memories[mi].alive
            && crate::lutram::eligible(&m.memories[mi])
            && !crate::bram::absorbs_read_pipeline(&m, mi)
        {
            let n = crate::lutram::map(&mut m, mi);
            let kind = if m.memories[mi].depth <= 32 { "RAM32M" } else { "RAM64M" };
            log.push(format!("{n} {kind} for {}", m.memories[mi].name));
            lutram = true;
        }
    }
    if lutram {
        opt::clean(&mut m)?;
    }
    // Read-register initialization belongs to the RAM output latch after
    // absorption. Keep it before the shared pass removes those registers.
    let mut read_init = FxHashMap::default();
    for c in m.cells.iter().filter(|c| c.alive) {
        if let synth_rtl::CellKind::Dff(ff) = &c.kind {
            for (b, v) in ff.q.iter().zip(ff.init.bits()) {
                read_init.insert(*b, v);
            }
        }
    }
    // An asynchronous read whose address registers are absorbed starts
    // with the word selected by those registers' initial values.
    for memory in m.memories.iter().filter(|mem| mem.alive) {
        for rd in &memory.reads {
            if rd.clk.is_some() || rd.addr.width() > 63 {
                continue;
            }
            let mut addr = 0u64;
            let mut known = true;
            for (i, b) in rd.addr.iter().enumerate() {
                let v = match b {
                    synth_rtl::SigBit::Const(v) => *v,
                    _ => read_init.get(b).copied().unwrap_or(Logic::X),
                };
                known &= matches!(v, Logic::Zero | Logic::One);
                addr |= ((v == Logic::One) as u64) << i;
            }
            let mask = (1u64 << rd.addr.width()) - 1;
            let index = addr.wrapping_sub(memory.offset as u64) & mask;
            if known && index < memory.depth as u64 {
                for (i, b) in rd.data.iter().enumerate() {
                    read_init.insert(*b, memory.init[index as usize].get(i));
                }
            }
        }
    }
    mem::merge_read_ffs(&mut m);
    for mi in (0..m.memories.len()).map(MemId::new) {
        if m.memories[mi].alive {
            if crate::bram::eligible(&m.memories[mi]) {
                let old_outputs: Vec<_> =
                    m.memories[mi].reads.iter().map(|r| r.data.clone()).collect();
                mem::emulate_transparency(&mut m, mi);
                for (old, rd) in old_outputs.iter().zip(&m.memories[mi].reads) {
                    for (from, to) in old.iter().zip(rd.data.iter()) {
                        if let Some(&init) = read_init.get(from) {
                            read_init.insert(*to, init);
                        }
                    }
                }
                let n = crate::bram::map(&mut m, mi, &read_init);
                log.push(format!("{n} RAMB18E1 for {}", m.memories[mi].name));
            } else if crate::lutram::eligible(&m.memories[mi]) {
                // Left for block RAM, but its read registers did not merge.
                let n = crate::lutram::map(&mut m, mi);
                let kind = if m.memories[mi].depth <= 32 { "RAM32M" } else { "RAM64M" };
                log.push(format!("{n} {kind} for {}", m.memories[mi].name));
            } else {
                log.push(format!("memory {} built from flip-flops", m.memories[mi].name));
                mem::to_registers(&mut m, mi)?;
            }
        }
    }
    opt::clean(&mut m)?;
    dff::extract_controls(&mut m);
    opt::clean(&mut m)?;
    let registered = crate::bram::absorb_output_registers(&mut m);
    if registered != 0 {
        log.push(format!("{registered} RAMB18E1 output registers"));
        opt::clean(&mut m)?;
    }
    let dsps = crate::dsp::map(&mut m);
    if dsps != 0 {
        log.push(format!("{dsps} DSP48E1 multiplier blocks"));
        opt::clean(&mut m)?;
    }
    let initial = synth_logic::lower::lower(&m, &Default::default())?;
    // A three-bit addition fits in LUT6s. Moving short arithmetic out of
    // carry boxes also exposes its surrounding logic to LUT optimization.
    // Try nearby thresholds only when they change a chain, and retain a
    // candidate only if no resource count or estimated delay gets worse.
    let alternatives: Vec<_> = [4, 5]
        .into_iter()
        .filter(|&min| {
            initial.carries.iter().any(|c| c.a.len() < min + if c.sum.is_empty() { 2 } else { 0 })
        })
        .collect();
    let lower = |min| {
        synth_logic::lower::lower(
            &m,
            &synth_logic::lower::LowerOptions { carry_min_width: Some(min) },
        )
    };
    let mut best = map_logic(initial, 8)?;
    let mut selected = 3;
    for min in alternatives {
        let candidate = map_logic(lower(min)?, 8)?;
        if candidate.improves(&best) {
            (best, selected) = (candidate, min);
        }
    }
    // More cuts can improve LUT6 area, but can also steer area recovery into
    // a worse local optimum. Retry only the selected arithmetic partition
    // and keep the existing mapping when any cost increases.
    let mut cuts = 8;
    let candidate = map_logic(lower(selected)?, 16)?;
    if candidate.improves(&best) {
        (best, cuts) = (candidate, 16);
    }
    best.share_luts()?;
    // Sharing arithmetic changes mapper heuristics as well as removing boxes.
    // Keep it only when the final resource vector and delay dominate the
    // unshared mapping; fewer LUTs alone must not buy extra carry blocks.
    let mut shared = lower(selected)?;
    if synth_logic::carry::merge_duplicate_carries(&mut shared) != 0 {
        let mut candidate = map_logic(shared, cuts)?;
        candidate.share_luts()?;
        if candidate.improves(&best) {
            best = candidate;
            best.log.push("shared equivalent carry chains".into());
        }
    }
    // Wide functions remap the selected partition only: comparing every
    // candidate with them would let their small savings steer the choice.
    let wide = MapOptions { wide: Some(lutmap::Wide { delay: vec![F7, F8] }), ..map_options(cuts) };
    if let Some((graph, mapping)) =
        synth_logic::mapping::widen(&best.net, &best.mapping, &wide, chain_timing, netlist_luts)?
    {
        let mut net = best.net.clone();
        net.aig = graph;
        let mut candidate = Mapped::new(net, mapping, best.log.clone())?;
        candidate.share_luts()?;
        if candidate.improves(&best) {
            let n = candidate.mapping.luts.iter().filter(|l| l.leaves.len() > 6).count();
            candidate.log.push(format!("{n} wide functions on MUXF7/MUXF8"));
            best = candidate;
        }
    }
    log.append(&mut best.log);
    let mut nl = best.nl;
    let srls = crate::srl::infer(&mut nl)?;
    if srls != 0 {
        log.push(format!("{srls} SRLC32E shift registers"));
    }
    let cost = resources(&nl);
    log.push(format!(
        "selected minimum carry width {selected}, {cuts} cuts; LUT/FF/CARRY4/BRAM18/DSP48 {cost:?}"
    ));
    Ok((nl, log))
}

/// MUXF7 and MUXF8 delays relative to a LUT and its routing (mapping
/// heuristics, like the carry weights below).
const F7: u32 = 50;
const F8: u32 = 25;

/// A candidate implementation: the logic network after optimization, its
/// mapping, and the emitted netlist.
struct Mapped {
    net: LogicNet,
    mapping: lutmap::Mapping,
    nl: Netlist,
    log: Vec<String>,
}

impl Mapped {
    fn new(net: LogicNet, mapping: lutmap::Mapping, log: Vec<String>) -> Result<Self, String> {
        let nl = emit(&net, &mapping)?;
        Ok(Mapped { net, mapping, nl, log })
    }

    /// Whether `self` dominates `other`: no resource count and no estimated
    /// delay increases, and something improves.
    fn improves(&self, other: &Mapped) -> bool {
        let (a, b) = (resources(&self.nl), resources(&other.nl));
        let (d, e) = (self.mapping.depth, other.mapping.depth);
        d <= e && a.iter().zip(b).all(|(&x, y)| x <= y) && (d < e || a != b)
    }

    fn share_luts(&mut self) -> Result<(), String> {
        merge_luts(&mut self.nl)?;
        sweep(&mut self.nl);
        Ok(())
    }
}

fn resources(nl: &Netlist) -> [usize; 5] {
    let mut counts = [0; 5];
    for c in nl.live_cells() {
        let k = nl.cell_kind(c);
        let index = if crate::pack::is_lut(k) {
            0
        } else if crate::pack::is_ff(k) {
            1
        } else {
            match k {
                "CARRY4" => 2,
                "RAMB18E1" => 3,
                "DSP48E1" => 4,
                _ => continue,
            }
        };
        counts[index] += 1;
    }
    counts
}

fn map_options(cuts_per_node: usize) -> MapOptions {
    MapOptions { k: 6, cuts_per_node, lut_delay: 100, ..Default::default() }
}

fn map_logic(mut net: LogicNet, cuts_per_node: usize) -> Result<Mapped, String> {
    synth_logic::ff::legalize(
        &mut net,
        &FfRules { init_one: true, srst_with_arst: false, srst_over_en: true },
    );
    synth_logic::seq::extract_sync_resets(&mut net);
    for ff in &mut net.ffs {
        if ff.en_over_srst
            && let (Some(en), Some((r, v))) = (ff.en, ff.srst)
        {
            let gated = net.aig.and(net.aig.outputs[en], net.aig.outputs[r]);
            ff.srst = Some((net.aig.output(gated), v));
            ff.en_over_srst = false;
        }
    }
    net.cleanup();
    let mut log = Vec::new();
    let opts = map_options(cuts_per_node);
    let mapping =
        synth_logic::mapping::optimize(&mut net, &opts, 6, chain_timing, netlist_luts, &mut log)?;
    Mapped::new(net, mapping, log)
}

// Relative mapping costs, not device STA or an Fmax prediction. Keep paths
// through hard carries and unregistered multipliers connected in the mapper's
// timing graph. A DSP multiply costs eight LUT levels here; this is a relative
// optimization weight, not a speed-grade characterization.
fn chain_timing(net: &LogicNet) -> lutmap::Timing {
    let mut t = lutmap::Timing::default();
    for c in &net.carries {
        let w = c.a.len();
        for j in 0..w {
            for &from in &[c.a[j], c.b[j]] {
                for (i, &to) in c.sum.iter().enumerate().skip(j) {
                    t.arcs.push(lutmap::BoxArc {
                        from,
                        to: to as usize,
                        delay: 100 + (i - j) as u32 * 5,
                    });
                }
                if let Some(to) = c.cout {
                    t.arcs.push(lutmap::BoxArc {
                        from,
                        to: to as usize,
                        delay: 100 + (w - j) as u32 * 5,
                    });
                }
            }
        }
        for (i, &to) in c.sum.iter().enumerate() {
            t.arcs.push(lutmap::BoxArc { from: c.cin, to: to as usize, delay: 20 + i as u32 * 5 });
        }
        if let Some(to) = c.cout {
            t.arcs.push(lutmap::BoxArc { from: c.cin, to: to as usize, delay: 20 + w as u32 * 5 });
        }
    }
    for instance in &net.instances {
        if instance.module != "DSP48E1"
            || !crate::dsp::REGISTERS.iter().all(|reg| {
                instance
                    .params
                    .iter()
                    .any(|(k, v)| k == reg && matches!(v, Attr::Const(v) if v.to_u64() == Some(0)))
            })
        {
            continue;
        }
        let Some((_, _, outputs)) = instance.conns.iter().find(|(k, _, _)| k == "P") else {
            continue;
        };
        for (pin, _, inputs) in &instance.conns {
            // The multiplier, or only the post-adder from C.
            let (width, delay) = match pin.as_str() {
                "A" => (25, 800),
                "B" => (18, 800),
                "C" => (48, 400),
                _ => continue,
            };
            for (i, input) in inputs.iter().take(width).enumerate() {
                let Bit::Snk(from) = *input else { continue };
                for output in outputs.iter().skip(i) {
                    if let Bit::Src(to) = *output {
                        t.arcs.push(lutmap::BoxArc { from, to: to as usize, delay });
                    }
                }
            }
        }
    }
    t
}

fn netlist_luts(
    net: &LogicNet,
    aig: &synth_logic::Aig,
    mapping: &lutmap::Mapping,
) -> Result<usize, String> {
    let mut n = net.clone();
    n.aig = aig.clone();
    let nl = emit(&n, mapping)?;
    Ok(nl.live_cells().filter(|&c| nl.cell_kind(c).starts_with("LUT")).count())
}

struct Emitter<'a> {
    net: &'a LogicNet,
    nl: Netlist,
    src: Vec<NetId>,
    lut_of: FxHashMap<u32, (NetId, Vec<u32>, Truth)>,
    /// Mapped nodes whose LUT outputs the complement (chosen when most
    /// uses outside the LUT network want the complement).
    phase: FxHashMap<u32, bool>,
    inverted: FxHashMap<u32, NetId>,
    pads: Vec<NetId>,
    zero: Option<NetId>,
    one: Option<NetId>,
}

pub(crate) fn lut_init(truth: u64, k: usize) -> Param {
    Param::Bits(
        (0..1usize << k)
            .map(|i| if truth >> i & 1 == 1 { PBit::One } else { PBit::Zero })
            .collect(),
    )
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
        e.lut_of.insert(l.node, (o, l.leaves.clone(), l.truth));
        let (pos, neg) = refs.get(&l.node).copied().unwrap_or_default();
        e.phase.insert(l.node, neg > pos);
    }
    // LUT levels, to give wide functions' multiplexer selects the latest
    // leaves: a select only passes through the multiplexers.
    let mut level: FxHashMap<u32, u32> = FxHashMap::default();
    for l in &mapping.luts {
        let deepest = l.leaves.iter().filter_map(|x| level.get(x)).max().copied().unwrap_or(0);
        level.insert(l.node, deepest + 1);
    }
    for l in &mapping.luts {
        let out = e.lut_of[&l.node].0;
        let ins: Vec<NetId> = l.leaves.iter().map(|&x| e.node_net(x)).collect::<Result<_, _>>()?;
        let truth = e.net_truth(l.node, &l.leaves, l.truth, false);
        let name = format!("$lut{}", l.node);
        if ins.len() <= 6 {
            e.lut(&name, &ins, truth[0], out)?;
        } else {
            let mut order: Vec<usize> = (0..ins.len()).collect();
            order.sort_by_key(|&j| (level.get(&l.leaves[j]).copied().unwrap_or(0), l.leaves[j]));
            e.wide(&name, &ins, truth, &order, out)?;
        }
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
    // Xilinx reset/preset overrides CE; the logic network is legalized for that priority.
    for (i, ff) in net.ffs.iter().enumerate() {
        let (kind, reset, pin) = match (ff.srst, ff.arst) {
            (Some((r, false)), None) => ("FDRE", Some(r), "R"),
            (Some((r, true)), None) => ("FDSE", Some(r), "S"),
            (None, Some((r, false))) => ("FDCE", Some(r), "CLR"),
            (None, Some((r, true))) => ("FDPE", Some(r), "PRE"),
            (None, None) => ("FDRE", None, "R"),
            _ => return Err("flip-flop with both resets after legalisation".into()),
        };
        let c = e.nl.add_cell(&format!("{}[{i}]", ff.name), kind);
        for (pin, sink, default) in [
            ("C", Some(ff.clk), false),
            ("D", Some(ff.d), false),
            ("CE", ff.en, true),
            (pin, reset, false),
        ] {
            let n = match sink {
                Some(k) => e.sink(k)?,
                None => e.konst(default),
            };
            e.pin(c, pin, Dir::Input, Some(n))?;
        }
        e.pin(c, "Q", Dir::Output, Some(e.src[ff.q as usize]))?;
        e.nl.set_param(c, "INIT", lut_init((ff.init == Logic::One) as u64, 0));
        e.nl.set_param(c, "IS_C_INVERTED", lut_init((!ff.posedge) as u64, 0));
    }
    // A CARRY4 stage computes S ? carry : DI, and O = S xor carry.
    // Set S = a xor b and DI = a. Pad partial groups with S=1, DI=0
    // so CO[3] is the real carry-out even when the width is not a multiple of four.
    for (ci, ch) in net.carries.iter().enumerate() {
        let mut carry = e.sink(ch.cin)?;
        for base in (0..ch.a.len()).step_by(4) {
            let c = e.nl.add_cell(&format!("$alu{ci}_carry{}", base / 4), "CARRY4");
            let zero = e.konst(false);
            e.pin(c, "CI", Dir::Input, Some(if base == 0 { zero } else { carry }))?;
            e.pin(c, "CYINIT", Dir::Input, Some(if base == 0 { carry } else { zero }))?;
            for j in 0..4 {
                let i = base + j;
                let (di, s) = if i < ch.a.len() {
                    let a = e.sink(ch.a[i])?;
                    let b = e.sink(ch.b[i])?;
                    let s = e.xor(a, b, &format!("$alu{ci}_p{i}"))?;
                    (a, s)
                } else {
                    (zero, e.konst(true))
                };
                e.pin(c, &format!("DI[{j}]"), Dir::Input, Some(di))?;
                e.pin(c, &format!("S[{j}]"), Dir::Input, Some(s))?;
                let out = ch.sum.get(i).map(|&s| e.src[s as usize]);
                e.pin(c, &format!("O[{j}]"), Dir::Output, out)?;
                let co = if j == 3 {
                    Some(if let Some(s) = ch.cout.filter(|_| base + 4 >= ch.a.len()) {
                        e.src[s as usize]
                    } else {
                        e.nl.add_net(&format!("$alu{ci}_c{}", base + 4))
                    })
                } else {
                    None
                };
                e.pin(c, &format!("CO[{j}]"), Dir::Output, co)?;
                if let Some(co) = co {
                    carry = co;
                }
            }
        }
    }
    // Instances.
    for inst in &net.instances {
        let c = e.nl.add_cell(&inst.name, &inst.module);
        for (name, dir, bits) in &inst.conns {
            let multi = bits.len() > 1;
            for (i, b) in bits.iter().enumerate() {
                let pname = if multi { format!("{name}[{i}]") } else { name.clone() };
                let pdir = match dir {
                    PortDir::Input => Dir::Input,
                    PortDir::Output => Dir::Output,
                    PortDir::Inout => Dir::Inout,
                };
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

impl Emitter<'_> {
    fn pin(&mut self, c: CellId, name: &str, dir: Dir, net: Option<NetId>) -> Result<(), String> {
        self.nl.add_pin(c, name, dir, net).map(|_| ()).map_err(|e| e.0)
    }

    fn lut(&mut self, name: &str, ins: &[NetId], truth: u64, out: NetId) -> Result<(), String> {
        if ins.is_empty() || ins.len() > 6 {
            return Err("invalid XC7 LUT width".into());
        }
        let c = self.nl.add_cell(name, &format!("LUT{}", ins.len()));
        for (j, &n) in ins.iter().enumerate() {
            self.pin(c, &format!("I{j}"), Dir::Input, Some(n))?;
        }
        self.pin(c, "O", Dir::Output, Some(out))?;
        self.nl.set_param(c, "INIT", lut_init(truth, ins.len()));
        Ok(())
    }

    /// A function of seven or eight inputs: LUT6 cofactors of the inputs
    /// last in `order`, joined by MUXF7 (and MUXF8) cells selected by them.
    fn wide(
        &mut self,
        name: &str,
        ins: &[NetId],
        truth: Truth,
        order: &[usize],
        out: NetId,
    ) -> Result<(), String> {
        let (data, selects) = order.split_at(6);
        let data_nets: Vec<NetId> = data.iter().map(|&j| ins[j]).collect();
        let mut level = Vec::new();
        for v in 0..1usize << selects.len() {
            let mut t = 0u64;
            for x in 0..64usize {
                let mut idx = 0;
                for (b, &j) in data.iter().enumerate() {
                    idx |= (x >> b & 1) << j;
                }
                for (b, &j) in selects.iter().enumerate() {
                    idx |= (v >> b & 1) << j;
                }
                t |= u64::from(lutmap::truth_bit(&truth, idx)) << x;
            }
            let o = self.nl.add_net(&format!("{name}_{v}"));
            self.lut(&format!("{name}_{v}"), &data_nets, t, o)?;
            level.push(o);
        }
        for (b, &j) in selects.iter().enumerate() {
            let kind = if b == 0 { "MUXF7" } else { "MUXF8" };
            let last = b + 1 == selects.len();
            let mut next = Vec::new();
            for pair in level.chunks(2) {
                let o = if last { out } else { self.nl.add_net(&format!("{name}_f7")) };
                let c = self.nl.add_cell(&format!("{name}_{}", kind.to_lowercase()), kind);
                self.pin(c, "I0", Dir::Input, Some(pair[0]))?;
                self.pin(c, "I1", Dir::Input, Some(pair[1]))?;
                self.pin(c, "S", Dir::Input, Some(ins[j]))?;
                self.pin(c, "O", Dir::Output, Some(o))?;
                next.push(o);
            }
            level = next;
        }
        Ok(())
    }

    fn xor(&mut self, a: NetId, b: NetId, name: &str) -> Result<NetId, String> {
        if a == b {
            return Ok(self.konst(false));
        }
        if self.nl.nets[a].constant == Some(Const::Zero) {
            return Ok(b);
        }
        if self.nl.nets[b].constant == Some(Const::Zero) {
            return Ok(a);
        }
        // Absorb operand LUTs into the propagate LUT when the joint support
        // fits a LUT6. This also absorbs complemented subtraction operands.
        let function = |n: NetId| -> (Vec<NetId>, u64) {
            if let Some(k) = self.nl.nets[n].constant {
                return (vec![], (k == Const::One) as u64);
            }
            if let Some(d) = self.nl.nets[n].driver {
                let kind = self.nl.cell_kind(d.cell);
                if let Some(k) = kind.strip_prefix("LUT").and_then(|k| k.parse::<usize>().ok()) {
                    let ins = (0..k)
                        .map(|i| self.nl.pin_net(d.cell, &format!("I{i}")).unwrap())
                        .collect();
                    return (ins, self.nl.param(d.cell, "INIT").and_then(Param::as_u64).unwrap());
                }
            }
            (vec![n], 2)
        };
        let (mut ai, mut at) = function(a);
        let (mut bi, mut bt) = function(b);
        let union = |a: &[NetId], b: &[NetId]| {
            let mut ins = a.to_vec();
            for &n in b {
                if !ins.contains(&n) {
                    ins.push(n);
                }
            }
            ins
        };
        let mut ins = union(&ai, &bi);
        if ins.len() > 6 {
            if union(&[a], &bi).len() <= 6 {
                ai = vec![a];
                at = 2;
            } else if union(&ai, &[b]).len() <= 6 {
                bi = vec![b];
                bt = 2;
            } else {
                ai = vec![a];
                at = 2;
                bi = vec![b];
                bt = 2;
            }
            ins = union(&ai, &bi);
        }
        if ins.is_empty() {
            return Ok(self.konst((at ^ bt) & 1 != 0));
        }
        let mut truth = 0;
        for x in 0..1usize << ins.len() {
            let eval = |inputs: &[NetId], t: u64| {
                let idx: usize = inputs
                    .iter()
                    .enumerate()
                    .map(|(i, n)| ((x >> ins.iter().position(|p| p == n).unwrap()) & 1) << i)
                    .sum();
                (t >> idx) & 1
            };
            truth |= (eval(&ai, at) ^ eval(&bi, bt)) << x;
        }
        let out = self.nl.add_net(name);
        self.lut(name, &ins, truth, out)?;
        Ok(out)
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
    fn net_truth(&self, root: u32, leaves: &[u32], truth: Truth, invert: bool) -> Truth {
        let k = leaves.len();
        let mask: usize =
            leaves.iter().enumerate().map(|(j, &l)| (self.phase(l) as usize) << j).sum();
        let flip = self.phase(root) ^ invert;
        let mut t = [0u64; 4];
        for x in 0..(1usize << k) {
            if lutmap::truth_bit(&truth, x ^ mask) != flip {
                t[x / 64] |= 1 << (x % 64);
            }
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
            Some((_, leaves, truth)) if leaves.len() <= 6 => {
                let ins: Vec<NetId> =
                    leaves.iter().map(|&x| self.node_net(x)).collect::<Result<_, _>>()?;
                let t = self.net_truth(l.node(), &leaves, truth, true);
                self.lut(&format!("$lutn{}", l.node()), &ins, t[0], out)?;
            }
            _ => self.lut(&format!("$not{}", l.node()), &[n], 0b01, out)?,
        }
        self.inverted.insert(l.node(), out);
        Ok(out)
    }
}

// Propagate-LUT folding can leave operand LUTs unreferenced.
fn sweep(nl: &mut Netlist) {
    loop {
        let dead: Vec<_> = nl
            .live_cells()
            .filter(|&c| {
                nl.cell_kind(c).starts_with("LUT")
                    && nl.pin_net(c, "O").is_none_or(|n| nl.nets[n].users.is_empty())
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

/// Fold exact duplicate LUT functions, including carry-propagate LUTs emitted
/// after mapping. Canonicalize the pin order so permuted functions can share.
/// Iterate because merging a driver can expose duplicates at its fanouts.
fn merge_luts(nl: &mut Netlist) -> Result<(), String> {
    loop {
        let mut functions = FxHashMap::default();
        let cells: Vec<_> = nl.live_cells().collect();
        let mut merged = 0;
        for c in cells {
            let Some(k) = nl.cell_kind(c).strip_prefix("LUT").and_then(|s| s.parse::<usize>().ok())
            else {
                continue;
            };
            if !(1..=6).contains(&k)
                || !nl.cells[c].attrs.is_empty()
                || nl.cells[c].params.len() != 1
            {
                continue;
            }
            let Some(Param::Bits(bits)) = nl.param(c, "INIT") else { continue };
            if bits.len() != 1 << k || bits.iter().any(|b| !matches!(b, PBit::Zero | PBit::One)) {
                continue;
            }
            let truth = nl.param(c, "INIT").and_then(Param::as_u64).unwrap();
            let Some(ins) =
                (0..k).map(|i| nl.pin_net(c, &format!("I{i}"))).collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let Some(out) = nl.pin_net(c, "O") else { continue };
            // A multiplexer's data inputs are its own LUTs in the slice.
            if nl.nets[out].users.iter().any(|u| nl.cell_kind(u.cell).starts_with("MUXF")) {
                continue;
            }
            let mut order: Vec<_> = (0..k).collect();
            order.sort_by_key(|&i| ins[i]);
            let inputs: Vec<_> = order.iter().map(|&i| ins[i]).collect();
            let mut canonical = 0u64;
            for x in 0..1usize << k {
                let original =
                    order.iter().enumerate().map(|(j, &i)| ((x >> j) & 1) << i).sum::<usize>();
                canonical |= ((truth >> original) & 1) << x;
            }
            if let Some(&existing) = functions.get(&(inputs.clone(), canonical)) {
                nl.remove_cell(c);
                nl.merge_nets(out, existing).map_err(|e| e.0)?;
                merged += 1;
            } else {
                functions.insert((inputs, canonical), out);
            }
        }
        if merged == 0 {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod lut_sharing_tests {
    use super::*;

    fn lut(nl: &mut Netlist, name: &str, ins: &[NetId], truth: u64) -> NetId {
        let c = nl.add_cell(name, &format!("LUT{}", ins.len()));
        let out = nl.add_net(name);
        for (i, &n) in ins.iter().enumerate() {
            nl.add_pin(c, &format!("I{i}"), Dir::Input, Some(n)).unwrap();
        }
        nl.add_pin(c, "O", Dir::Output, Some(out)).unwrap();
        nl.set_param(c, "INIT", lut_init(truth, ins.len()));
        out
    }

    #[test]
    fn shares_permuted_functions_and_their_fanouts() {
        let mut nl = Netlist::new();
        let a = nl.add_net("a");
        let b = nl.add_net("b");
        let x = lut(&mut nl, "x", &[a, b], 0b0010); // a & !b
        let y = lut(&mut nl, "y", &[b, a], 0b0100); // a & !b
        let p = lut(&mut nl, "p", &[x, a], 0b0110);
        let q = lut(&mut nl, "q", &[a, y], 0b0110);
        let ports: Vec<_> = [p, q]
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                let c = nl.add_cell(&format!("out{i}"), synth_netlist::OUTPUT_PORT);
                nl.add_pin(c, "PAD", Dir::Input, Some(n)).unwrap();
                c
            })
            .collect();
        merge_luts(&mut nl).unwrap();
        assert_eq!(nl.live_cells().filter(|&c| crate::pack::is_lut(nl.cell_kind(c))).count(), 2);
        assert_eq!(nl.pin_net(ports[0], "PAD"), nl.pin_net(ports[1], "PAD"));
        nl.check().unwrap();
    }

    #[test]
    fn distinguishes_functions_and_preserves_unknown_initialization() {
        let mut nl = Netlist::new();
        let a = nl.add_net("a");
        let b = nl.add_net("b");
        lut(&mut nl, "and", &[a, b], 0b1000);
        lut(&mut nl, "or", &[a, b], 0b1110);
        let x = lut(&mut nl, "unknown", &[a, b], 0b1000);
        let c = nl.nets[x].driver.unwrap().cell;
        nl.set_param(c, "INIT", Param::Bits(vec![PBit::X, PBit::Zero, PBit::Zero, PBit::One]));
        merge_luts(&mut nl).unwrap();
        assert_eq!(nl.live_cells().count(), 3);
        nl.check().unwrap();
    }
}
