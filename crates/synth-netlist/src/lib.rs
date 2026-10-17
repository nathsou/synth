//! The mapped netlist: a flat, bit-level graph of primitive cells connected by
//! single-bit nets. It is produced by technology mapping (or read from an
//! external netlist) and consumed by packing, placement, routing, timing and
//! bitstream generation.
//!
//! Top-level design ports are represented as pseudo-cells of kind
//! [`INPUT_PORT`], [`OUTPUT_PORT`] or [`INOUT_PORT`] with a single pin `PAD`,
//! so every net endpoint is a cell pin.

pub mod verilog;

use std::fmt;

use synth_core::{FxHashMap, IndexVec, Interner, OrderedMap, Sym, define_id};

define_id!(
    /// A cell of a [`Netlist`].
    pub struct CellId
);
define_id!(
    /// A single-bit net of a [`Netlist`].
    pub struct NetId
);

/// Kind of the pseudo-cell standing for a top-level input port.
pub const INPUT_PORT: &str = "$input";
/// Kind of the pseudo-cell standing for a top-level output port.
pub const OUTPUT_PORT: &str = "$output";
/// Kind of the pseudo-cell standing for a top-level bidirectional port.
pub const INOUT_PORT: &str = "$inout";

/// One bit of a parameter value.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Bit {
    Zero,
    One,
    X,
    Z,
}

impl Bit {
    pub fn from_char(c: char) -> Option<Bit> {
        match c {
            '0' => Some(Bit::Zero),
            '1' => Some(Bit::One),
            'x' | 'X' => Some(Bit::X),
            'z' | 'Z' => Some(Bit::Z),
            _ => None,
        }
    }

    pub fn to_char(self) -> char {
        match self {
            Bit::Zero => '0',
            Bit::One => '1',
            Bit::X => 'x',
            Bit::Z => 'z',
        }
    }

    /// `true` for `1`; `x` and `z` count as `0`.
    pub fn is_one(self) -> bool {
        self == Bit::One
    }
}

/// A cell parameter or attribute value.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Param {
    /// A sized bit vector, least significant bit first.
    Bits(Vec<Bit>),
    /// An integer (unsized literal).
    Int(i64),
    Str(String),
}

impl Param {
    /// Interprets the value as an unsigned integer of up to 64 bits
    /// (`x`/`z` bits read as 0). Strings of binary digits are accepted too,
    /// as some tools pass bit patterns as strings.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Param::Int(v) => Some(*v as u64),
            Param::Bits(bits) => Some(
                bits.iter()
                    .take(64)
                    .enumerate()
                    .fold(0u64, |acc, (i, b)| acc | ((b.is_one() as u64) << i)),
            ),
            Param::Str(s) => {
                let s = s.strip_prefix("0b").unwrap_or(s);
                u64::from_str_radix(s, 2).ok()
            }
        }
    }

    /// The value as bits, least significant first, zero-extended or
    /// truncated to `width`.
    pub fn to_bits(&self, width: usize) -> Vec<Bit> {
        let mut out = match self {
            Param::Bits(b) => b.clone(),
            Param::Int(v) => {
                (0..64).map(|i| if v >> i & 1 != 0 { Bit::One } else { Bit::Zero }).collect()
            }
            Param::Str(s) => s.chars().rev().filter_map(Bit::from_char).collect(),
        };
        out.resize(width, Bit::Zero);
        out
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Param::Str(s) => Some(s),
            _ => None,
        }
    }
}

impl fmt::Display for Param {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Param::Bits(b) => {
                write!(f, "{}'", b.len())?;
                b.iter().rev().try_for_each(|b| write!(f, "{}", b.to_char()))
            }
            Param::Int(v) => write!(f, "{v}"),
            Param::Str(s) => write!(f, "{s:?}"),
        }
    }
}

/// Direction of a cell pin.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Dir {
    Input,
    Output,
    Inout,
}

