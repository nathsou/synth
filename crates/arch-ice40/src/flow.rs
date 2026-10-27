//! The iCE40 implementation flow: pack, place, route, generate bitstream.

use synth_core::progress;
use synth_core::time::Instant;

use synth_netlist::Netlist;

use crate::arch::{Arch, Part};
use crate::bitstream::Config;
use crate::checkpoint::Checkpoint;
use crate::place::{PinConstraints, PlaceOptions};

/// Options for [`implement`].
#[derive(Clone, Debug)]
pub struct FlowOptions {
    pub part: Part,
    pub package: String,
    pub seed: u64,
    pub place_effort: f64,
    /// Weight of timing in the placement cost (0..1, 0 = wirelength only).
    pub timing_weight: f64,
    /// Requested clock frequencies.
    pub clocks: crate::timing::ClockConstraints,
    /// Rounds of rerouting critical connections once routing is legal (the
    /// best timed legal routing is kept).
    pub route_timing_rounds: usize,
    /// Placement seeds placed and routed (in parallel where there are
    /// threads), the best timed kept.
    pub tries: usize,
}

/// Default rounds of rerouting critical connections once routing is legal:
/// the best timed routing of these is kept (1% Fmax over the iCE40 suite).
pub const ROUTE_TIMING_ROUNDS: usize = 3;

/// Default placement seeds of a run: the best of four timed is 3.3% faster
/// than one (geometric mean over the iCE40 suite, every design faster).
pub const TRIES: usize = 4;

/// Default weight of timing against wirelength in the placement cost.
/// Since placement predicts delays from the routing graph, timing earns
/// more of the cost than VPR's 0.5: 0.8 gained 1.9% over 0.5 on the iCE40
/// suite (four tries), 0.9 no more.
pub const TIMING_WEIGHT: f64 = 0.8;

/// Result of a successful run.
#[derive(Debug)]
pub struct FlowResult {
    pub config: Config,
    pub log: Vec<String>,
    /// Whether every clock with a target meets it.
    pub timing_met: bool,
    /// Present when the caller requested an incremental checkpoint.
    pub checkpoint: Option<Checkpoint>,
}

/// An incremental result whose clock periods grow by more than this factor
/// over the checkpointed run's is replaced by fresh place and route.
const MAX_SLOWDOWN: f64 = 1.05;

/// The physical design at the end of a run, for inspection.
#[derive(Debug)]
pub struct Implementation {
    pub arch: Arch,
    /// The packed netlist, with LUT inputs permuted as routed.
    pub netlist: Netlist,
    pub placement: crate::place::Placement,
    pub routing: crate::route::Routing,
    pub timing: crate::timing::TimingReport,
}

/// Packs, places and routes a netlist of iCE40 primitives. On failure the
/// error message starts with the log so far (one `info:` line per entry).
pub fn implement(
    nl: Netlist,
    pins: &PinConstraints,
    opts: &FlowOptions,
) -> Result<FlowResult, String> {
    implement_with_checkpoint(nl, pins, opts, None, false)
}

/// [`implement`], also returning the physical design.
pub fn implement_detailed(
    nl: Netlist,
    pins: &PinConstraints,
    opts: &FlowOptions,
) -> Result<(FlowResult, Implementation), String> {
    let mut log = Vec::new();
    let mut detail = None;
    match run(nl, pins, opts, &mut log, None, false, &mut detail) {
        Ok((config, timing_met, checkpoint)) => {
            let detail = detail.expect("a successful run records its implementation");
            Ok((FlowResult { config, log, timing_met, checkpoint }, detail))
        }
        Err(e) => {
            let mut msg: String = log.iter().map(|l| format!("info: {l}\n")).collect();
            msg.push_str(&e);
            Err(msg)
        }
    }
}

/// Implements the current netlist, using a previous checkpoint as a hint,
/// and returns a new checkpoint. Synthesis and packing are never skipped.
pub fn implement_incremental(
    nl: Netlist,
    pins: &PinConstraints,
    opts: &FlowOptions,
    previous: Option<&Checkpoint>,
) -> Result<FlowResult, String> {
    implement_with_checkpoint(nl, pins, opts, previous, true)
}

