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

/// `synth unpack-ecp5 <in.bit> -o <out.config> [--db <dir>]`.
pub fn unpack(argv: &[String]) -> Result<(), String> {
    let mut a = Args::parse(argv, &["--db", "-o"], &[])?;
    let input = a.positional(0, "bitstream")?;
    let out = a.required("-o")?;
    let db = database(&mut a)?;
    let bytes = std::fs::read(&input).map_err(|e| format!("{input}: {e}"))?;
    let cc = arch_ecp5::unpack_bitstream(&db, &bytes).map_err(|e| format!("{input}: {e}"))?;
    write(&out, cc.to_text().as_bytes())
}

/// Places, routes and writes the bitstream of a synthesised ECP5 design:
/// `--device LFE5U-25F [--package CABGA381] [--speed 6] [--lpf <file>]
/// [-o <out.bit>] [--textcfg <out.config>] [--compress] [--db <dir>]`.
pub fn finish(nl: synth_netlist::Netlist, a: &mut Args) -> Result<(), String> {
    let cons = match a.value("--lpf") {
        Some(p) => {
            arch_ecp5::lpf::Constraints::parse(&read(&p)?).map_err(|e| format!("{p}: {e}"))?
        }
        None => Default::default(),
    };
    for w in &cons.warnings {
        info!("warning: {w}");
    }
    let opts = arch_ecp5::flow::FlowOptions {
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
    };
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
