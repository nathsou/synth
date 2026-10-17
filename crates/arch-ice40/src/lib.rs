//! Lattice iCE40 support.
//!
//! - [`device`]: die geometry, tile kinds, configuration memory mapping.
//! - [`bitstream`]: the configuration image and its binary encoding.
//! - [`asc`]: the IceStorm `.asc` text format.

pub mod asc;
pub mod bitstream;
pub mod device;

pub use bitstream::{Config, FreqRange};
pub use device::{Die, TileKind};