fn implement_with_checkpoint(
    nl: Netlist,
    pins: &PinConstraints,
    opts: &FlowOptions,
    previous: Option<&Checkpoint>,
    save: bool,
) -> Result<FlowResult, String> {
    let mut log = Vec::new();
    match run(nl, pins, opts, &mut log, previous, save, &mut None) {
        Ok((config, timing_met, checkpoint)) => {
            Ok(FlowResult { config, log, timing_met, checkpoint })
        }
        Err(e) => {
            let mut msg: String = log.iter().map(|l| format!("info: {l}\n")).collect();
            msg.push_str(&e);
            Err(msg)
        }
    }
}

fn run(
    mut nl: Netlist,
    pins: &PinConstraints,
    opts: &FlowOptions,
    log: &mut Vec<String>,
    previous: Option<&Checkpoint>,
    save: bool,
    detail: &mut Option<Implementation>,
) -> Result<(Config, bool, Option<Checkpoint>), String> {
    let t0 = Instant::now();
    progress::step("device", "");
    let arch = Arch::new(opts.part, &opts.package)?;
    let previous = previous.filter(|c| {
        if c.compatible(&arch, opts) {
            true
        } else {
            log.push(
                "incremental checkpoint incompatible with device/options; using fresh PnR".into(),
            );
            false
        }
    });
    let fresh_input = previous.map(|_| nl.clone());
    log.push(format!(
        "device {} {} loaded in {:.2}s",
        opts.part,
        arch.package.name,
        t0.elapsed().as_secs_f64()
    ));

    let t = Instant::now();
    progress::step("pack", "");
    let (packed, report) = crate::pack::pack(&mut nl)?;
    log.extend(report.messages);
    let stats = nl.stats();
    log.push(format!(
        "packed in {:.2}s: {}",
        t.elapsed().as_secs_f64(),
        stats.iter().map(|(k, n)| format!("{n} {k}")).collect::<Vec<_>>().join(", ")
    ));

    // IOs and global networks once; then placement and routing, retrying
    // with less weight on timing if the placement does not route (timing
    // pulls critical logic together, which can congest local routing).
    let mut fixed = crate::place::Placement::default();
    // PLL outputs are clocks of the frequency they make.
    let clocks = crate::pll::clock_targets(&nl, &opts.clocks);
    crate::place::place_plls(&arch, &nl, &packed, pins, &mut fixed, log)?;
    crate::place::place_ios(&arch, &nl, pins, &mut fixed, log)?;
    crate::place::assign_globals(&arch, &mut nl, &mut fixed, log)?;
    let mut result = None;
    let mut incremental = false;
    if let Some(previous) = previous {
        let t = Instant::now();
        progress::step("place", "incremental");
        let mut placement = fixed.clone();
        let popts = PlaceOptions {
            seed: opts.seed,
            effort: opts.place_effort,
            timing_weight: opts.timing_weight,
            max_locals: 32,
            clocks: clocks.clone(),
        };
        let attempt = crate::place::place_incremental(
            &arch,
            &nl,
            &packed,
            &mut placement,
            &popts,
            &previous.placement(&nl),
            log,
        )
        .and_then(|()| {
            progress::step("route", "incremental");
            crate::route::route_incremental(
                &arch,
                &nl,
                &placement,
                &clocks,
                opts.route_timing_rounds,
                log,
                Some(previous),
            )
        });
        match attempt {
            Ok(routing) => {
                log.push(format!("incremental PnR in {:.2}s", t.elapsed().as_secs_f64()));
                result = Some((placement, routing));
                incremental = true;
            }
            Err(e) => log.push(format!("incremental repair failed: {e}; using fresh PnR")),
        }
    }
    if result.is_none() {
        result = Some(fresh_pnr(&arch, &nl, &packed, &fixed, &clocks, opts, log)?);
    }
    let (placement, routing) = result.expect("placed and routed");
    // Capture logical pin endpoints before applying the physical permutation.
    let mut checkpoint = if save {
        Some(Checkpoint::capture(&arch, &nl, &placement, &routing, opts)?)
    } else {
        None
    };
    if !routing.lut_pins.is_empty() {
        log.push(format!("{} LUTs with permuted inputs", routing.lut_pins.len()));
    }
    routing.apply_lut_permutation(&mut nl);

    let t = Instant::now();
    progress::step("timing", "");
    let timing = crate::timing::analyse(&arch, &nl, &placement, &routing, &clocks);
    let met = timing.clocks.iter().all(|c| c.target_mhz.is_none_or(|t| c.fmax_mhz >= t));
    for c in &timing.clocks {
        let target = match c.target_mhz {
            Some(t) if c.fmax_mhz >= t => format!(" (target {t:.1} MHz met)"),
            Some(t) => {
                format!(" (target {t:.1} MHz FAILED, slack {:.2} ns)", 1000.0 / t - c.period_ns)
            }
            None => String::new(),
        };
        log.push(format!(
            "clock {}: {:.2} ns, {:.1} MHz{target}",
            c.clock, c.period_ns, c.fmax_mhz
        ));
        log.push(format!("  critical path: {}", c.path.join(" -> ")));
    }
    log.push(format!("timing analysed in {:.2}s", t.elapsed().as_secs_f64()));

    if incremental && !met {
        log.push("incremental result misses timing targets; using fresh PnR".into());
        return run(fresh_input.unwrap(), pins, opts, log, None, save, detail);
    }
    // Quality: no clock may get much slower than in the checkpointed run.
    if let Some(previous) = previous.filter(|_| incremental) {
        let slower = timing.clocks.iter().find(|c| {
            previous.period(&c.clock).is_some_and(|p| c.period_ns * 1000.0 > p * MAX_SLOWDOWN)
        });
        if let Some(c) = slower {
            log.push(format!(
                "incremental result slows clock {} to {:.2} ns (checkpoint {:.2} ns); using fresh PnR",
                c.clock,
                c.period_ns,
                previous.period(&c.clock).unwrap_or(0.0) / 1000.0
            ));
            return run(fresh_input.unwrap(), pins, opts, log, None, save, detail);
        }
    }
    if let Some(c) = checkpoint.as_mut() {
        c.set_periods(&timing);
    }

    progress::step("bitstream", "");
    let config = crate::bitgen::generate(&arch, &nl, &placement, &routing)?;
    log.push(format!("total {:.2}s", t0.elapsed().as_secs_f64()));
    *detail = Some(Implementation { arch, netlist: nl, placement, routing, timing });
    Ok((config, met, checkpoint))
}

