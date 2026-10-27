//! ECP5 command adapters.

use std::path::Path;

use crate::args::Args;
use crate::{read, write};

/// The Project Trellis database: `--db`, else its usual locations.
fn database(a: &mut Args) -> Result<arch_ecp5::db::Database, String> {
    let db = a.value("--db");
    let root = arch_ecp5::db::locate(db.as_deref().map(Path::new))?;
    arch_ecp5::db::Database::open(&root)
}

/// `synth pack-ecp5 <in.config> -o <out.bit> [--compress] [--db <dir>]`.
pub fn pack(argv: &[String]) -> Result<(), String> {
    let mut a = Args::parse(argv, &["--db", "-o"], &["--compress"])?;
    let input = a.positional(0, "textual configuration")?;
    let out = a.required("-o")?;
    let db = database(&mut a)?;
    let cc = arch_ecp5::config::ChipConfig::parse(&read(&input)?)
        .map_err(|e| format!("{input}: {e}"))?;
    write(&out, &arch_ecp5::pack_config(&db, &cc, a.flag("--compress"))?)
}

/// `synth unpack-ecp5 <in.bit> [-o <out.config>] [--timing [--speed 6]
/// [--freq <MHz>]] [--db <dir>]`: the textual configuration, and with
/// `--timing` the timing of what the bitstream implements.
pub fn unpack(argv: &[String]) -> Result<(), String> {
    let mut a = Args::parse(argv, &["--db", "-o", "--speed", "--freq"], &["--timing"])?;
    let input = a.positional(0, "bitstream")?;
    let out = a.value("-o");
    let timing = a.flag("--timing");
    if out.is_none() && !timing {
        return Err("unpack-ecp5 needs -o or --timing".into());
    }
    let speed = a.value("--speed").unwrap_or_else(|| "6".into());
    let freq: Option<f64> =
        a.value("--freq").map(|s| s.parse().map_err(|_| "bad --freq")).transpose()?;
    let root = arch_ecp5::db::locate(a.value("--db").as_deref().map(Path::new))?;
    a.finish()?;
    let db = arch_ecp5::db::Database::open(&root)?;
    let bytes = std::fs::read(&input).map_err(|e| format!("{input}: {e}"))?;
    if let Some(out) = out {
        let cc = arch_ecp5::unpack_bitstream(&db, &bytes).map_err(|e| format!("{input}: {e}"))?;
        write(&out, cc.to_text().as_bytes())?;
    }
    if timing {
        let bits = arch_ecp5::read_bitstream(&db, &bytes).map_err(|e| format!("{input}: {e}"))?;
        let arch = arch_ecp5::arch::Arch::load(&root, &bits.info.name, "CABGA381", &speed)?;
        // Every pad in use, named after its pin.
        let pins: Vec<(String, (usize, usize, char))> = arch.dev.packages[&arch.package]
            .iter()
            .map(|(name, p)| (name.clone(), (p.col, p.row, p.pio)))
            .collect();
        let d = arch_ecp5::decode::decode(&arch, &bits.cram, &bits.bram, &pins)?;
        for w in &d.warnings {
            info!("warning: {w}");
        }
        let clocks = arch_ecp5::timing::ClockConstraints { default_mhz: freq, nets: Vec::new() };
        let report =
            arch_ecp5::timing::analyse(&arch, &d.netlist, &d.placement, &d.routing, &clocks);
        for c in &report.clocks {
            println!("clock {}: {:.2} MHz ({:.2} ns)", c.clock, c.fmax_mhz, c.period_ns);
            for p in &c.path {
                println!("  {p}");
            }
        }
    }
    Ok(())
}

/// The pin constraints of `--lpf`, if given.
fn constraints(a: &mut Args) -> Result<arch_ecp5::lpf::Constraints, String> {
    let cons = match a.value("--lpf") {
        Some(p) => {
            arch_ecp5::lpf::Constraints::parse(&read(&p)?).map_err(|e| format!("{p}: {e}"))?
        }
        None => Default::default(),
    };
    for w in &cons.warnings {
        info!("warning: {w}");
    }
    Ok(cons)
}

