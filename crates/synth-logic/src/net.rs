//! A design as an AIG plus the elements outside it.

use synth_core::Logic;
use synth_rtl::{Attr, PortDir};

use crate::aig::Aig;

/// Index of an AIG output (a value consumed outside the graph).
pub type Sink = usize;
/// Index of an AIG input (a value produced outside the graph).
pub type Source = u32;

/// One bit of a port or instance connection.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Bit {
    /// Produced outside the graph (a top-level input, an instance output).
    Src(Source),
    /// Consumed outside the graph (a top-level output, an instance input).
    Snk(Sink),
    /// A bidirectional pad connection, passed through untouched.
    Pad(u32),
}

#[derive(Clone, Debug)]
pub struct Port {
    pub name: String,
    pub dir: PortDir,
    pub bits: Vec<Bit>,
}

/// A single-bit flip-flop. Control inputs are active high; inversions are
/// part of the graph (a complemented clock sink means the opposite edge).
#[derive(Clone, Debug)]
pub struct Ff {
    pub name: String,
    pub clk: Sink,
    pub posedge: bool,
    pub d: Sink,
    pub en: Option<Sink>,
    /// Synchronous reset and the value it loads.
    pub srst: Option<(Sink, bool)>,
    pub en_over_srst: bool,
    /// Asynchronous reset and the value it loads.
    pub arst: Option<(Sink, bool)>,
    pub init: Logic,
    pub q: Source,
}

/// An adder on a hard carry chain: `sum = a + b + cin`.
#[derive(Clone, Debug)]
pub struct Carry {
    pub a: Vec<Sink>,
    pub b: Vec<Sink>,
    pub cin: Sink,
    pub sum: Vec<Source>,
    pub cout: Option<Source>,
}

/// A black-box or primitive instance.
#[derive(Clone, Debug)]
pub struct Instance {
    pub name: String,
    pub module: String,
    pub params: Vec<(String, Attr)>,
    pub conns: Vec<(String, PortDir, Vec<Bit>)>,
}

/// A synthesised design: combinational logic in `aig`, everything else
/// connected to its inputs and outputs.
#[derive(Clone, Debug, Default)]
pub struct LogicNet {
    pub name: String,
    pub aig: Aig,
    pub ports: Vec<Port>,
    pub ffs: Vec<Ff>,
    pub carries: Vec<Carry>,
    pub instances: Vec<Instance>,
    /// Number of bidirectional pad connections.
    pub pads: u32,
}

impl LogicNet {
    /// Removes logic not reachable from any sink (sources are kept).
    pub fn cleanup(&mut self) {
        self.aig = self.aig.cleanup();
    }
}
