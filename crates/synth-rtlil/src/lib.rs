//! Reader for RTLIL, the text intermediate language of Yosys (also emitted by
//! Amaranth).
//!
//! - [`parse`] turns text into a syntax tree ([`ast::Design`]).
//! - [`to_netlist`] converts a design whose top module contains only
//!   primitive cells (for example after `synth_ice40`) into a [`Netlist`].

pub mod ast;
mod netlist;
mod parser;

pub use netlist::{PrimitiveLibrary, to_netlist};
pub use parser::{ParseError, parse};
pub use synth_netlist::Netlist;