/// `--device`, `--package`, `--speed`, `--db`, `--seed`, `--effort`, `--freq`,
/// `--timing-weight` and `--tries`.
fn flow_options(a: &mut Args) -> Result<arch_ecp5::flow::FlowOptions, String> {
    Ok(arch_ecp5::flow::FlowOptions {
        device: a.required("--device")?,
        package: a.value("--package").unwrap_or_else(|| "CABGA381".into()),
        speed: a.value("--speed").unwrap_or_else(|| "6".into()),
        db: a.value("--db").map(Into::into),
        seed: a
            .value("--seed")
            .map(|s| s.parse().map_err(|_| "bad --seed"))
            .transpose()?
            .unwrap_or(1),
        effort: a
            .value("--effort")
            .map(|s| s.parse().map_err(|_| "bad --effort"))
            .transpose()?
            .unwrap_or(1.0),
        freq: a.value("--freq").map(|s| s.parse().map_err(|_| "bad --freq")).transpose()?,
        timing_weight: a
            .value("--timing-weight")
            .map(|s| s.parse().map_err(|_| "bad --timing-weight"))
            .transpose()?
            .unwrap_or(arch_ecp5::place::TIMING_WEIGHT),
        tries: a
            .value("--tries")
            .map(|s| s.parse().map_err(|_| "bad --tries"))
            .transpose()?
            .unwrap_or(arch_ecp5::flow::TRIES),
    })
}

/// The design after ECP5 synthesis, as RTL built from the primitives'
/// simulation models.
pub fn synthesized(m: synth_rtl::Module) -> Result<synth_rtl::Module, String> {
    let (nl, _) = arch_ecp5::synth::synthesize(m, &arch_ecp5::synth::SynthOptions::default())?;
    synth_sim::netlist::from_netlist(&nl, &mut arch_ecp5::sim::add_cell)
}

/// What the ECP5 bitstream of the design (built, or read from
/// `--bitstream` with the pads of `--lpf`) implements, decoded back to
/// cells, as RTL.
pub fn decoded(a: &mut Args, m: synth_rtl::Module) -> Result<synth_rtl::Module, String> {
    let cons = constraints(a)?;
    let opts = flow_options(a)?;
    let (arch, bytes, pins) = match a.value("--bitstream") {
        Some(path) => {
            let bytes = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
            let arch = arch_ecp5::flow::arch(&opts)?;
            let mut pins = Vec::new();
            for (port, site) in &cons.sites {
                let p = arch.pin(site).ok_or_else(|| format!("{port}: no pin {site}"))?;
                pins.push((port.clone(), (p.col, p.row, p.pio)));
            }
            if pins.is_empty() {
                return Err("--bitstream needs the pads of the ports (--lpf)".into());
            }
            (arch, bytes, pins)
        }
        None => {
            let (nl, _) =
                arch_ecp5::synth::synthesize(m, &arch_ecp5::synth::SynthOptions::default())?;
            let mut log = Vec::new();
            let imp = arch_ecp5::flow::implement(
                arch_ecp5::flow::arch(&opts)?,
                nl,
                &cons,
                &opts,
                &mut log,
            )?;
            let bytes = arch_ecp5::flow::bitstream(&imp, false)?;
            let pins = imp
                .pads
                .iter()
                .map(|(&c, &at)| (imp.netlist.str(imp.netlist.cells[c].name).to_string(), at))
                .collect();
            (imp.arch, bytes, pins)
        }
    };
    let bits = arch_ecp5::read_bitstream(&arch.db, &bytes)?;
    if bits.info.name != arch.dev.info.name {
        return Err(format!("the bitstream is for {}, not {}", bits.info.name, arch.dev.info.name));
    }
    let d = arch_ecp5::decode::decode(&arch, &bits.cram, &bits.bram, &pins)?;
    for w in &d.warnings {
        eprintln!("warning: {w}");
    }
    synth_sim::netlist::from_netlist(&d.netlist, &mut arch_ecp5::sim::add_cell)
}

/// Places, routes and writes the bitstream of a synthesised ECP5 design:
/// `--device LFE5U-25F [--package CABGA381] [--speed 6] [--lpf <file>]
/// [-o <out.bit>] [--textcfg <out.config>] [--compress] [--db <dir>]`.
pub fn finish(nl: synth_netlist::Netlist, a: &mut Args) -> Result<(), String> {
    let cons = constraints(a)?;
    let opts = flow_options(a)?;
    let t = std::time::Instant::now();
    synth_core::progress::step("load device", "");
    let arch = arch_ecp5::flow::arch(&opts)?;
    info!(
        "loaded {} ({} wires, {} switches) in {:.2}s",
        opts.device,
        arch.graph.num_nodes(),
        arch.graph.num_edges(),
        t.elapsed().as_secs_f64()
    );
    let mut log = Vec::new();
    let res = arch_ecp5::flow::implement(arch, nl, &cons, &opts, &mut log);
    for l in &log {
        info!("{l}");
    }
    let imp = res?;
    if let Some(out) = a.value("--textcfg") {
        write(&out, imp.config.to_text().as_bytes())?;
    }
    if let Some(out) = a.value("-o") {
        write(&out, &arch_ecp5::flow::bitstream(&imp, a.flag("--compress"))?)?;
    }
    Ok(())
}
