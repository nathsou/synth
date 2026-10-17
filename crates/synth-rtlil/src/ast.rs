//! Syntax tree of an RTLIL file.

use synth_netlist::{Bit, Param};

/// One chunk of a signal: a slice of a wire or constant bits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SigChunk {
    /// `width` bits of wire `wire` starting at bit `offset` (0-based storage
    /// index, already adjusted for the wire's declared offset and direction).
    Wire { wire: String, offset: usize, width: usize },
    /// Constant bits, least significant first.
    Const(Vec<Bit>),
}

impl SigChunk {
    pub fn width(&self) -> usize {
        match self {
            SigChunk::Wire { width, .. } => *width,
            SigChunk::Const(b) => b.len(),
        }
    }
}

/// A signal: a concatenation of chunks, least significant chunk first.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SigSpec {
    pub chunks: Vec<SigChunk>,
}

impl SigSpec {
    pub fn width(&self) -> usize {
        self.chunks.iter().map(SigChunk::width).sum()
    }
}

/// Port direction of a module wire.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PortDir {
    Input,
    Output,
    Inout,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wire {
    pub name: String,
    pub width: usize,
    pub offset: i64,
    pub upto: bool,
    pub signed: bool,
    /// Port direction and position, for module ports.
    pub port: Option<(PortDir, u32)>,
    pub attrs: Vec<(String, Param)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Memory {
    pub name: String,
    pub width: usize,
    pub size: usize,
    pub offset: i64,
    pub attrs: Vec<(String, Param)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub kind: String,
    pub name: String,
    pub params: Vec<(String, Param)>,
    pub connections: Vec<(String, SigSpec)>,
    pub attrs: Vec<(String, Param)>,
}

/// A `case` of a `switch`; an empty `compare` list is the default case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Case {
    pub compare: Vec<SigSpec>,
    pub body: CaseBody,
    pub attrs: Vec<(String, Param)>,
}

/// Assignments followed by nested switches, in source order.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CaseBody {
    pub actions: Vec<(SigSpec, SigSpec)>,
    pub switches: Vec<Switch>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Switch {
    pub signal: SigSpec,
    pub cases: Vec<Case>,
    pub attrs: Vec<(String, Param)>,
}

/// The trigger of a `sync` rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncKind {
    Low(SigSpec),
    High(SigSpec),
    Posedge(SigSpec),
    Negedge(SigSpec),
    Edge(SigSpec),
    Always,
    Global,
    Init,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemWrite {
    pub memory: String,
    pub address: SigSpec,
    pub data: SigSpec,
    pub enable: SigSpec,
    pub priority: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sync {
    pub kind: SyncKind,
    pub updates: Vec<(SigSpec, SigSpec)>,
    pub memwrites: Vec<MemWrite>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    pub name: String,
    pub root: CaseBody,
    pub syncs: Vec<Sync>,
    pub attrs: Vec<(String, Param)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Module {
    pub name: String,
    pub attrs: Vec<(String, Param)>,
    /// Module parameters with optional default values.
    pub params: Vec<(String, Option<Param>)>,
    pub wires: Vec<Wire>,
    pub memories: Vec<Memory>,
    pub cells: Vec<Cell>,
    pub connections: Vec<(SigSpec, SigSpec)>,
    pub processes: Vec<Process>,
}

impl Module {
    pub fn attr(&self, name: &str) -> Option<&Param> {
        self.attrs.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    pub fn is_blackbox(&self) -> bool {
        ["blackbox", "whitebox"].iter().any(|a| self.attr(a).is_some_and(|v| v.as_u64() != Some(0)))
    }

    pub fn wire(&self, name: &str) -> Option<&Wire> {
        self.wires.iter().find(|w| w.name == name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Design {
    pub modules: Vec<Module>,
}

impl Design {
    pub fn module(&self, name: &str) -> Option<&Module> {
        self.modules.iter().find(|m| m.name == name)
    }

    /// The top module: the one marked `top`, else the only non-blackbox
    /// module that no other module instantiates.
    pub fn top(&self) -> Option<&Module> {
        if let Some(m) =
            self.modules.iter().find(|m| m.attr("top").is_some_and(|v| v.as_u64() != Some(0)))
        {
            return Some(m);
        }
        let candidates: Vec<&Module> = self
            .modules
            .iter()
            .filter(|m| !m.is_blackbox())
            .filter(|m| !self.modules.iter().any(|o| o.cells.iter().any(|c| c.kind == m.name)))
            .collect();
        (candidates.len() == 1).then(|| candidates[0])
    }
}
