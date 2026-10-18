//! Native Artix-7 synthesis and experimental physical implementation.
pub mod bitstream;
mod bram;
pub mod device;
pub mod fasm;
mod json;
pub mod synth;
pub mod xdc;

pub mod flow;
pub mod pack;
pub mod place;
pub mod routing;

mod physical_bram;

pub mod timing;
