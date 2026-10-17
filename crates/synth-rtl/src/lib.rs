//! The word-level RTL intermediate representation.
//!
//! A [`Module`] is a flat netlist of word-level [`Cell`]s (arithmetic,
//! logic, multiplexers, flip-flops, memories, black-box instances) over
//! [`Wire`]s. Signals ([`Sig`]) are vectors of bits, each either a wire bit
//! or a constant, least significant bit first.
//!
//! Cell operands are pre-extended by the frontend so that semantics stay
//! simple: arithmetic and bitwise cells have operands of the output width,
//! comparisons have equally wide operands and a 1-bit output.

pub mod build;
pub mod eval;
pub mod opt;
pub mod verilog;

use std::fmt;

use synth_core::{IndexVec, Logic, LogicVec, define_id};

define_id!(
    /// A wire of a [`Module`].
    pub struct WireId
);
define_id!(
    /// A cell of a [`Module`].
    pub struct CellId
);
define_id!(
    /// A memory of a [`Module`].
    pub struct MemId
);

/// One bit of a signal.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SigBit {
    Const(Logic),
    Wire(WireId, u32),
}

impl SigBit {
    pub const ZERO: SigBit = SigBit::Const(Logic::Zero);
    pub const ONE: SigBit = SigBit::Const(Logic::One);
    pub const X: SigBit = SigBit::Const(Logic::X);

    pub fn as_const(self) -> Option<Logic> {
        match self {
            SigBit::Const(l) => Some(l),
            SigBit::Wire(..) => None,
        }
    }

    pub fn is_const(self) -> bool {
        matches!(self, SigBit::Const(_))
    }
}

impl fmt::Debug for SigBit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SigBit::Const(l) => write!(f, "{}", l.to_char()),
            SigBit::Wire(w, b) => write!(f, "w{}[{b}]", w.index()),
        }
    }
}

/// A signal: bits, least significant first.
#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub struct Sig(pub Vec<SigBit>);

impl Sig {
    pub fn new() -> Sig {
        Sig(Vec::new())
    }

    pub fn from_const(v: &LogicVec) -> Sig {
        Sig(v.bits().map(SigBit::Const).collect())
    }

    pub fn zeros(width: usize) -> Sig {
        Sig(vec![SigBit::ZERO; width])
    }

    pub fn xs(width: usize) -> Sig {
        Sig(vec![SigBit::X; width])
    }

    pub fn from_u64(v: u64, width: usize) -> Sig {
        Sig::from_const(&LogicVec::from_u64(v, width))
    }

    pub fn bit(b: SigBit) -> Sig {
        Sig(vec![b])
    }

    /// All bits of a wire.
    pub fn wire(w: WireId, width: u32) -> Sig {
        Sig((0..width).map(|i| SigBit::Wire(w, i)).collect())
    }

    pub fn width(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The constant value, if every bit is constant.
    pub fn as_const(&self) -> Option<LogicVec> {
        let bits: Option<Vec<Logic>> = self.0.iter().map(|b| b.as_const()).collect();
        bits.map(|b| LogicVec::from_bits(&b))
    }

    pub fn is_const(&self) -> bool {
        self.0.iter().all(|b| b.is_const())
    }

    /// Bits `lo..lo + width` (must be in range).
    pub fn slice(&self, lo: usize, width: usize) -> Sig {
        Sig(self.0[lo..lo + width].to_vec())
    }

    /// Resizes, sign- or zero-extending.
    pub fn extend(&self, width: usize, signed: bool) -> Sig {
        let mut v = self.0.clone();
        let fill = if signed { v.last().copied().unwrap_or(SigBit::ZERO) } else { SigBit::ZERO };
        v.resize(width, fill);
        Sig(v)
    }

    /// Concatenation with `self` as the least significant part.
    pub fn concat(&self, high: &Sig) -> Sig {
        let mut v = self.0.clone();
        v.extend_from_slice(&high.0);
        Sig(v)
    }

    pub fn push(&mut self, b: SigBit) {
        self.0.push(b);
    }

    pub fn iter(&self) -> std::slice::Iter<'_, SigBit> {
        self.0.iter()
    }
}

impl fmt::Debug for Sig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(c) = self.as_const() {
            return write!(f, "{c:?}");
        }
        write!(f, "{{")?;
        for (i, b) in self.0.iter().rev().enumerate() {
            if i > 0 {
                write!(f, " ")?;
            }
            write!(f, "{b:?}")?;
        }
        write!(f, "}}")
    }
}

