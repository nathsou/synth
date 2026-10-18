//! Shared AIG optimization, structural choices and LUT remapping.
//! Architecture backends supply carry timing and the cost of the emitted netlist.

use crate::choice::ChoiceAig;
use crate::lutmap::{self, MapOptions, Tiebreak};
use crate::net::LogicNet;
use synth_core::time::Instant;

/// Optimizes `net` and returns a mapping of its final graph.
pub fn optimize(
    net: &mut LogicNet,
    opts: &MapOptions,
    remap_rounds: usize,
    timing: impl Fn(&LogicNet) -> lutmap::Timing,
    cost: impl Fn(&LogicNet, &crate::Aig, &lutmap::Mapping) -> Result<usize, String>,
    log: &mut Vec<String>,
) -> Result<lutmap::Mapping, String> {
    optimize_observed(net, opts, remap_rounds, timing, cost, log, |_| {})
}

/// Like [`optimize`], reporting the optimized graph before mapping.
pub fn optimize_observed(
    net: &mut LogicNet,
    opts: &MapOptions,
    remap_rounds: usize,
    timing: impl Fn(&LogicNet) -> lutmap::Timing,
    cost: impl Fn(&LogicNet, &crate::Aig, &lutmap::Mapping) -> Result<usize, String>,
    log: &mut Vec<String>,
    mut observe: impl FnMut(&LogicNet),
) -> Result<lutmap::Mapping, String> {
    let t = Instant::now();
    let before = net.aig.num_ands();
    let mut merged = 0;
    let mut chain_bits = 0;
    let mut lut_chains = 0;
    let mut const_ffs = 0;
    // Merging equivalent logic can make carry-chain operands constant;
    // simplifying the chains feeds their sums back into the graph.
    // The graph before the last fraiging: alternative structures for
    // choices, if nothing outside the graph changed after it.
    let mut unfraiged = None;
    for _ in 0..4 {
        unfraiged = Some(net.aig.clone());
        let (aig, st) = crate::fraig::fraig(&net.aig, &Default::default());
        net.aig = aig;
        merged += st.proved;
        let removed = crate::carry::simplify_carries(net);
        chain_bits += removed;
        let lowered = crate::carry::lower_lut_sized_carries(net, opts.k);
        lut_chains += lowered;
        let ffs = crate::seq::remove_constant_ffs(net) + crate::seq::merge_duplicate_ffs(net);
        const_ffs += ffs;
        net.cleanup();
        if removed + ffs + lowered == 0 {
            break;
        }
        unfraiged = None;
    }
    if lut_chains > 0 {
        log.push(format!("{lut_chains} carry chains mapped to LUTs (boundary inputs fit a LUT)"));
    }
    // Snapshots for structural choices: the fraiged graph, its balanced
    // form, rounds of rewriting scripts (the last is the mapped graph) and
    // rounds of refactoring.
    let fraiged = net.aig.clone();
    let balanced = crate::opt::balance(&fraiged).cleanup();
    let compressed = crate::opt::compress(&fraiged);
    let compressed2 = crate::opt::compress2(&compressed);
    let main = crate::opt::compress2(&compressed2);
    let mut refactored = fraiged.clone();
    for _ in 0..3 {
        let o = crate::refactor::RefactorOptions::default();
        refactored = crate::opt::balance(&crate::refactor::refactor(&refactored, &o)).cleanup();
    }
    net.aig = main;
    net.cleanup();
    observe(net);
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
    let choices = crate::choice::build(&snapshots, &Default::default());
    log.push(format!(
        "{} structural choices in {:.2}s",
        choices.num_choices(),
        t.elapsed().as_secs_f64()
    ));
    // Candidate mappings are compared on their netlists: shallower, then
    // fewer LUTs. Choices usually help, but not always.
    let t = Instant::now();
    let main = std::mem::take(&mut net.aig);
    let mut best = map_best(net, choices, opts, &timing(net), &cost)?;
    let plain = map_best(net, ChoiceAig::plain(main.clone()), opts, &timing(net), &cost)?;
    if (plain.mapping.depth, plain.luts) < (best.mapping.depth, best.luts) {
        best = plain;
    }
    log.push(format!(
        "first mapping: {} LUTs, delay {:.2} LUT levels",
        best.mapping.luts.len(),
        best.mapping.depth as f64 / opts.lut_delay as f64
    ));
    // Remapping: the mapped LUTs as a graph, restructured, with the previous
    // structure as choices; kept while the netlist shrinks without getting
    // deeper, or gets shallower without growing.
    let mut prev = main;
    for round in 1..=remap_rounds {
        let lg = lutmap::lut_graph(&best.graph, &best.mapping);
        let restructured = crate::opt::compress2(&lg);
        let choices = crate::choice::build(&[&restructured, &lg, &prev], &Default::default());
        let c = map_best(net, choices, opts, &timing(net), &cost)?;
        let (d, bd) = (c.mapping.depth, best.mapping.depth);
        if !(c.luts < best.luts && d <= bd || c.luts <= best.luts && d < bd) {
            break;
        }
        log.push(format!(
            "remapping round {round}: {} LUTs, delay {:.2} LUT levels",
            c.mapping.luts.len(),
            c.mapping.depth as f64 / opts.lut_delay as f64
        ));
        // Rounds saving less than 0.1% are not worth their time.
        let last = (best.luts - c.luts) * 1024 < best.luts;
        (best, prev) = (c, restructured);
        if last {
            break;
        }
    }
    best = polish(net, best, opts, &timing(net), &cost)?;
    log.push(format!(
        "mapped in {:.2}s: {} LUTs, delay {:.2} LUT levels",
        t.elapsed().as_secs_f64(),
        best.mapping.luts.len(),
        best.mapping.depth as f64 / opts.lut_delay as f64
    ));
    net.aig = best.graph;
    Ok(best.mapping)
}

