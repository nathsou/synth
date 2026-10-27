//! The flow from RTL to a bitstream: synthesis, packing, placement,
//! routing and configuration.

use std::path::PathBuf;

use synth_core::FxHashMap;
use synth_core::time::Instant;
use synth_netlist::{CellId, INOUT_PORT, INPUT_PORT, Netlist, OUTPUT_PORT};

use crate::arch::Arch;
use crate::config::ChipConfig;
use crate::lpf::Constraints;
use crate::place::{PlaceOptions, Placement};
use crate::route::{PadSites, Routing};

#[derive(Clone, Debug)]
pub struct FlowOptions {
    /// `LFE5U-25F`, `LFE5UM5G-85F`, ...
    pub device: String,
    /// `CABGA381`, ...
    pub package: String,
    /// Speed grade: `6`, `7`, `8` (`8_5G` for the UM5G).
    pub speed: String,
    /// The Project Trellis database (else its usual locations).
    pub db: Option<PathBuf>,
    pub seed: u64,
    pub effort: f64,
    /// Target frequency (MHz) of clocks the constraints leave out.
    pub freq: Option<f64>,
}

impl Default for FlowOptions {
    fn default() -> Self {
        FlowOptions {
            device: "LFE5U-25F".into(),
            package: "CABGA381".into(),
            speed: "6".into(),
            db: None,
            seed: 1,
            effort: 1.0,
            freq: None,
        }
    }
}

/// A placed and routed design.
#[derive(Debug)]
pub struct Implementation {
    pub arch: Arch,
    pub netlist: Netlist,
    pub placement: Placement,
    pub pads: PadSites,
    pub routing: Routing,
    pub config: ChipConfig,
    pub timing: crate::timing::TimingReport,
}

/// Loads the device of `opts`.
pub fn arch(opts: &FlowOptions) -> Result<Arch, String> {
    let root = crate::db::locate(opts.db.as_deref())?;
    Arch::load(&root, &opts.device, &opts.package, &opts.speed)
}

/// The pads of the ports: as constrained, the others on free pins (with a
/// warning), in pin name order.
fn assign_pads(
    arch: &Arch,
    nl: &Netlist,
    cons: &Constraints,
    log: &mut Vec<String>,
) -> Result<PadSites, String> {
    let mut pads = PadSites::default();
    let mut taken: FxHashMap<(usize, usize, char), String> = FxHashMap::default();
    let ports: Vec<CellId> = nl
        .live_cells()
        .filter(|&c| matches!(nl.cell_kind(c), INPUT_PORT | OUTPUT_PORT | INOUT_PORT))
        .collect();
    let mut free: Vec<(&String, &crate::db::Pin)> =
        arch.dev.packages.get(&arch.package).map(|m| m.iter().collect()).unwrap_or_default();
    free.sort_by(|a, b| a.0.cmp(b.0));
    let mut unconstrained = Vec::new();
    for &c in &ports {
        let name = nl.str(nl.cells[c].name).to_string();
        match cons.sites.get(&name) {
            Some(site) => {
                let pin = arch
                    .pin(site)
                    .ok_or_else(|| format!("{name}: no pin {site} in {}", arch.package))?;
                let at = (pin.col, pin.row, pin.pio);
                if let Some(other) = taken.insert(at, name.clone()) {
                    return Err(format!("{name} and {other} are both on pin {site}"));
                }
                pads.insert(c, at);
            }
            None => unconstrained.push((c, name)),
        }
    }
    let mut next = free.into_iter();
    for (c, name) in unconstrained {
        let (pin, at) = loop {
            let (pin, p) = next.next().ok_or("more ports than free pins")?;
            let at = (p.col, p.row, p.pio);
            if !taken.contains_key(&at) {
                break (pin, at);
            }
        };
        log.push(format!("warning: port {name} has no LOCATE; placed on pin {pin}"));
        taken.insert(at, name);
        pads.insert(c, at);
    }
    Ok(pads)
}

/// The clock targets: the constraints' frequencies, and `opts.freq` for
/// the others.
pub fn clock_constraints(
    cons: &Constraints,
    opts: &FlowOptions,
) -> crate::timing::ClockConstraints {
    crate::timing::ClockConstraints {
        default_mhz: opts.freq,
        nets: cons.frequencies.iter().map(|(n, &f)| (n.clone(), f)).collect(),
    }
}

/// Places, routes and configures a synthesised netlist.
pub fn implement(
    arch: Arch,
    mut nl: Netlist,
    cons: &Constraints,
    opts: &FlowOptions,
    log: &mut Vec<String>,
) -> Result<Implementation, String> {
    let t = Instant::now();
    let consts = crate::pack::legalize_constants(&mut nl)?;
    if consts > 0 {
        log.push(format!("{consts} LUTs driving constants"));
    }
    let pads = assign_pads(&arch, &nl, cons, log)?;
    let packed = crate::pack::pack(&nl)?;
    log.push(format!("packed in {:.2}s", t.elapsed().as_secs_f64()));
    let t = Instant::now();
    let fixed: FxHashMap<CellId, (usize, usize)> =
        pads.iter().map(|(&c, &(x, y, _))| (c, (x, y))).collect();
    let popts = PlaceOptions { seed: opts.seed, effort: opts.effort };
    let placement = crate::place::place(&nl, &packed, &arch.grid, &fixed, &popts, log)?;
    log.push(format!("placed in {:.2}s", t.elapsed().as_secs_f64()));
    let t = Instant::now();
    let routing = crate::route::route(&arch, &nl, &placement, &pads, log)?;
    log.push(format!("routed in {:.2}s", t.elapsed().as_secs_f64()));
    let t = Instant::now();
    let clocks = clock_constraints(cons, opts);
    let timing = crate::timing::analyse(&arch, &nl, &placement, &routing, &clocks);
    for c in &timing.clocks {
        let target = c.target_mhz.map(|t| format!(" (target {t:.2} MHz)")).unwrap_or_default();
        log.push(format!("clock {}: {:.2} MHz{target}", c.clock, c.fmax_mhz));
        if std::env::var_os("SYNTH_DEBUG").is_some() {
            for p in &c.path {
                log.push(format!("  {p}"));
            }
        }
    }
    log.push(format!("timing analysed in {:.2}s", t.elapsed().as_secs_f64()));
    let config = crate::bitgen::config(&arch, &nl, &placement, &pads, &routing, cons)?;
    Ok(Implementation { arch, netlist: nl, placement, pads, routing, config, timing })
}

/// The bitstream of a configuration.
pub fn bitstream(imp: &Implementation, compress: bool) -> Result<Vec<u8>, String> {
    crate::pack_config(&imp.arch.db, &imp.config, compress)
}
