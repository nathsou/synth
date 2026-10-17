//! Technology-independent logic synthesis.
//!
//! Combinational logic is represented as an and-inverter graph ([`Aig`]):
//! two-input AND nodes and complemented edges, with structural hashing.
//! Sequential elements and hard blocks (carry chains, RAMs, black boxes)
//! stay outside the graph; their outputs are graph inputs and their inputs
//! are graph outputs. The graph is optimised and then covered with K-input
//! lookup tables ([`lutmap`]).

pub mod aig;
pub mod carry;
pub mod ff;
pub mod fraig;
pub mod lower;
pub mod lutmap;
pub mod net;
pub mod opt;
pub mod sat;

pub use aig::{Aig, Lit};
pub use net::LogicNet;