impl FromIterator<SigBit> for Sig {
    fn from_iter<I: IntoIterator<Item = SigBit>>(iter: I) -> Self {
        Sig(iter.into_iter().collect())
    }
}

/// Direction of a module port.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum PortDir {
    Input,
    Output,
    Inout,
}

#[derive(Clone, Debug)]
pub struct Wire {
    pub name: String,
    pub width: u32,
    /// Port direction and position for module ports.
    pub port: Option<(PortDir, u32)>,
    /// Declared signedness (informational).
    pub signed: bool,
    /// User-visible (true) or generated (false).
    pub public: bool,
    pub attrs: Vec<(String, Attr)>,
}

/// An attribute or instance parameter value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attr {
    Const(LogicVec),
    Str(String),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    /// Bitwise complement (`~`), output width = input width.
    Not,
    /// Two's complement negation, output width = input width.
    Neg,
    ReduceAnd,
    ReduceOr,
    ReduceXor,
    ReduceXnor,
    /// `!a` (1-bit).
    LogicNot,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    // Equal-width operands and result.
    And,
    Or,
    Xor,
    Xnor,
    Add,
    Sub,
    Mul,
    /// Division; signed if the flag is set.
    Div,
    Mod,
    // Shifts: `a` and result have equal width; `b` is an unsigned amount.
    Shl,
    Shr,
    /// Arithmetic shift right (sign fill).
    Sshr,
    // Comparisons: equal-width operands, 1-bit result.
    Eq,
    Ne,
    /// `===` / `!==` (compare x and z literally; only in constant code).
    Eqx,
    Nex,
    Lt,
    Le,
    Gt,
    Ge,
    // Logical: any widths, 1-bit result.
    LogicAnd,
    LogicOr,
}

impl BinaryOp {
    pub fn is_compare(self) -> bool {
        matches!(
            self,
            BinaryOp::Eq
                | BinaryOp::Ne
                | BinaryOp::Eqx
                | BinaryOp::Nex
                | BinaryOp::Lt
                | BinaryOp::Le
                | BinaryOp::Gt
                | BinaryOp::Ge
        )
    }

    pub fn is_logic(self) -> bool {
        matches!(self, BinaryOp::LogicAnd | BinaryOp::LogicOr)
    }

    pub fn is_shift(self) -> bool {
        matches!(self, BinaryOp::Shl | BinaryOp::Shr | BinaryOp::Sshr)
    }

    pub fn is_commutative(self) -> bool {
        matches!(
            self,
            BinaryOp::And
                | BinaryOp::Or
                | BinaryOp::Xor
                | BinaryOp::Xnor
                | BinaryOp::Add
                | BinaryOp::Mul
                | BinaryOp::Eq
                | BinaryOp::Ne
                | BinaryOp::LogicAnd
                | BinaryOp::LogicOr
        )
    }
}

/// A clock edge.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Clock {
    pub sig: SigBit,
    pub posedge: bool,
}

/// A control input with polarity (active high when `pol`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Control {
    pub sig: SigBit,
    pub pol: bool,
}

/// A flip-flop vector: `q <= d` on the clock edge, with optional enable,
/// synchronous reset and asynchronous reset.
///
/// Priority: async reset, then sync reset, then enable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dff {
    pub clk: Clock,
    pub d: Sig,
    pub q: Sig,
    pub en: Option<Control>,
    pub srst: Option<(Control, LogicVec)>,
    pub arst: Option<(Control, LogicVec)>,
    /// Power-on value (`x` bits mean unspecified).
    pub init: LogicVec,
}

/// A level-sensitive latch: `q = en ? d : q`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Latch {
    pub en: Control,
    pub d: Sig,
    pub q: Sig,
}

/// A black-box or primitive instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instance {
    pub module: String,
    pub params: Vec<(String, Attr)>,
    pub conns: Vec<(String, PortDir, Sig)>,
}

/// A cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CellKind {
    Unary {
        op: UnaryOp,
        a: Sig,
        y: Sig,
    },
    Binary {
        op: BinaryOp,
        signed: bool,
        a: Sig,
        b: Sig,
        y: Sig,
    },
    /// `y = s ? b : a`
    Mux {
        a: Sig,
        b: Sig,
        s: SigBit,
        y: Sig,
    },
    /// `y = s[i] ? cases[i] : default` for the (one-hot) set bit `i`.
    Pmux {
        default: Sig,
        cases: Vec<Sig>,
        s: Sig,
        y: Sig,
    },
    /// Dynamic part select: `y = a[b + offset +: width(y)]`, `x` outside `a`.
    Shiftx {
        a: Sig,
        b: Sig,
        b_signed: bool,
        offset: i64,
        y: Sig,
    },
    Dff(Box<Dff>),
    Latch(Box<Latch>),
    Instance(Box<Instance>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub name: String,
    pub kind: CellKind,
    pub alive: bool,
}