/// A pin of a cell. Multi-bit ports are split into one pin per bit, named
/// `PORT[i]`.
#[derive(Clone, Debug)]
pub struct Pin {
    pub name: Sym,
    pub dir: Dir,
    pub net: Option<NetId>,
}

#[derive(Clone, Debug)]
pub struct Cell {
    pub name: Sym,
    pub kind: Sym,
    pub params: OrderedMap<Sym, Param>,
    pub attrs: OrderedMap<Sym, Param>,
    pub pins: Vec<Pin>,
    /// False once the cell has been removed.
    pub alive: bool,
}

impl Cell {
    pub fn pin_index(&self, name: Sym) -> Option<usize> {
        self.pins.iter().position(|p| p.name == name)
    }
}

/// A cell pin: `pins[pin]` of `cells[cell]`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PinRef {
    pub cell: CellId,
    pub pin: u32,
}

/// A constant net value.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Const {
    Zero,
    One,
    /// Undriven or don't-care.
    Undef,
}

#[derive(Clone, Debug)]
pub struct Net {
    pub name: Sym,
    /// The output (or bidirectional) pin driving the net.
    pub driver: Option<PinRef>,
    /// Input (and bidirectional) pins reading the net.
    pub users: Vec<PinRef>,
    /// Set when the net carries a constant instead of a driver.
    pub constant: Option<Const>,
    pub alive: bool,
}

/// An error raised while building a netlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetlistError(pub String);

impl fmt::Display for NetlistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NetlistError {}

/// A flat bit-level netlist.
#[derive(Clone, Debug, Default)]
pub struct Netlist {
    pub names: Interner,
    /// Name of the top-level design.
    pub top: Option<Sym>,
    pub cells: IndexVec<CellId, Cell>,
    pub nets: IndexVec<NetId, Net>,
    cell_by_name: FxHashMap<Sym, CellId>,
}

impl Netlist {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sym(&mut self, s: &str) -> Sym {
        self.names.intern(s)
    }

    pub fn str(&self, s: Sym) -> &str {
        self.names.resolve(s)
    }

    /// Adds a cell with no pins. Cell names must be unique; a numeric suffix
    /// is added if the name is taken.
    pub fn add_cell(&mut self, name: &str, kind: &str) -> CellId {
        let mut name_sym = self.sym(name);
        let mut k = 1;
        while self.cell_by_name.contains_key(&name_sym) {
            name_sym = self.names.intern(&format!("{name}${k}"));
            k += 1;
        }
        let kind = self.sym(kind);
        let id = self.cells.push(Cell {
            name: name_sym,
            kind,
            params: OrderedMap::new(),
            attrs: OrderedMap::new(),
            pins: Vec::new(),
            alive: true,
        });
        self.cell_by_name.insert(name_sym, id);
        id
    }

    pub fn cell_by_name(&self, name: &str) -> Option<CellId> {
        let sym = self.names.get(name)?;
        self.cell_by_name.get(&sym).copied()
    }

    pub fn add_net(&mut self, name: &str) -> NetId {
        let name = self.sym(name);
        self.nets.push(Net { name, driver: None, users: Vec::new(), constant: None, alive: true })
    }

    /// Adds a net tied to a constant.
    pub fn add_const_net(&mut self, value: Const) -> NetId {
        let name = match value {
            Const::Zero => "$const0",
            Const::One => "$const1",
            Const::Undef => "$undef",
        };
        let id = self.add_net(name);
        self.nets[id].constant = Some(value);
        id
    }

    /// Adds a pin to a cell and optionally connects it.
    pub fn add_pin(
        &mut self,
        cell: CellId,
        name: &str,
        dir: Dir,
        net: Option<NetId>,
    ) -> Result<u32, NetlistError> {
        let name = self.sym(name);
        if self.cells[cell].pin_index(name).is_some() {
            return Err(NetlistError(format!(
                "cell {} already has a pin {}",
                self.str(self.cells[cell].name),
                self.str(name)
            )));
        }
        let pin = self.cells[cell].pins.len() as u32;
        self.cells[cell].pins.push(Pin { name, dir, net: None });
        if let Some(net) = net {
            self.connect(PinRef { cell, pin }, net)?;
        }
        Ok(pin)
    }

