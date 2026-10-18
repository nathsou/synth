//! The iCE40 implementation flow: pack, place, route, generate bitstream.

use std::time::Instant;

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
}

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

/// Packs, places and routes a netlist of iCE40 primitives. On failure the
/// error message starts with the log so far (one `info:` line per entry).
pub fn implement(
    nl: Netlist,
    pins: &PinConstraints,
    opts: &FlowOptions,
) -> Result<FlowResult, String> {
    implement_with_checkpoint(nl, pins, opts, None, false)
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
    match run(nl, pins, opts, &mut log, previous, save) {
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
) -> Result<(Config, bool, Option<Checkpoint>), String> {
    let t0 = Instant::now();
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
    crate::place::place_ios(&arch, &nl, pins, &mut fixed, log)?;
    crate::place::assign_globals(&arch, &mut nl, &mut fixed, log)?;
    // Each retry also leaves tiles room on their local tracks (contested
    // local tracks in full tiles are what fails to route), anneals longer
    // and starts from another seed, not to meet the same conflict again.
    let attempts = [
        (opts.timing_weight, 32, opts.place_effort),
        (opts.timing_weight / 2.0, 30, opts.place_effort),
        (0.0, 28, 2.0 * opts.place_effort),
    ];
    let mut result = None;
    let mut incremental = false;
    if let Some(previous) = previous {
        let t = Instant::now();
        let mut placement = fixed.clone();
        let popts = PlaceOptions {
            seed: opts.seed,
            effort: opts.place_effort,
            timing_weight: opts.timing_weight,
            max_locals: 32,
            clocks: opts.clocks.clone(),
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
            crate::route::route_incremental(
                &arch,
                &nl,
                &placement,
                &opts.clocks,
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
    for (attempt, &(w, max_locals, effort)) in attempts.iter().enumerate() {
        if result.is_some() {
            break;
        }
        let t = Instant::now();
        let popts = PlaceOptions {
            seed: opts.seed.wrapping_add(attempt as u64),
            effort,
            timing_weight: w,
            max_locals,
            clocks: opts.clocks.clone(),
        };
        let mut placement = fixed.clone();
        crate::place::place_with_fixed(&arch, &nl, &packed, &mut placement, &popts, log)?;
        log.push(format!("placed in {:.2}s", t.elapsed().as_secs_f64()));
        if std::env::var("SYNTH_DEBUG").is_ok() {
            for l in log.iter().rev().take(2).rev() {
                eprintln!("debug: {l}");
            }
        }
        let t = Instant::now();
        match crate::route::route(
            &arch,
            &nl,
            &placement,
            &opts.clocks,
            opts.route_timing_rounds,
            log,
        ) {
            Ok(routing) => {
                log.push(format!("routed in {:.2}s", t.elapsed().as_secs_f64()));
                result = Some((placement, routing));
                break;
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
    let (placement, routing) = result.expect("the last attempt returns or errors");
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
    let timing = crate::timing::analyse(&arch, &nl, &placement, &routing, &opts.clocks);
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
        return run(fresh_input.unwrap(), pins, opts, log, None, save);
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
            return run(fresh_input.unwrap(), pins, opts, log, None, save);
        }
    }
    if let Some(c) = checkpoint.as_mut() {
        c.set_periods(&timing);
    }

    let config = crate::bitgen::generate(&arch, &nl, &placement, &routing)?;
    log.push(format!("total {:.2}s", t0.elapsed().as_secs_f64()));
    Ok((config, met, checkpoint))
}