/// Fresh placement and routing: of `opts.tries` placement seeds, each
/// placed and routed on a thread of its own, the best timed.
fn fresh_pnr(
    arch: &Arch,
    nl: &Netlist,
    packed: &crate::pack::Packed,
    fixed: &crate::place::Placement,
    clocks: &crate::timing::ClockConstraints,
    opts: &FlowOptions,
    log: &mut Vec<String>,
) -> Result<(crate::place::Placement, crate::route::Routing), String> {
    let tries = opts.tries.max(1);
    if tries == 1 {
        return place_and_route(arch, nl, packed, fixed, clocks, opts, opts.seed, log);
    }
    // Seeds after the first are far from it, so that two runs' sets (and
    // their retries) do not overlap.
    let seeds: Vec<u64> =
        (0..tries as u64).map(|k| opts.seed.wrapping_add(k * 1_000_003)).collect();
    progress::step("place", &format!("{tries} placement seeds"));
    let runs = synth_core::par::map(&seeds, tries, |&seed| {
        let mut log = Vec::new();
        let run = place_and_route(arch, nl, packed, fixed, clocks, opts, seed, &mut log);
        // Each clock's period beyond its target (or its period, without
        // one), worst first: the routing's timing score.
        let score = run.as_ref().ok().map(|(placement, routing)| {
            let graph =
                crate::timing::TimingGraph::with_lut_pins(arch, nl, clocks, &routing.lut_pins);
            let a =
                graph.analyse(&crate::timing::routed_delays(arch, nl, placement, routing, &graph));
            a.period
                .iter()
                .zip(&graph.clocks)
                .map(|(p, c)| p - c.target_ps.unwrap_or(0.0))
                .fold(f64::NEG_INFINITY, f64::max)
        });
        (run, log, score)
    });
    let best = (0..runs.len())
        .filter(|&i| runs[i].2.is_some())
        .min_by(|&a, &b| runs[a].2.unwrap().total_cmp(&runs[b].2.unwrap()));
    let scores: Vec<String> = runs
        .iter()
        .zip(&seeds)
        .map(|((_, _, score), seed)| match score {
            Some(s) => format!("seed {seed}: {:.2} ns", s / 1000.0),
            None => format!("seed {seed}: failed"),
        })
        .collect();
    let (run, kept, _) = runs.into_iter().nth(best.unwrap_or(0)).expect("a run per seed");
    log.extend(kept);
    if let Some(best) = best {
        log.push(format!(
            "kept seed {} of {tries} placement seeds (worst period beyond its target: {})",
            seeds[best],
            scores.join(", ")
        ));
    }
    run
}