    /// Connects a pin (currently unconnected) to a net.
    pub fn connect(&mut self, p: PinRef, net: NetId) -> Result<(), NetlistError> {
        let pin = &self.cells[p.cell].pins[p.pin as usize];
        assert!(pin.net.is_none(), "pin already connected");
        let dir = pin.dir;
        if dir == Dir::Output {
            if let Some(d) = self.nets[net].driver {
                return Err(NetlistError(format!(
                    "net {} has multiple drivers: {} and {}",
                    self.str(self.nets[net].name),
                    self.pin_name(d),
                    self.pin_name(p)
                )));
            }
            if self.nets[net].constant.is_some() {
                return Err(NetlistError(format!(
                    "constant net {} driven by {}",
                    self.str(self.nets[net].name),
                    self.pin_name(p)
                )));
            }
            self.nets[net].driver = Some(p);
        } else if dir == Dir::Inout {
            // Bidirectional pins (pads) drive their net when nothing else does.
            if self.nets[net].driver.is_none() && self.nets[net].constant.is_none() {
                self.nets[net].driver = Some(p);
            } else {
                self.nets[net].users.push(p);
            }
        } else {
            self.nets[net].users.push(p);
        }
        self.cells[p.cell].pins[p.pin as usize].net = Some(net);
        Ok(())
    }

    /// Disconnects a pin from its net, if connected.
    pub fn disconnect(&mut self, p: PinRef) {
        let Some(net) = self.cells[p.cell].pins[p.pin as usize].net.take() else { return };
        let n = &mut self.nets[net];
        if n.driver == Some(p) {
            n.driver = None;
        } else {
            n.users.retain(|&u| u != p);
        }
    }

    /// Removes a cell, disconnecting all its pins.
    pub fn remove_cell(&mut self, cell: CellId) {
        for pin in 0..self.cells[cell].pins.len() {
            self.disconnect(PinRef { cell, pin: pin as u32 });
        }
        let c = &mut self.cells[cell];
        c.alive = false;
        let name = c.name;
        self.cell_by_name.remove(&name);
    }

    /// Moves every endpoint of `from` onto `to` and kills `from`.
    pub fn merge_nets(&mut self, from: NetId, to: NetId) -> Result<(), NetlistError> {
        if from == to {
            return Ok(());
        }
        let driver = self.nets[from].driver.take();
        let users = std::mem::take(&mut self.nets[from].users);
        for p in driver.into_iter().chain(users) {
            self.cells[p.cell].pins[p.pin as usize].net = None;
            self.connect(p, to)?;
        }
        if let Some(c) = self.nets[from].constant {
            match self.nets[to].constant {
                None if self.nets[to].driver.is_none() => self.nets[to].constant = Some(c),
                Some(d) if d == c => {}
                _ => {
                    return Err(NetlistError(format!(
                        "net {} would be both constant and driven",
                        self.str(self.nets[to].name)
                    )));
                }
            }
        }
        self.nets[from].alive = false;
        Ok(())
    }

    /// The net connected to a named pin of a cell.
    pub fn pin_net(&self, cell: CellId, pin: &str) -> Option<NetId> {
        let sym = self.names.get(pin)?;
        let c = &self.cells[cell];
        c.pin_index(sym).and_then(|i| c.pins[i].net)
    }

    /// Reads a parameter of a cell.
    pub fn param(&self, cell: CellId, name: &str) -> Option<&Param> {
        let sym = self.names.get(name)?;
        self.cells[cell].params.get(&sym)
    }

    pub fn set_param(&mut self, cell: CellId, name: &str, value: Param) {
        let sym = self.sym(name);
        self.cells[cell].params.insert(sym, value);
    }

    /// `cell.pin` as a human-readable string.
    pub fn pin_name(&self, p: PinRef) -> String {
        let c = &self.cells[p.cell];
        format!("{}.{}", self.str(c.name), self.str(c.pins[p.pin as usize].name))
    }

