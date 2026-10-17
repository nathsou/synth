//! The iCE40 implementation flow: pack, place, route, generate bitstream.

use std::time::Instant;

use synth_netlist::Netlist;

use crate::arch::{Arch, Part};
use crate::bitstream::Config;
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
}

/// Result of a successful run.
#[derive(Debug)]
pub struct FlowResult {
    pub config: Config,
    pub log: Vec<String>,
}

/// Packs, places and routes a netlist of iCE40 primitives. On failure the
/// error message starts with the log so far (one `info:` line per entry).
pub fn implement(
    nl: Netlist,
    pins: &PinConstraints,
    opts: &FlowOptions,
) -> Result<FlowResult, String> {
    let mut log = Vec::new();
    match run(nl, pins, opts, &mut log) {
        Ok(config) => Ok(FlowResult { config, log }),
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
) -> Result<Config, String> {
    let t0 = Instant::now();
    let arch = Arch::new(opts.part, &opts.package)?;
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
    let weights = [opts.timing_weight, opts.timing_weight / 2.0, 0.0];
    let mut result = None;
    for (attempt, &w) in weights.iter().enumerate() {
        if attempt > 0 && w == weights[attempt - 1] {
            continue;
        }
        let t = Instant::now();
        let popts = PlaceOptions {
            seed: opts.seed,
            effort: opts.place_effort,
            timing_weight: w,
            clocks: opts.clocks.clone(),
            ..PlaceOptions::default()
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
        match crate::route::route(&arch, &nl, &placement, &opts.clocks, log) {
            Ok(routing) => {
                log.push(format!("routed in {:.2}s", t.elapsed().as_secs_f64()));
                result = Some((placement, routing));
                break;
            }
            Err(e) if attempt + 1 < weights.len() => {
                log.push(format!("{e}; placing again with timing weight {}", weights[attempt + 1]));
            }
            Err(e) => return Err(e),
        }
    }
    let (placement, routing) = result.expect("the last attempt returns or errors");
    if !routing.lut_pins.is_empty() {
        log.push(format!("{} LUTs with permuted inputs", routing.lut_pins.len()));
    }
    routing.apply_lut_permutation(&mut nl);

    let t = Instant::now();
    let timing = crate::timing::analyse(&arch, &nl, &placement, &routing, &opts.clocks);
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

    let config = crate::bitgen::generate(&arch, &nl, &placement, &routing)?;
    log.push(format!("total {:.2}s", t0.elapsed().as_secs_f64()));
    Ok(config)
}