/// Placement and routing from one seed, placing again if routing fails.
#[allow(clippy::too_many_arguments)]
fn place_and_route(
    arch: &Arch,
    nl: &Netlist,
    packed: &crate::pack::Packed,
    fixed: &crate::place::Placement,
    clocks: &crate::timing::ClockConstraints,
    opts: &FlowOptions,
    seed: u64,
    log: &mut Vec<String>,
) -> Result<(crate::place::Placement, crate::route::Routing), String> {
    // Each retry also leaves tiles room on their local tracks (contested
    // local tracks in full tiles are what fails to route), anneals twice as
    // long (on a nearly full device, a short anneal leaves more wire than
    // the routing has: Euler 58 at effort 0.3 does not route with its seed
    // again, nor does it at effort 0.6 with Euler 58 in the playground)
    // and starts from another seed, not to meet the same conflict again.
    let attempts = [
        (opts.timing_weight, 32, opts.place_effort),
        (opts.timing_weight / 2.0, 30, 2.0 * opts.place_effort),
        (0.0, 28, 4.0 * opts.place_effort),
    ];
    for (attempt, &(w, max_locals, effort)) in attempts.iter().enumerate() {
        let t = Instant::now();
        let popts = PlaceOptions {
            seed: seed.wrapping_add(attempt as u64),
            effort,
            timing_weight: w,
            max_locals,
            clocks: clocks.clone(),
        };
        let mut placement = fixed.clone();
        let again = if attempt > 0 {
            format!("again ({} of {})", attempt + 1, attempts.len())
        } else {
            String::new()
        };
        progress::step("place", &again);
        crate::place::place_with_fixed(arch, nl, packed, &mut placement, &popts, log)?;
        log.push(format!("placed in {:.2}s", t.elapsed().as_secs_f64()));
        if std::env::var("SYNTH_DEBUG").is_ok() {
            for l in log.iter().rev().take(2).rev() {
                eprintln!("debug: {l}");
            }
        }
        let t = Instant::now();
        progress::step("route", &again);
        match crate::route::route(arch, nl, &placement, clocks, opts.route_timing_rounds, log) {
            Ok(routing) => {
                log.push(format!("routed in {:.2}s", t.elapsed().as_secs_f64()));
                return Ok((placement, routing));
            }
            Err(e) if attempt + 1 < attempts.len() => {
                let (w, l, _) = attempts[attempt + 1];
                log.push(format!(
                    "{e}; placing again with timing weight {w} and at most {l} local inputs per tile"
                ));
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("the last attempt returns or errors")
}
