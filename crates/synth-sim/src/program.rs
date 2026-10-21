//! The compiled form: instructions over a buffer of 64-bit words.
//!
//! A value of `w` bits occupies `max(1, ceil(w / 64))` consecutive words of
//! the state, least significant first, with the bits above `w` kept at 0.
//! 4-state programs keep a second buffer of the same shape, the unknown
//! plane, with the encoding of [`synth_core::LogicVec`]: `x` is (unk 1,
//! val 0) and `z` is (unk 1, val 1).

use synth_rtl::{BinaryOp, UnaryOp};

/// No slot, for optional operands.
pub const NONE: u32 = u32::MAX;

/// Words used by a value of `w` bits.
#[inline]
pub fn nwords(w: u32) -> u32 {
    w.div_ceil(64).max(1)
}

/// The mask of the low `w` bits of a word (`w` at most 64).
#[inline]
pub fn mask(w: u32) -> u64 {
    if w >= 64 { u64::MAX } else { (1u64 << w) - 1 }
}

/// A value in the state: its first word and its width in bits.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Val {
    pub off: u32,
    pub w: u32,
}

impl Val {
    pub fn words(self) -> u32 {
        nwords(self.w)
    }

    pub fn narrow(self) -> bool {
        self.w <= 64
    }
}

/// Instruction kinds. Unless noted, `d` is the destination, `a` and `b` the
/// operands and `w` the operand width; narrow kinds need every value to fit
/// in one word, wide operations go through [`K::Wide`].
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum K {
    /// `d = a` over `nwords(w)` words.
    Copy,
    /// `d = c (a constant, or 0 when NONE) | parts[a..a + b]`, `w` bits.
    Gather,
    Not,
    Neg,
    RedAnd,
    RedOr,
    RedXor,
    RedXnor,
    LogicNot,
    And,
    Or,
    Xor,
    Xnor,
    Add,
    Sub,
    Mul,
    DivU,
    DivS,
    ModU,
    ModS,
    /// Shifts: `c` is the width of the amount `b`.
    Shl,
    Shr,
    Sshr,
    Eq,
    Ne,
    Eqx,
    Nex,
    LtU,
    LtS,
    LeU,
    LeS,
    GtU,
    GtS,
    GeU,
    GeS,
    /// `d = a && b`; `w` and `c` are the widths of `a` and `b`.
    LAnd,
    LOr,
    /// `d = s ? b : a` over `w` bits (any number of words), with `s` bit
    /// `x & 63` of word `c`; `x >> 8` set for a `?:` multiplexer (merges
    /// both inputs on an unknown select, where `if` takes `a`).
    Mux,
    /// One-hot select: `a` default, `b` the select of `c` bits, the case
    /// values at `lists[x..x + c]`; `w` bits, any number of words.
    Pmux,
    /// Dynamic part select of `w` (at most 64) bits: `shiftx[c]`.
    Shiftx,
    /// Asynchronous memory read: memory `c`, address `a` of `b` bits.
    MemRd,
    /// Level-sensitive latch `d = en ? a : d`, `en` bit `x & 63` of word `c`,
    /// active high when `x >> 8 & 1`.
    Latch,
    /// Asynchronous reset: `d = a` while control bit `x & 63` of word `c` is
    /// at polarity `x >> 8 & 1`.
    Arst,
    /// A strongly connected group: the next `a` instructions repeat until
    /// the `c` slots listed at `lists[b..]` stop changing.
    Loop,
    /// Any operation on wide values: `wide[x]`.
    Wide,
    /// Flip-flop update with enable: `d = en ? a : b` (enable as for `Latch`).
    FfEn,
    /// Flip-flop update with resets: `flops[x]`.
    Ff,
    /// Memory write port `writes[x]`.
    MemWr,
    /// Synchronous memory read port `sync_reads[x]`.
    MemRdSync,
}

/// One instruction. Field meanings depend on the kind (see [`K`]).
#[derive(Copy, Clone, Debug)]
pub struct Ins {
    pub k: K,
    pub w: u32,
    pub d: u32,
    pub a: u32,
    pub b: u32,
    pub c: u32,
    pub x: u64,
}

impl Ins {
    pub fn new(k: K, w: u32, d: u32, a: u32, b: u32) -> Ins {
        Ins { k, w, d, a, b, c: 0, x: 0 }
    }
}

/// A run of at most 64 bits copied by [`K::Gather`]: `len` bits from bit
/// `lo` of word `src` to bit `pos` of word `dst` (an offset from the
/// destination's first word).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Part {
    pub src: u32,
    pub dst: u32,
    pub lo: u8,
    pub pos: u8,
    pub len: u8,
}

/// A control bit: bit `bit` of word `word`, active at `pol`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ctl {
    pub word: u32,
    pub bit: u8,
    pub pol: bool,
}

