//! Converts upstream device databases into the binary databases embedded in
//! `synth`. Run by maintainers when upstream data changes; end users never
//! need it.
//!
//! ```text
//! synth-dbgen ice40 <icebox chipdb.txt> <icestorm timings.txt> <out.bin>
//! ```
//!
//! The iCE40 inputs come from Project IceStorm: `icebox_chipdb -1|-8|-3`
//! output and `icefuzz/timings_*.txt`. See `crates/arch-ice40/db/README.md`.

use std::process::ExitCode;

fn run(args: &[String]) -> Result<(), String> {
    match args {
        [family, chipdb, timing, out] if family == "ice40" => {
            let read = |p: &str| std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"));
            let db = arch_ice40::chipdb::import(&read(chipdb)?, &read(timing)?)
                .map_err(|e| e.to_string())?;
            let bytes = db.encode();
            std::fs::write(out, &bytes).map_err(|e| format!("{out}: {e}"))?;
            let pips: usize = (0..db.grid_height())
                .flat_map(|y| (0..db.grid_width()).map(move |x| (x, y)))
                .filter_map(|(x, y)| db.template_at(x, y))
                .map(|t| t.pips.len())
                .sum();
            eprintln!(
                "{}: {} nodes, {} templates, ~{} pips, {} bytes",
                db.die,
                db.nodes.len(),
                db.templates.len(),
                pips,
                bytes.len()
            );
            Ok(())
        }
        _ => Err("usage: synth-dbgen ice40 <chipdb.txt> <timings.txt> <out.bin>".into()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
