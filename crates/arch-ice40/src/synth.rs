//! Synthesis for iCE40: maps word-level RTL to iCE40 primitives
//! (`SB_LUT4`, `SB_CARRY`, `SB_DFF*`, `SB_RAM40_4K`) for the packer.

use std::time::Instant;

use synth_core::{FxHashMap, FxHashSet, Logic, LogicVec};
use synth_logic::aig::{Lit, Node};
use synth_logic::choice::ChoiceAig;
use synth_logic::ff::FfRules;
use synth_logic::lower::LowerOptions;
use synth_logic::lutmap::{self, MapOptions, Tiebreak};
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
    /// Rounds of restructuring and remapping the mapped LUTs.
    pub remap_rounds: usize,
    /// Memories with fewer bits are built from flip-flops.
    pub bram_min_bits: u64,
}

impl Default for SynthOptions {
    fn default() -> Self {
        SynthOptions {
            lower: LowerOptions::default(),
            map: MapOptions { lut_delay: LEVEL, ..MapOptions::default() },
            remap_rounds: 6,
            bram_min_bits: 256,
        }
    }
}

/// Synthesises `m` into a netlist of iCE40 primitives.
pub fn synthesize(mut m: Module, opts: &SynthOptions) -> Result<(Netlist, Vec<String>), String> {
    let mut log = Vec::new();
    let t = Instant::now();
    opt::clean(&mut m)?;
    if mem::constant_index_to_registers(&mut m)? > 0 {
        opt::clean(&mut m)?;
    }
    // Narrower arithmetic first: shift amounts become recognisably narrow.
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
    let resets = synth_logic::seq::extract_sync_resets(&mut net);
    net.cleanup();
    log.push(format!(
        "lowered in {:.2}s: {} AND nodes, {} flip-flops ({} synchronous resets extracted), {} carry chains",
        t.elapsed().as_secs_f64(),
        net.aig.num_ands(),
        net.ffs.len(),
        resets,
        net.carries.len()
    ));
    if std::env::var("SYNTH_DEBUG").is_ok() {
        let w: Vec<String> = net
            .carries
            .iter()
            .map(|c| format!("{}{}", c.a.len(), if c.sum.is_empty() { "c" } else { "" }))
            .collect();
        log.push(format!("carry chains: {}", w.join(" ")));
    }
    if std::env::var("SYNTH_DEBUG").is_ok() {
        // AND nodes in each flip-flop's exclusive input cone, by register.
        let g = &net.aig;
        let refs = g.fanout_counts();
        let mut cost: FxHashMap<String, usize> = FxHashMap::default();
        for ff in &net.ffs {
            let mut seen = synth_core::FxHashSet::default();
            let mut stack: Vec<u32> = [Some(ff.d), ff.en, ff.srst.map(|x| x.0)]
                .into_iter()
                .flatten()
                .map(|k| g.outputs[k].node())
                .collect();
            let mut n = 0;
            while let Some(x) = stack.pop() {
                if !seen.insert(x) {
                    continue;
                }
                if let Some((a, b)) = g.fanins(x) {
                    n += 1;
                    for f in [a.node(), b.node()] {
                        if refs[f as usize] <= 1 {
                            stack.push(f);
                        }
                    }
                }
            }
            let name = ff.name.trim_end_matches("$dff").to_string();
            *cost.entry(name).or_default() += n;
        }
        let mut v: Vec<_> = cost.into_iter().collect();
        v.sort_by_key(|a| std::cmp::Reverse(a.1));
        v.truncate(25);
        log.push(format!("exclusive input cones: {v:?}"));
    }
    let t = Instant::now();
    let before = net.aig.num_ands();
    let mut merged = 0;
    let mut chain_bits = 0;
    let mut const_ffs = 0;
    // Merging equivalent logic can make carry-chain operands constant;
    // simplifying the chains feeds their sums back into the graph.
    // The graph before the last fraiging: alternative structures for
    // choices, if nothing outside the graph changed after it.
    let mut unfraiged = None;
    for _ in 0..4 {
        unfraiged = Some(net.aig.clone());
        let (aig, st) = synth_logic::fraig::fraig(&net.aig, &Default::default());
        net.aig = aig;
        merged += st.proved;
        let removed = synth_logic::carry::simplify_carries(&mut net);
        chain_bits += removed;
        let ffs = synth_logic::seq::remove_constant_ffs(&mut net)
            + synth_logic::seq::merge_duplicate_ffs(&mut net);
        const_ffs += ffs;
        net.cleanup();
        if removed + ffs == 0 {
            break;
        }
        unfraiged = None;
    }
    // Snapshots for structural choices: the fraiged graph, its balanced
    // form, rounds of rewriting scripts (the last is the mapped graph) and
    // rounds of refactoring.
    let fraiged = net.aig.clone();
    let balanced = synth_logic::opt::balance(&fraiged).cleanup();
    let compressed = synth_logic::opt::compress(&fraiged);
    let compressed2 = synth_logic::opt::compress2(&compressed);
    let main = synth_logic::opt::compress2(&compressed2);
    let mut refactored = fraiged.clone();
    for _ in 0..3 {
        let o = synth_logic::refactor::RefactorOptions::default();
        refactored =
            synth_logic::opt::balance(&synth_logic::refactor::refactor(&refactored, &o)).cleanup();
    }
    net.aig = main;
    net.cleanup();
    log.push(format!(
        "optimised in {:.2}s: {} -> {} AND nodes ({} merged), {} carry bits and {} flip-flops removed",
        t.elapsed().as_secs_f64(),
        before,
        net.aig.num_ands(),
        merged,
        chain_bits,
        const_ffs
    ));
    if let Ok(path) = std::env::var("SYNTH_DUMP_AIG") {
        // Debugging aid: the graph about to be mapped, for other tools.
        let _ = std::fs::write(path, net.aig.to_aiger());
    }
    let t = Instant::now();
    let mut snapshots = vec![&net.aig, &balanced, &fraiged];
    snapshots.extend(unfraiged.as_ref());
    snapshots.extend([&compressed, &compressed2, &refactored]);
    let choices = synth_logic::choice::build(&snapshots, &Default::default());
    log.push(format!(
        "{} structural choices in {:.2}s",
        choices.num_choices(),
        t.elapsed().as_secs_f64()
    ));
    // Candidate mappings are compared on their netlists: shallower, then
    // fewer LUTs. Choices usually help, but not always.
    let t = Instant::now();
    let main = std::mem::take(&mut net.aig);
    let mut best = map_best(&net, choices, &opts.map)?;
    let plain = map_best(&net, ChoiceAig::plain(main.clone()), &opts.map)?;
    if (plain.mapping.depth, plain.luts) < (best.mapping.depth, best.luts) {
        best = plain;
    }
    log.push(format!(
        "first mapping: {} LUTs, delay {:.2} LUT levels",
        best.mapping.luts.len(),
        best.mapping.depth as f64 / LEVEL as f64
    ));
    // Remapping: the mapped LUTs as a graph, restructured, with the previous
    // structure as choices; kept while the netlist shrinks without getting
    // deeper.
    let mut prev = main;
    for round in 1..=opts.remap_rounds {
        let lg = lutmap::lut_graph(&best.graph, &best.mapping);
        let restructured = synth_logic::opt::compress2(&lg);
        let choices = synth_logic::choice::build(&[&restructured, &lg, &prev], &Default::default());
        let c = map_best(&net, choices, &opts.map)?;
        if c.luts >= best.luts || c.mapping.depth > best.mapping.depth {
            break;
        }
        log.push(format!(
            "remapping round {round}: {} LUTs, delay {:.2} LUT levels",
            c.mapping.luts.len(),
            c.mapping.depth as f64 / LEVEL as f64
        ));
        // Rounds saving less than 0.1% are not worth their time.
        let last = (best.luts - c.luts) * 1024 < best.luts;
        (best, prev) = (c, restructured);
        if last {
            break;
        }
    }
    best = polish(&net, best, &opts.map)?;
    log.push(format!(
        "mapped in {:.2}s: {} LUTs, delay {:.2} LUT levels",
        t.elapsed().as_secs_f64(),
        best.mapping.luts.len(),
        best.mapping.depth as f64 / LEVEL as f64
    ));
    let mapping = best.mapping;
    // The mapping refers to the nodes of the graph with choices.
    net.aig = best.graph;
    if std::env::var("SYNTH_DEBUG").is_ok() {
        // The deepest sinks after mapping.
        let mut level: FxHashMap<u32, u32> = FxHashMap::default();
        for l in &mapping.luts {
            let lv =
                1 + l.leaves.iter().map(|x| level.get(x).copied().unwrap_or(0)).max().unwrap_or(0);
            level.insert(l.node, lv);
        }
        let lv = |k: usize| level.get(&net.aig.outputs[k].node()).copied().unwrap_or(0);
        let mut deep: Vec<(u32, String)> = net
            .ffs
            .iter()
            .map(|f| (lv(f.d).max(f.en.map_or(0, lv)), f.name.clone()))
            .chain(net.carries.iter().enumerate().map(|(i, c)| {
                (
                    c.a.iter().chain(&c.b).map(|&k| lv(k)).max().unwrap_or(0),
                    format!("carry chain {i}"),
                )
            }))
            .collect();
        deep.sort_by_key(|d| std::cmp::Reverse(d.0));
        deep.dedup_by(|a, b| a.1 == b.1);
        deep.truncate(8);
        log.push(format!("deepest sinks: {deep:?}"));
    }
    let nl = emit(&net, &mapping)?;
    Ok((nl, log))
}