/// A mapping, the graph it maps and the LUTs of its netlist.
struct Candidate {
    graph: crate::Aig,
    mapping: lutmap::Mapping,
    luts: usize,
}

/// The best mapping of `c` for `net` with either delay tie-break.
fn map_best(
    net: &LogicNet,
    c: ChoiceAig,
    opts: &MapOptions,
    timing: &lutmap::Timing,
    cost: &impl Fn(&LogicNet, &crate::Aig, &lutmap::Mapping) -> Result<usize, String>,
) -> Result<Candidate, String> {
    let mut best: Option<(lutmap::Mapping, usize)> = None;
    for delay_tiebreak in [Tiebreak::Size, Tiebreak::AreaFlow] {
        let m =
            lutmap::map_choices_timed(&c, &MapOptions { delay_tiebreak, ..opts.clone() }, timing);
        let luts = cost(net, &c.aig, &m)?;
        if best.as_ref().is_none_or(|(b, l)| (m.depth, luts) < (b.depth, *l)) {
            best = Some((m, luts));
        }
    }
    let (mapping, luts) = best.expect("two mappings tried");
    Ok(Candidate { graph: c.aig, mapping, luts })
}

/// `c` after don't-care optimisation, if that shrinks its netlist.
fn polish(
    net: &LogicNet,
    c: Candidate,
    opts: &MapOptions,
    timing: &lutmap::Timing,
    cost: &impl Fn(&LogicNet, &crate::Aig, &lutmap::Mapping) -> Result<usize, String>,
) -> Result<Candidate, String> {
    let (mut graph, mut mapping) = (c.graph.clone(), c.mapping.clone());
    let o = crate::mfs::MfsOptions { k: opts.k, lut_delay: opts.lut_delay, ..Default::default() };
    let timing = lutmap::resolved_timing(&graph, &mapping, timing, opts.lut_delay);
    crate::mfs::optimize(&mut graph, &mut mapping, &o, &timing);
    let luts = cost(net, &graph, &mapping)?;
    if luts < c.luts && mapping.depth <= c.mapping.depth {
        Ok(Candidate { graph, mapping, luts })
    } else {
        Ok(c)
    }
}
