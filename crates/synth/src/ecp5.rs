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