    pub fn cell_kind(&self, cell: CellId) -> &str {
        self.str(self.cells[cell].kind)
    }

    /// Iterates over live cells.
    pub fn live_cells(&self) -> impl Iterator<Item = CellId> + '_ {
        self.cells.iter_enumerated().filter(|(_, c)| c.alive).map(|(id, _)| id)
    }

    /// Iterates over live nets.
    pub fn live_nets(&self) -> impl Iterator<Item = NetId> + '_ {
        self.nets.iter_enumerated().filter(|(_, n)| n.alive).map(|(id, _)| id)
    }

    /// Counts live cells per kind, sorted by kind name.
    pub fn stats(&self) -> Vec<(String, usize)> {
        let mut m: std::collections::BTreeMap<String, usize> = Default::default();
        for c in self.live_cells() {
            *m.entry(self.cell_kind(c).to_string()).or_default() += 1;
        }
        m.into_iter().collect()
    }

    /// Checks internal consistency of pin/net cross references.
    pub fn check(&self) -> Result<(), NetlistError> {
        for (id, cell) in self.cells.iter_enumerated().filter(|(_, c)| c.alive) {
            for (i, pin) in cell.pins.iter().enumerate() {
                let p = PinRef { cell: id, pin: i as u32 };
                if let Some(net) = pin.net {
                    let n = &self.nets[net];
                    if !n.alive || (n.driver != Some(p) && !n.users.contains(&p)) {
                        return Err(NetlistError(format!("dangling pin {}", self.pin_name(p))));
                    }
                }
            }
        }
        for (_, net) in self.nets.iter_enumerated().filter(|(_, n)| n.alive) {
            for p in net.driver.iter().chain(&net.users) {
                let c = &self.cells[p.cell];
                if !c.alive || c.pins.get(p.pin as usize).and_then(|x| x.net).is_none() {
                    return Err(NetlistError(format!(
                        "net {} references a removed pin",
                        self.str(net.name)
                    )));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_edit() {
        let mut nl = Netlist::new();
        let a = nl.add_net("a");
        let y = nl.add_net("y");
        let inp = nl.add_cell("a", INPUT_PORT);
        nl.add_pin(inp, "PAD", Dir::Output, Some(a)).unwrap();
        let lut = nl.add_cell("lut", "SB_LUT4");
        nl.add_pin(lut, "I0", Dir::Input, Some(a)).unwrap();
        nl.add_pin(lut, "O", Dir::Output, Some(y)).unwrap();
        nl.set_param(lut, "LUT_INIT", Param::Bits(vec![Bit::One, Bit::Zero]));
        assert_eq!(nl.param(lut, "LUT_INIT").unwrap().as_u64(), Some(1));
        assert!(nl.add_pin(lut, "O", Dir::Output, None).is_err());
        let lut2 = nl.add_cell("lut2", "SB_LUT4");
        assert!(nl.add_pin(lut2, "O", Dir::Output, Some(y)).is_err(), "double driver");
        nl.check().unwrap();
        nl.remove_cell(lut);
        assert!(nl.nets[a].users.is_empty());
        nl.check().unwrap();
        assert_eq!(nl.add_cell("lut", "X"), CellId::new(3));
        assert_eq!(nl.str(nl.cells[CellId::new(3)].name), "lut");
    }

    #[test]
    fn merge() {
        let mut nl = Netlist::new();
        let a = nl.add_net("a");
        let b = nl.add_net("b");
        let c = nl.add_cell("c", "X");
        nl.add_pin(c, "O", Dir::Output, Some(a)).unwrap();
        let d = nl.add_cell("d", "X");
        nl.add_pin(d, "I", Dir::Input, Some(b)).unwrap();
        nl.merge_nets(b, a).unwrap();
        assert_eq!(nl.nets[a].users.len(), 1);
        assert!(!nl.nets[b].alive);
        nl.check().unwrap();
    }
}