/// A mapping, the graph it maps and the LUTs of its netlist.
struct Candidate {
    graph: synth_logic::Aig,
    mapping: lutmap::Mapping,
    luts: usize,
}

/// Delays of the paths through carry chains, in units of a hundredth of a
/// LUT level (a LUT and a typical net, about 1.15 ns): a net into the
/// chain, the carry logic, 11 per bit of carry propagation and the carry
/// into the sum LUT (IceStorm timing of the logic cell).
const CHAIN_NET: u32 = 60;
const CHAIN_ENTRY: u32 = 20;
const CHAIN_BIT: u32 = 11;
const CHAIN_SUM: u32 = 27;
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
                        LEVEL
                    } else {
                        CHAIN_NET + CHAIN_ENTRY + (i - 1 - j) as u32 * CHAIN_BIT + CHAIN_SUM
                    };
                    arc(op, s, d);
                }
                if let Some(co) = c.cout {
                    arc(op, co, CHAIN_NET + CHAIN_ENTRY + (w - 1 - j) as u32 * CHAIN_BIT);
                }
            }
        }
        for (i, &s) in c.sum.iter().enumerate() {
            arc(c.cin, s, CHAIN_NET + i as u32 * CHAIN_BIT + CHAIN_SUM);
        }
        if let Some(co) = c.cout {
            arc(c.cin, co, CHAIN_NET + w as u32 * CHAIN_BIT);
        }
    }
    t
}