/// A dynamic part select (see [`K::Shiftx`]).
#[derive(Clone, Debug)]
pub struct ShiftxInfo {
    pub a: Val,
    pub b: Val,
    pub b_signed: bool,
    pub offset: i64,
}

/// The operation of a [`K::Wide`] instruction.
#[derive(Clone, Debug)]
pub enum WideOp {
    Unary(UnaryOp),
    Binary(BinaryOp, bool),
    Shiftx { b_signed: bool, offset: i64 },
}

/// An operation on wide values, evaluated through [`synth_core::LogicVec`].
#[derive(Clone, Debug)]
pub struct WideIns {
    pub op: WideOp,
    pub d: Val,
    pub a: Val,
    pub b: Val,
}

/// A flip-flop with resets (see [`K::Ff`]); writes `dst` (its output or a
/// shadow).
#[derive(Clone, Debug)]
pub struct Flop {
    pub dst: u32,
    pub d: u32,
    pub q: u32,
    pub w: u32,
    pub en: Option<Ctl>,
    pub srst: Option<(Ctl, u32)>,
    pub en_over_srst: bool,
    pub arst: Option<(Ctl, u32)>,
}

/// A memory: `depth` words of `width` bits from address `offset`, each
/// `nwords(width)` words of the memory buffer from `base`.
#[derive(Clone, Debug)]
pub struct Mem {
    pub name: String,
    pub width: u32,
    pub depth: u32,
    pub offset: i64,
    pub base: usize,
}

/// A memory write port.
#[derive(Clone, Debug)]
pub struct MemWrite {
    pub mem: u32,
    pub addr: Val,
    pub data: Val,
    pub en: Val,
}

/// A synchronous memory read port: on its edge, when `en`, `dst` takes
/// the word at `addr`, or for each listed write port of the same edge that
/// writes that address, the bits it writes.
#[derive(Clone, Debug)]
pub struct SyncRead {
    pub mem: u32,
    pub dst: u32,
    /// The port's output (kept by `dst` while disabled).
    pub q: u32,
    pub en: Option<Ctl>,
    pub addr: Val,
    pub transparent: Vec<u32>,
}

/// The logic of one clock edge.
#[derive(Clone, Debug, Default)]
pub struct Domain {
    /// Sources set when it fires.
    pub bit: u64,
    /// Run first, for every domain that fires: values needed after state
    /// changes (into shadows) and synchronous reads.
    pub sample: Vec<Ins>,
    /// Memory writes, after every sample.
    pub write: Vec<Ins>,
    /// State updates, last.
    pub commit: Vec<Ins>,
}

/// A clock bit and the domains of its edges.
#[derive(Clone, Debug)]
pub struct Clock {
    pub word: u32,
    pub bit: u8,
    pub pos: Option<u32>,
    pub neg: Option<u32>,
}

/// Auxiliary tables referenced by instructions.
#[derive(Clone, Debug, Default)]
pub struct Aux {
    pub parts: Vec<Part>,
    pub lists: Vec<u32>,
    pub shiftx: Vec<ShiftxInfo>,
    pub wide: Vec<WideIns>,
    pub flops: Vec<Flop>,
    pub mems: Vec<Mem>,
    pub writes: Vec<MemWrite>,
    pub sync_reads: Vec<SyncRead>,
}

/// Where a bit of a named signal lives.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Loc {
    Const(synth_core::Logic),
    Bit { word: u32, bit: u8 },
}

/// What may write a slot from outside the program.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SlotKind {
    /// Computed by an instruction; not writable.
    Comb,
    /// An undriven bit: a primary input (or an instance output).
    Input,
    /// Held state (flip-flop, latch or memory read register).
    State,
    Const,
}

/// A compiled design.
#[derive(Clone, Debug)]
pub struct Program {
    pub four_state: bool,
    /// Initial state, both planes (`init_unk` is empty for 2-state).
    pub init_val: Vec<u64>,
    pub init_unk: Vec<u64>,
    pub mem_init_val: Vec<u64>,
    pub mem_init_unk: Vec<u64>,
    /// Combinational instructions in evaluation order, and the sources
    /// each depends on (the same for every member of a loop).
    pub ops: Vec<Ins>,
    pub op_masks: Vec<u64>,
    pub aux: Aux,
    pub domains: Vec<Domain>,
    pub clocks: Vec<Clock>,
    /// Per word: the source bits set when it is written from outside.
    pub word_src: Vec<u64>,
    pub word_kind: Vec<SlotKind>,
    /// Named signals: (name, bit locations, least significant first).
    pub signals: Vec<(String, Vec<Loc>)>,
    pub warnings: Vec<String>,
}

/// The source bit set when memories are written from outside.
pub const HOST: u64 = 1 << 63;
