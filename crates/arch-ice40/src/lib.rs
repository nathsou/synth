//! Lattice iCE40 support.
//!
//! - [`device`]: die geometry, tile kinds, configuration memory mapping.
//! - [`bitstream`]: the configuration image and its binary encoding.
//! - [`asc`]: the IceStorm `.asc` text format.
//! - [`chipdb`]: the device database (routing graph, config bits, timing).

pub mod arch;
pub mod asc;
pub mod bitgen;
pub mod bitstream;
pub mod cells;
pub mod checkpoint;
pub mod chipdb;
pub mod device;
pub mod flow;
pub mod pack;
pub mod pcf;
pub mod place;
pub mod route;
pub mod synth;
pub mod timing;

pub use bitstream::{Config, FreqRange};
pub use device::{Die, TileKind};