/// The best mapping of `c` for `net` with either delay tie-break.
fn map_best(net: &LogicNet, c: ChoiceAig, opts: &MapOptions) -> Result<Candidate, String> {
    let timing = chain_timing(net);
    let mut best: Option<(lutmap::Mapping, usize)> = None;
    for delay_tiebreak in [Tiebreak::Size, Tiebreak::AreaFlow] {
        let m =
            lutmap::map_choices_timed(&c, &MapOptions { delay_tiebreak, ..opts.clone() }, &timing);
        let luts = netlist_luts(net, &c.aig, &m)?;
        if best.as_ref().is_none_or(|(b, l)| (m.depth, luts) < (b.depth, *l)) {
            best = Some((m, luts));
        }
    }
    let (mapping, luts) = best.expect("two mappings tried");
    Ok(Candidate { graph: c.aig, mapping, luts })
}

/// `c` after don't-care optimisation, if that shrinks its netlist.
fn polish(net: &LogicNet, c: Candidate, opts: &MapOptions) -> Result<Candidate, String> {
    let (mut graph, mut mapping) = (c.graph.clone(), c.mapping.clone());
    let o =
        synth_logic::mfs::MfsOptions { k: opts.k, lut_delay: opts.lut_delay, ..Default::default() };
    let timing = lutmap::resolved_timing(&graph, &mapping, &chain_timing(net), opts.lut_delay);
    synth_logic::mfs::optimize(&mut graph, &mut mapping, &o, &timing);
    let luts = netlist_luts(net, &graph, &mapping)?;
    if luts < c.luts && mapping.depth <= c.mapping.depth {
        Ok(Candidate { graph, mapping, luts })
    } else {
        Ok(c)
    }
}

