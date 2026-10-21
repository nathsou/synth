//! Native Artix-7 synthesis and experimental physical implementation.
pub mod bitstream;
mod bram;
mod detail;
pub mod device;
mod dsp;
pub mod fasm;
mod json;
mod lutram;
mod physical_dsp;
mod srl;
pub mod sta;
pub mod synth;
pub mod xdc;

pub mod flow;
pub mod pack;
pub mod place;
pub mod routing;

mod physical_bram;

pub mod timing;
