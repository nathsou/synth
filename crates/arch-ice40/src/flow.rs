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
}

/// Result of a successful run.
#[derive(Debug)]
pub struct FlowResult {
    pub config: Config,
    pub log: Vec<String>,
}

/// Packs, places and routes a netlist of iCE40 primitives.
pub fn implement(
    mut nl: Netlist,
    pins: &PinConstraints,
    opts: &FlowOptions,
) -> Result<FlowResult, String> {
    let mut log = Vec::new();
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

    let t = Instant::now();
    let popts =
        PlaceOptions { seed: opts.seed, effort: opts.place_effort, ..PlaceOptions::default() };
    let (placement, plog) = crate::place::place(&arch, &mut nl, &packed, pins, &popts)?;
    log.extend(plog);
    log.push(format!("placed in {:.2}s", t.elapsed().as_secs_f64()));

    let t = Instant::now();
    let routing = crate::route::route(&arch, &nl, &placement, &mut log)?;
    log.push(format!("routed in {:.2}s", t.elapsed().as_secs_f64()));

    let config = crate::bitgen::generate(&arch, &nl, &placement, &routing)?;
    log.push(format!("total {:.2}s", t0.elapsed().as_secs_f64()));
    Ok(FlowResult { config, log })
}