/// Number of LUTs in the netlist of a mapping of `aig` (the graph of
/// `net`), counting adder sums folded into their readers and LUTs added for
/// polarities.
fn netlist_luts(
    net: &LogicNet,
    aig: &synth_logic::Aig,
    m: &lutmap::Mapping,
) -> Result<usize, String> {
    let n = LogicNet {
        name: net.name.clone(),
        aig: aig.clone(),
        ports: net.ports.clone(),
        ffs: net.ffs.clone(),
        carries: net.carries.clone(),
        instances: net.instances.clone(),
        pads: net.pads,
    };
    let nl = emit(&n, m)?;
    Ok(nl.live_cells().filter(|&c| nl.cell_kind(c) == "SB_LUT4").count())
}

// ---------------------------------------------------------------- BRAM

fn bram_ok(mem: &Memory, opts: &SynthOptions) -> bool {
    mem.width as u64 * mem.depth as u64 >= opts.bram_min_bits
        && mem.writes.len() <= 1
        && !mem.reads.is_empty()
        && mem.reads.iter().all(|r| r.clk.is_some_and(|c| c.posedge))
        && mem.writes.iter().all(|w| w.clk.posedge)
}

/// Physical `RDATA`/`WDATA`/`MASK` pin of data bit `b` in a `w`-bit mode.
fn bram_pin(w: usize, b: usize) -> usize {
    const P2: [usize; 2] = [3, 11];
    const P4: [usize; 4] = [1, 9, 5, 13];
    const P8: [usize; 8] = [0, 8, 4, 12, 2, 10, 6, 14];
    match w {
        2 => P2[b],
        4 => P4[b],
        8 => P8[b],
        // Bit-reversed, like the other modes' layout.
        _ => (b & 1) << 3 | (b & 2) << 1 | (b & 4) >> 1 | (b & 8) >> 3,
    }
}

/// The 11 physical address pins for word `word` (a signal of `abits` bits)
/// in a `w`-bit mode: the logical address counts 2-bit units, and its three
/// low bits go to the top pins in reverse.
fn bram_addr(word: &Sig, w: usize) -> Sig {
    let shift = (w / 2).trailing_zeros() as usize;
    // Logical address A (11 bits) = word << shift.
    let mut a = [SigBit::ZERO; 11];
    for (i, &b) in word.iter().enumerate() {
        if i + shift < 11 {
            a[i + shift] = b;
        }
    }
    // RADDR = {A[0], A[1], A[2], A[10:3]}: pins 0..7 carry A[3..10].
    let mut pins = a[3..11].to_vec();
    pins.extend([a[2], a[1], a[0]]);
    Sig(pins)
}

