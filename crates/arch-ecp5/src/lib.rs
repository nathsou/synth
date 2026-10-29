//! Lattice ECP5 support: the Project Trellis database of the family, its
//! routing graph, bitstream assembly and disassembly, and the native flow.

pub mod arch;
pub mod base;
pub mod bitgen;
pub mod bits;
pub mod bitstream;
pub mod bram;
pub mod cells;
pub mod clocks;
pub mod config;
pub mod db;
pub mod decode;
pub mod dsp;
pub mod flow;
pub mod graph;
pub mod lpf;
pub mod pack;
pub mod place;
pub mod pll;
pub mod route;
pub mod sim;
pub mod speed;
pub mod synth;
pub mod timing;

use crate::config::ChipConfig;
use crate::db::Database;

/// The `.bit` file of a textual configuration (as `ecppack`), compressed
/// if asked or if the configuration's `COMPRESS_CONFIG` sysconfig is on.
pub fn pack_config(db: &Database, cc: &ChipConfig, compress: bool) -> Result<Vec<u8>, String> {
    let dev = db.device(&cc.device)?;
    let cram = dev.to_cram(db, cc)?;
    let compress = compress || cc.sysconfig.get("COMPRESS_CONFIG").is_some_and(|v| v == "ON");
    let opts = bitstream::Options { comments: cc.comments.clone(), compress, ..Default::default() };
    Ok(bitstream::write(&dev.info, &cram, &cc.bram_init, &opts))
}

/// The contents of a `.bit` file, for any device of the database.
pub fn read_bitstream(db: &Database, bytes: &[u8]) -> Result<bitstream::Parsed, String> {
    let infos: Vec<_> = db.device_names().iter().filter_map(|n| db.chip_info(n).ok()).collect();
    bitstream::read(bytes, &|id| infos.iter().find(|i| i.idcode == id).cloned())
}

/// The textual configuration of a `.bit` file (as `ecpunpack`).
pub fn unpack_bitstream(db: &Database, bytes: &[u8]) -> Result<ChipConfig, String> {
    let p = read_bitstream(db, bytes)?;
    let dev = db.device(&p.info.name)?;
    let mut cc = dev.from_cram(db, &p.cram)?;
    cc.comments = p.comments;
    cc.bram_init = p.bram;
    Ok(cc)
}