impl Cell {
    /// The signals the cell drives.
    pub fn outputs(&self) -> Vec<&Sig> {
        match &self.kind {
            CellKind::Unary { y, .. }
            | CellKind::Binary { y, .. }
            | CellKind::Mux { y, .. }
            | CellKind::Pmux { y, .. }
            | CellKind::Shiftx { y, .. } => vec![y],
            CellKind::Dff(d) => vec![&d.q],
            CellKind::Latch(l) => vec![&l.q],
            CellKind::Instance(i) => {
                i.conns.iter().filter(|(_, d, _)| *d != PortDir::Input).map(|(_, _, s)| s).collect()
            }
        }
    }

    /// Applies `f` to every input bit.
    pub fn for_each_input(&self, mut f: impl FnMut(SigBit)) {
        let mut sig = |s: &Sig| s.iter().for_each(|&b| f(b));
        match &self.kind {
            CellKind::Unary { a, .. } => sig(a),
            CellKind::Binary { a, b, .. } => {
                sig(a);
                sig(b);
            }
            CellKind::Mux { a, b, s, .. } => {
                sig(a);
                sig(b);
                f(*s);
            }
            CellKind::Pmux { default, cases, s, .. } => {
                sig(default);
                cases.iter().for_each(&mut sig);
                sig(s);
            }
            CellKind::Shiftx { a, b, .. } => {
                sig(a);
                sig(b);
            }
            CellKind::Dff(d) => {
                sig(&d.d);
                f(d.clk.sig);
                if let Some(en) = d.en {
                    f(en.sig);
                }
                if let Some((c, _)) = &d.srst {
                    f(c.sig);
                }
                if let Some((c, _)) = &d.arst {
                    f(c.sig);
                }
            }
            CellKind::Latch(l) => {
                sig(&l.d);
                f(l.en.sig);
            }
            CellKind::Instance(i) => {
                for (_, d, s) in &i.conns {
                    if *d != PortDir::Output {
                        sig(s);
                    }
                }
            }
        }
    }

    /// Applies `f` to every input bit, allowing replacement.
    pub fn map_inputs(&mut self, mut f: impl FnMut(SigBit) -> SigBit) {
        let mut sig = |s: &mut Sig| s.0.iter_mut().for_each(|b| *b = f(*b));
        match &mut self.kind {
            CellKind::Unary { a, .. } => sig(a),
            CellKind::Binary { a, b, .. } => {
                sig(a);
                sig(b);
            }
            CellKind::Mux { a, b, s, .. } => {
                sig(a);
                sig(b);
                let mut t = Sig::bit(*s);
                sig(&mut t);
                *s = t.0[0];
            }
            CellKind::Pmux { default, cases, s, .. } => {
                sig(default);
                cases.iter_mut().for_each(&mut sig);
                sig(s);
            }
            CellKind::Shiftx { a, b, .. } => {
                sig(a);
                sig(b);
            }
            CellKind::Dff(d) => {
                sig(&mut d.d);
                let mut ctrl: Vec<&mut SigBit> = vec![&mut d.clk.sig];
                if let Some(en) = &mut d.en {
                    ctrl.push(&mut en.sig);
                }
                if let Some((c, _)) = &mut d.srst {
                    ctrl.push(&mut c.sig);
                }
                if let Some((c, _)) = &mut d.arst {
                    ctrl.push(&mut c.sig);
                }
                for c in ctrl {
                    let mut t = Sig::bit(*c);
                    sig(&mut t);
                    *c = t.0[0];
                }
            }
            CellKind::Latch(l) => {
                sig(&mut l.d);
                let mut t = Sig::bit(l.en.sig);
                sig(&mut t);
                l.en.sig = t.0[0];
            }
            CellKind::Instance(i) => {
                for (_, d, s) in &mut i.conns {
                    if *d != PortDir::Output {
                        sig(s);
                    }
                }
            }
        }
    }
}