/// Replaces memory `mi` (synchronous, non-transparent reads; at most one
/// write port) with `SB_RAM40_4K` blocks: one set per read port, in the
/// 16/8/4/2-bit mode needing the fewest blocks, banked by the address bits
/// above the block depth. Returns the number of blocks.
fn map_bram(m: &mut Module, mi: MemId) -> usize {
    let mem = m.memories[mi].clone();
    let width = mem.width as usize;
    let depth = mem.depth as usize;
    let wr = mem.writes.first().cloned();
    // Narrow modes have no bit mask: each column's bits must share an enable.
    let uniform = |w: usize| {
        wr.as_ref().is_none_or(|wr| (0..width).all(|b| wr.en.0[b] == wr.en.0[b - b % w]))
    };
    let (mode, _) = [16usize, 8, 4, 2]
        .into_iter()
        .filter(|&w| w == 16 || uniform(w))
        .map(|w| (w, width.div_ceil(w) * depth.div_ceil(4096 / w)))
        .min_by_key(|&(w, n)| (n, usize::MAX - 4096 / w))
        .unwrap();
    let cols = width.div_ceil(mode);
    let wdepth = 4096 / mode;
    let abits = wdepth.trailing_zeros() as usize;
    let banks = depth.div_ceil(wdepth);
    let bank_bits = banks.next_power_of_two().trailing_zeros() as usize;
    let mode_param = match mode {
        16 => 0,
        8 => 1,
        4 => 2,
        _ => 3,
    };
    let mut count = 0;
    // Word index (address minus offset), abits + bank_bits wide.
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
    let windex = wr.as_ref().map(|w| index(m, &w.addr));
    for rd in &mem.reads {
        let clk = rd.clk.unwrap();
        let rindex = index(m, &rd.addr);
        let mut bank_data: Vec<Sig> = Vec::new();
        for bank in 0..banks {
            let mut data = Sig::new();
            for col in 0..cols {
                let lo = col * mode;
                let n = (width - lo).min(mode);
                let rdata = m.new_sig(16);
                data.0.extend((0..n).map(|b| rdata.0[bram_pin(mode, b)]));
                let raddr = bram_addr(&Sig(rindex.0[..abits].to_vec()), mode);
                let mut conns: Vec<(String, PortDir, Sig)> = vec![
                    ("RDATA".into(), PortDir::Output, rdata),
                    ("RADDR".into(), PortDir::Input, raddr),
                    ("RCLK".into(), PortDir::Input, Sig::bit(clk.sig)),
                    ("RCLKE".into(), PortDir::Input, Sig::bit(SigBit::ONE)),
                    ("RE".into(), PortDir::Input, Sig::bit(rd.en)),
                ];
                match (&wr, &windex) {
                    (Some(w), Some(wi)) => {
                        let in_bank = if banks == 1 {
                            SigBit::ONE
                        } else {
                            let hi = Sig(wi.0[abits..].to_vec());
                            m.eq_const(hi, &LogicVec::from_u64(bank as u64, bank_bits))
                        };
                        let en: Vec<SigBit> = (0..n).map(|i| w.en.0[lo + i]).collect();
                        let same = en.iter().all(|&e| e == en[0]);
                        let any = if same { en[0] } else { m.reduce_bool(Sig(en.clone())) };
                        let we = m.and1(any, in_bank);
                        let mut wdata = vec![SigBit::ZERO; 16];
                        let mut mask = vec![SigBit::ZERO; 16];
                        for b in 0..n {
                            wdata[bram_pin(mode, b)] = w.data.0[lo + b];
                            if !same {
                                mask[bram_pin(mode, b)] = m.not1(en[b]);
                            }
                        }
                        let waddr = bram_addr(&Sig(wi.0[..abits].to_vec()), mode);
                        conns.push(("WADDR".into(), PortDir::Input, waddr));
                        conns.push(("WDATA".into(), PortDir::Input, Sig(wdata)));
                        conns.push(("MASK".into(), PortDir::Input, Sig(mask)));
                        conns.push(("WCLK".into(), PortDir::Input, Sig::bit(w.clk.sig)));
                        conns.push(("WCLKE".into(), PortDir::Input, Sig::bit(SigBit::ONE)));
                        conns.push(("WE".into(), PortDir::Input, Sig::bit(we)));
                    }
                    _ => {
                        conns.push(("WE".into(), PortDir::Input, Sig::bit(SigBit::ZERO)));
                        conns.push(("WCLKE".into(), PortDir::Input, Sig::bit(SigBit::ZERO)));
                    }
                }
                // Initial contents: logical bit `word * mode + b` of the block,
                // sliced into INIT_0..F like the hardware expects.
                let mut logical = vec![false; 4096];
                for wd in 0..wdepth {
                    let word = bank * wdepth + wd;
                    if word >= depth {
                        break;
                    }
                    for b in 0..n {
                        if mem.init[word].get(lo + b) == Logic::One {
                            logical[wd * mode + b] = true;
                        }
                    }
                }
                let mut params = vec![
                    ("READ_MODE".to_string(), Attr::Const(LogicVec::from_u64(mode_param, 2))),
                    ("WRITE_MODE".to_string(), Attr::Const(LogicVec::from_u64(mode_param, 2))),
                ];
                for k in 0..16usize {
                    let bits: Vec<Logic> = (0..256usize)
                        .map(|i| {
                            let a = k << 8
                                | (i >> 4) << 4
                                | (i & 1) << 3
                                | (i & 2) << 1
                                | (i & 4) >> 1
                                | (i & 8) >> 3;
                            if logical[a] { Logic::One } else { Logic::Zero }
                        })
                        .collect();
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
            let sel = Sig(rindex.0[abits..].to_vec());
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
    /// Mapped nodes whose LUT outputs the complement (chosen when most
    /// uses outside the LUT network want the complement).
    phase: FxHashMap<u32, bool>,
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
    // A carry chain is self-dual: with every operand bit and the carry in
    // complemented, it computes the complemented sums and carry out. Chains
    // take the form needing fewer inverters for their operands, when only
    // LUTs read their carry out (they absorb its complement); their sum LUTs
    // then compute XNOR.
    let flipped: Vec<bool> = net
        .carries
        .iter()
        .map(|ch| {
            let cout_read = ch.cout.is_some_and(|s| refs.contains_key(&net.aig.inputs[s as usize]));
            let inverters = |flip: bool| {
                let mut nodes: FxHashSet<u32> = FxHashSet::default();
                for &k in ch.a.iter().chain(&ch.b).chain([&ch.cin]) {
                    let l = net.aig.outputs[k].xor(flip);
                    if !l.is_const() && l.is_compl() != e.phase(l.node()) {
                        nodes.insert(l.node());
                    }
                }
                nodes.len()
            };
            !cout_read && inverters(true) < inverters(false)
        })
        .collect();
    for (ch, _) in net.carries.iter().zip(&flipped).filter(|(_, f)| **f) {
        if let Some(s) = ch.cout {
            e.phase.insert(net.aig.inputs[s as usize], true);
        }
    }
    for l in &mapping.luts {
        let out = e.lut_of[&l.node].0;
        let ins: Vec<NetId> = l.leaves.iter().map(|&x| e.node_net(x)).collect::<Result<_, _>>()?;
        let truth = e.net_truth(l.node, &l.leaves, l.truth, false);
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
    let mut sums = Vec::new();
    for (ci, ch) in net.carries.iter().enumerate() {
        let flip = flipped[ci];
        let operand = |e: &mut Emitter, k: usize| e.lit_net(net.aig.outputs[k].xor(flip));
        let mut carry = operand(&mut e, ch.cin)?;
        let w = ch.a.len();
        for i in 0..w {
            let a = operand(&mut e, ch.a[i])?;
            let b = operand(&mut e, ch.b[i])?;
            if let Some(&s) = ch.sum.get(i) {
                let c = e.nl.add_cell(&format!("$alu{ci}_lut{i}"), "SB_LUT4");
                e.pin(c, "I1", Dir::Input, Some(a))?;
                e.pin(c, "I2", Dir::Input, Some(b))?;
                e.pin(c, "I3", Dir::Input, Some(carry))?;
                let out = e.src[s as usize];
                e.pin(c, "O", Dir::Output, Some(out))?;
                let init = if flip { !xor3 & 0xffff } else { xor3 };
                e.nl.set_param(c, "LUT_INIT", lut_init(init, 4));
                sums.push(SumLut { cell: c, a, b, carry, out, xnor: flip });
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
    absorb_sums(&mut e.nl, &sums);
    sweep(&mut e.nl);
    Ok(e.nl)
}

/// The LUT computing bit `out = a ^ b ^ carry` (complemented if `xnor`) of
/// an adder on a carry chain.
struct SumLut {
    cell: CellId,
    a: NetId,
    b: NetId,
    carry: NetId,
    out: NetId,
    xnor: bool,
}

/// Folds each sum bit read only by one LUT into that LUT, when the LUT's
/// other inputs leave a pin free besides the adder's operands and carry: the
/// LUT then computes on `I1 = a`, `I2 = b`, `I3 = carry` and packs with the
/// carry in place of the sum LUT (as a conditional subtraction's selection
/// does, `t < 0 ? s : t` beside `t = s - m`). Returns the number folded.
fn absorb_sums(nl: &mut Netlist, sums: &[SumLut]) -> usize {
    let is_sum: FxHashSet<CellId> = sums.iter().map(|s| s.cell).collect();
    let mut taken: FxHashSet<CellId> = FxHashSet::default();
    let mut folded = 0;
    for s in sums {
        let users = &nl.nets[s.out].users;
        if users.len() != 1 || nl.nets[s.out].driver.map(|d| d.cell) != Some(s.cell) {
            continue;
        }
        let l = users[0].cell;
        if nl.cell_kind(l) != "SB_LUT4" || is_sum.contains(&l) || taken.contains(&l) {
            continue;
        }
        // The LUT's inputs, and the one net besides a, b, carry and the sum.
        let ins: Vec<Option<NetId>> = (0..4).map(|j| nl.pin_net(l, &format!("I{j}"))).collect();
        let mut extra: Option<NetId> = None;
        let mut fits = true;
        for n in ins.iter().flatten().copied() {
            if [s.a, s.b, s.carry, s.out].contains(&n) || nl.nets[n].constant.is_some() {
                continue;
            }
            match extra {
                None => extra = Some(n),
                Some(x) if x == n => {}
                Some(_) => fits = false,
            }
        }
        if !fits {
            continue;
        }
        let init = nl.param(l, "LUT_INIT").and_then(Param::as_u64).unwrap_or(0);
        let mut truth = 0u64;
        for idx in 0..16u64 {
            let (x, a, b, c) = (idx & 1, idx >> 1 & 1, idx >> 2 & 1, idx >> 3 & 1);
            let value = |n: NetId| -> u64 {
                if n == s.a {
                    a
                } else if n == s.b {
                    b
                } else if n == s.carry {
                    c
                } else if n == s.out {
                    a ^ b ^ c ^ s.xnor as u64
                } else if let Some(k) = nl.nets[n].constant {
                    (k == Const::One) as u64
                } else {
                    x
                }
            };
            let at: u64 = ins.iter().enumerate().map(|(j, n)| n.map_or(0, value) << j).sum();
            truth |= (init >> at & 1) << idx;
        }
        let pin_of = |nl: &Netlist, j: usize| {
            let i = nl.cells[l].pin_index(nl.names.get(&format!("I{j}"))?)?;
            Some(synth_netlist::PinRef { cell: l, pin: i as u32 })
        };
        for j in 0..4 {
            if let Some(p) = pin_of(nl, j) {
                nl.disconnect(p);
            }
        }
        let conns = [(0, extra), (1, Some(s.a)), (2, Some(s.b)), (3, Some(s.carry))];
        for (j, n) in conns {
            let Some(n) = n else { continue };
            let done = match pin_of(nl, j) {
                Some(p) => nl.connect(p, n),
                None => nl.add_pin(l, &format!("I{j}"), Dir::Input, Some(n)).map(|_| ()),
            };
            done.expect("LUT input connects");
        }
        nl.set_param(l, "LUT_INIT", lut_init(truth, 4));
        taken.insert(l);
        folded += 1;
    }
    folded
}

/// Removes LUTs and carry cells whose outputs drive nothing (such as sum
/// LUTs of adders whose sums are unused), repeatedly.
fn sweep(nl: &mut Netlist) {
    loop {
        let dead: Vec<CellId> = nl
            .live_cells()
            .filter(|&c| {
                let out = match nl.cell_kind(c) {
                    "SB_LUT4" => "O",
                    "SB_CARRY" => "CO",
                    _ => return false,
                };
                nl.pin_net(c, out).is_none_or(|n| nl.nets[n].users.is_empty())
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
    use synth_logic::Aig;
    use synth_logic::net::{Carry, Port};

    /// Evaluates a combinational netlist of `SB_LUT4` and `SB_CARRY` cells:
    /// the values of the output ports given those of the input ports.
    fn evaluate(nl: &Netlist, inputs: &FxHashMap<String, bool>) -> FxHashMap<String, bool> {
        let mut val: FxHashMap<NetId, bool> = FxHashMap::default();
        for (n, net) in nl.nets.iter_enumerated() {
            if let Some(k) = net.constant {
                val.insert(n, k == Const::One);
            }
        }
        let name = |c: CellId| nl.names.resolve(nl.cells[c].name).to_string();
        for c in nl.live_cells().filter(|&c| nl.cell_kind(c) == synth_netlist::INPUT_PORT) {
            val.insert(nl.pin_net(c, "PAD").unwrap(), inputs[&name(c)]);
        }
        loop {
            let mut progress = false;
            for c in nl.live_cells() {
                let get = |pin: &str| nl.pin_net(c, pin).map(|n| val.get(&n).copied());
                let (out, v) = match nl.cell_kind(c) {
                    "SB_LUT4" => {
                        let mut idx = 0;
                        let mut known = true;
                        for j in 0..4 {
                            match get(&format!("I{j}")) {
                                Some(Some(b)) => idx |= (b as u64) << j,
                                Some(None) => known = false,
                                None => {}
                            }
                        }
                        let init = nl.param(c, "LUT_INIT").and_then(Param::as_u64).unwrap();
                        ("O", known.then(|| (init >> idx) & 1 == 1))
                    }
                    "SB_CARRY" => {
                        let v = match (get("I0"), get("I1"), get("CI")) {
                            (Some(Some(a)), Some(Some(b)), Some(Some(ci))) => {
                                Some((a & b) | (a & ci) | (b & ci))
                            }
                            _ => None,
                        };
                        ("CO", v)
                    }
                    _ => continue,
                };
                let n = nl.pin_net(c, out).unwrap();
                if let (Some(v), None) = (v, val.get(&n)) {
                    val.insert(n, v);
                    progress = true;
                }
            }
            if !progress {
                break;
            }
        }
        nl.live_cells()
            .filter(|&c| nl.cell_kind(c) == synth_netlist::OUTPUT_PORT)
            .map(|c| (name(c), val[&nl.pin_net(c, "PAD").unwrap()]))
            .collect()
    }

    #[test]
    fn carry_chain_takes_the_polarity_needing_fewer_inverters() {
        // y = ~x + 5 on a chain whose operand is the complement of an input
        // (an inverter per bit), and c = cout & x[0] read by a LUT.
        let mut g = Aig::new();
        let x: Vec<Lit> = (0..4).map(|_| g.input()).collect();
        let s: Vec<Lit> = (0..4).map(|_| g.input()).collect();
        let cout = g.input();
        let a: Vec<usize> = x.iter().map(|&l| g.output(!l)).collect();
        let b: Vec<usize> = (0..4).map(|i| g.output(Lit::new(0, (5 >> i) & 1 == 1))).collect();
        let cin = g.output(Lit::FALSE);
        let y: Vec<usize> = s.iter().map(|&l| g.output(l)).collect();
        let t = g.and(cout, x[0]);
        let c = g.output(t);
        let port = |name: &str, dir: PortDir, bits: Vec<Bit>| Port { name: name.into(), dir, bits };
        let net = LogicNet {
            name: "t".into(),
            aig: g,
            ports: vec![
                port("x", PortDir::Input, (0..4).map(Bit::Src).collect()),
                port("y", PortDir::Output, y.into_iter().map(Bit::Snk).collect()),
                port("c", PortDir::Output, vec![Bit::Snk(c)]),
            ],
            carries: vec![Carry { a, b, cin, sum: (4..8).collect(), cout: Some(8) }],
            ..Default::default()
        };
        let mapping = lutmap::map(&net.aig, &MapOptions::default());
        let nl = emit(&net, &mapping).unwrap();
        // Four sum LUTs and the AND: no inverters.
        assert_eq!(nl.live_cells().filter(|&c| nl.cell_kind(c) == "SB_LUT4").count(), 5);
        for v in 0..16u32 {
            let inputs = (0..4).map(|i| (format!("x[{i}]"), (v >> i) & 1 == 1)).collect();
            let out = evaluate(&nl, &inputs);
            let sum = (!v & 15) + 5;
            for i in 0..4 {
                assert_eq!(out[&format!("y[{i}]")], (sum >> i) & 1 == 1, "x = {v}");
            }
            assert_eq!(out["c"], sum > 15 && v & 1 == 1, "x = {v}");
        }
    }
}