/// A memory read port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemRead {
    /// Clock for synchronous reads; `None` for asynchronous reads.
    pub clk: Option<Clock>,
    /// Read enable (synchronous ports only).
    pub en: SigBit,
    pub addr: Sig,
    pub data: Sig,
    /// For synchronous ports: whether a write in the same cycle through
    /// each write port is visible (write-first) instead of the old value.
    pub transparent: Vec<bool>,
}

/// A memory write port. Ports listed later win on conflicting writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemWrite {
    pub clk: Clock,
    /// Per-bit write enable.
    pub en: Sig,
    pub addr: Sig,
    pub data: Sig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Memory {
    pub name: String,
    pub width: u32,
    pub depth: u32,
    /// Address of the first word.
    pub offset: i64,
    /// Initial contents, one entry per word (`x` = unspecified).
    pub init: Vec<LogicVec>,
    pub reads: Vec<MemRead>,
    pub writes: Vec<MemWrite>,
    pub alive: bool,
}

/// A flat RTL module.
#[derive(Clone, Debug, Default)]
pub struct Module {
    pub name: String,
    pub wires: IndexVec<WireId, Wire>,
    pub cells: IndexVec<CellId, Cell>,
    pub memories: IndexVec<MemId, Memory>,
    /// Direct connections: `lhs` (wire bits) is driven by `rhs`.
    pub connections: Vec<(Sig, Sig)>,
}

impl Module {
    pub fn new(name: impl Into<String>) -> Module {
        Module { name: name.into(), ..Default::default() }
    }

    pub fn add_wire(&mut self, name: impl Into<String>, width: u32) -> WireId {
        self.wires.push(Wire {
            name: name.into(),
            width,
            port: None,
            signed: false,
            public: true,
            attrs: vec![],
        })
    }

    /// Adds an internal (generated) wire and returns all its bits.
    pub fn new_sig(&mut self, width: usize) -> Sig {
        let n = self.wires.len();
        let w = self.wires.push(Wire {
            name: format!("$n{n}"),
            width: width as u32,
            port: None,
            signed: false,
            public: false,
            attrs: vec![],
        });
        Sig::wire(w, width as u32)
    }

    pub fn add_cell(&mut self, name: impl Into<String>, kind: CellKind) -> CellId {
        self.cells.push(Cell { name: name.into(), kind, alive: true })
    }

    pub fn connect(&mut self, lhs: Sig, rhs: Sig) {
        debug_assert_eq!(lhs.width(), rhs.width());
        self.connections.push((lhs, rhs));
    }

    pub fn wire_sig(&self, w: WireId) -> Sig {
        Sig::wire(w, self.wires[w].width)
    }

    pub fn ports(&self) -> Vec<WireId> {
        let mut p: Vec<WireId> = self
            .wires
            .iter_enumerated()
            .filter(|(_, w)| w.port.is_some())
            .map(|(i, _)| i)
            .collect();
        p.sort_by_key(|&w| self.wires[w].port.unwrap().1);
        p
    }

    pub fn live_cells(&self) -> impl Iterator<Item = CellId> + '_ {
        self.cells.iter_enumerated().filter(|(_, c)| c.alive).map(|(i, _)| i)
    }

    /// Cell counts by kind, for reports.
    pub fn stats(&self) -> Vec<(String, usize)> {
        let mut m = std::collections::BTreeMap::<String, usize>::new();
        for c in self.cells.iter().filter(|c| c.alive) {
            let k = match &c.kind {
                CellKind::Unary { op, .. } => format!("{op:?}"),
                CellKind::Binary { op, .. } => format!("{op:?}"),
                CellKind::Mux { .. } => "Mux".into(),
                CellKind::Pmux { .. } => "Pmux".into(),
                CellKind::Shiftx { .. } => "Shiftx".into(),
                CellKind::Dff(_) => "Dff".into(),
                CellKind::Latch(_) => "Latch".into(),
                CellKind::Instance(i) => i.module.clone(),
            };
            *m.entry(k).or_default() += 1;
        }
        let mems = self.memories.iter().filter(|m| m.alive).count();
        if mems > 0 {
            m.insert("Memory".into(), mems);
        }
        m.into_iter().collect()
    }

    /// A readable name for a signal bit.
    pub fn bit_name(&self, b: SigBit) -> String {
        match b {
            SigBit::Const(l) => format!("1'b{}", l.to_char()),
            SigBit::Wire(w, i) => {
                let wire = &self.wires[w];
                if wire.width == 1 { wire.name.clone() } else { format!("{}[{i}]", wire.name) }
            }
        }
    }
}
