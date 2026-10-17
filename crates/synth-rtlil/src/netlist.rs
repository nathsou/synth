//! Conversion of a mapped RTLIL design into a bit-level [`Netlist`].

use synth_core::FxHashMap;
use synth_netlist::{Bit, CellId, Const, Dir, INOUT_PORT, INPUT_PORT, NetId, Netlist, OUTPUT_PORT};

use crate::ast::{Design, Module, PortDir, SigChunk, SigSpec};

/// Knowledge about primitive cells that a netlist reader needs.
pub trait PrimitiveLibrary {
    /// Direction of port `port` of primitive `kind`, or `None` if `kind` is
    /// not a known primitive or has no such port.
    fn port_dir(&self, kind: &str, port: &str) -> Option<Dir>;
}

/// Union-find over signal bits; class roots may carry a constant.
struct Bits {
    parent: Vec<u32>,
    constant: Vec<Option<Const>>,
}

impl Bits {
    fn find(&mut self, mut x: u32) -> u32 {
        while self.parent[x as usize] != x {
            let p = self.parent[x as usize];
            self.parent[x as usize] = self.parent[p as usize];
            x = p;
        }
        x
    }

    fn union(&mut self, a: u32, b: u32) -> Result<(), String> {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return Ok(());
        }
        let (ca, cb) = (self.constant[ra as usize], self.constant[rb as usize]);
        let c = match (ca, cb) {
            (Some(x), Some(y)) if x != y && x != Const::Undef && y != Const::Undef => {
                return Err("conflicting constants connected together".into());
            }
            (Some(Const::Undef), Some(y)) => Some(y),
            (Some(x), _) => Some(x),
            (None, y) => y,
        };
        // Keep the smaller index as root for determinism.
        let (root, child) = if ra < rb { (ra, rb) } else { (rb, ra) };
        self.parent[child as usize] = root;
        self.constant[root as usize] = c;
        Ok(())
    }
}

fn const_of(b: Bit) -> Const {
    match b {
        Bit::Zero => Const::Zero,
        Bit::One => Const::One,
        Bit::X | Bit::Z => Const::Undef,
    }
}

/// Signal bit: either an index into the wire bit space or a constant.
#[derive(Copy, Clone)]
enum SigBit {
    Wire(u32),
    Const(Const),
}

struct Ctx<'a> {
    module: &'a Module,
    wire_base: FxHashMap<&'a str, u32>,
}

impl Ctx<'_> {
    fn bits(&self, sig: &SigSpec) -> Vec<SigBit> {
        let mut out = Vec::with_capacity(sig.width());
        for c in &sig.chunks {
            match c {
                SigChunk::Wire { wire, offset, width } => {
                    let base = self.wire_base[wire.as_str()];
                    out.extend((0..*width).map(|i| SigBit::Wire(base + (*offset + i) as u32)));
                }
                SigChunk::Const(bits) => {
                    out.extend(bits.iter().map(|&b| SigBit::Const(const_of(b))))
                }
            }
        }
        out
    }

    /// Human-readable name of storage bit `i` of a wire.
    fn bit_name(&self, wire_idx: usize, i: usize) -> String {
        let w = &self.module.wires[wire_idx];
        if w.width == 1 {
            w.name.clone()
        } else {
            let idx =
                if w.upto { w.offset + (w.width - 1 - i) as i64 } else { w.offset + i as i64 };
            format!("{}[{}]", w.name, idx)
        }
    }
}

/// Converts the top module of a mapped design into a netlist.
///
/// The top module may contain only cells of known primitive types (from
/// `lib`, or blackbox modules declared in the design) and plain connections.
pub fn to_netlist(design: &Design, lib: &dyn PrimitiveLibrary) -> Result<Netlist, String> {
    let top = design.top().ok_or("cannot determine the top module (mark it with attribute top)")?;
    if !top.processes.is_empty() || !top.memories.is_empty() {
        return Err(format!(
            "module {} contains processes or memories; it is not a mapped netlist",
            top.name
        ));
    }

    let mut ctx = Ctx { module: top, wire_base: FxHashMap::default() };
    let mut owner: Vec<(usize, usize)> = Vec::new(); // bit -> (wire index, bit)
    for (wi, w) in top.wires.iter().enumerate() {
        ctx.wire_base.insert(&w.name, owner.len() as u32);
        owner.extend((0..w.width).map(|b| (wi, b)));
    }
    let n = owner.len();
    let mut bits = Bits { parent: (0..n as u32).collect(), constant: vec![None; n] };

    for (a, b) in &top.connections {
        let (ab, bb) = (ctx.bits(a), ctx.bits(b));
        if ab.len() != bb.len() {
            return Err(format!(
                "width mismatch in connection ({} vs {} bits)",
                ab.len(),
                bb.len()
            ));
        }
        for (x, y) in ab.into_iter().zip(bb) {
            match (x, y) {
                (SigBit::Wire(x), SigBit::Wire(y)) => bits.union(x, y)?,
                (SigBit::Wire(w), SigBit::Const(c)) | (SigBit::Const(c), SigBit::Wire(w)) => {
                    let r = bits.find(w);
                    match bits.constant[r as usize] {
                        Some(old) if old != c && old != Const::Undef && c != Const::Undef => {
                            return Err("wire tied to two different constants".into());
                        }
                        Some(old) if old != Const::Undef => {}
                        _ => bits.constant[r as usize] = Some(c),
                    }
                }
                (SigBit::Const(_), SigBit::Const(_)) => {}
            }
        }
    }

    let mut nl = Netlist::new();
    nl.top = Some(nl.sym(&top.name));

    // One net per class; prefer a public (non-`$`) name.
    let mut class_net: FxHashMap<u32, NetId> = FxHashMap::default();
    let mut const_nets: FxHashMap<Const, NetId> = FxHashMap::default();
    let mut best_name: FxHashMap<u32, (bool, usize)> = FxHashMap::default();
    for bit in 0..n as u32 {
        let r = bits.find(bit);
        let public = !top.wires[owner[bit as usize].0].name.starts_with('$');
        let e = best_name.entry(r).or_insert((public, bit as usize));
        if public && !e.0 {
            *e = (public, bit as usize);
        }
    }
    let mut net_of = |nl: &mut Netlist, bits: &mut Bits, b: SigBit| -> NetId {
        let c = match b {
            SigBit::Const(c) => Some(c),
            SigBit::Wire(w) => {
                let r = bits.find(w);
                bits.constant[r as usize]
            }
        };
        if let Some(c) = c {
            return *const_nets.entry(c).or_insert_with(|| nl.add_const_net(c));
        }
        let SigBit::Wire(w) = b else { unreachable!() };
        let r = bits.find(w);
        *class_net.entry(r).or_insert_with(|| {
            let (_, rep) = best_name[&r];
            let (wi, bi) = owner[rep];
            nl.add_net(&ctx.bit_name(wi, bi))
        })
    };

    // Top-level ports become pseudo-cells.
    let mut ports: Vec<&crate::ast::Wire> = top.wires.iter().filter(|w| w.port.is_some()).collect();
    ports.sort_by_key(|w| w.port.unwrap().1);
    for w in ports {
        let wi = top.wires.iter().position(|x| std::ptr::eq(x, w)).unwrap();
        let base = ctx.wire_base[w.name.as_str()];
        let (kind, dir) = match w.port.unwrap().0 {
            PortDir::Input => (INPUT_PORT, Dir::Output),
            PortDir::Output => (OUTPUT_PORT, Dir::Input),
            PortDir::Inout => (INOUT_PORT, Dir::Inout),
        };
        for b in 0..w.width {
            let net = net_of(&mut nl, &mut bits, SigBit::Wire(base + b as u32));
            let name = ctx.bit_name(wi, b);
            let cell = nl.add_cell(&name, kind);
            if dir != Dir::Input && nl.nets[net].constant.is_some() {
                return Err(format!("input port {name} is tied to a constant"));
            }
            nl.add_pin(cell, "PAD", dir, Some(net)).map_err(|e| e.0)?;
        }
    }

    for c in &top.cells {
        let blackbox = design.module(&c.kind).filter(|m| m.is_blackbox());
        if blackbox.is_none() && design.module(&c.kind).is_some() {
            return Err(format!(
                "cell {} instantiates module {}; flatten the design first",
                c.name, c.kind
            ));
        }
        let cell: CellId = nl.add_cell(&c.name, &c.kind);
        for (k, v) in &c.params {
            nl.set_param(cell, k, v.clone());
        }
        for (k, v) in &c.attrs {
            let s = nl.sym(k);
            nl.cells[cell].attrs.insert(s, v.clone());
        }
        for (port, sig) in &c.connections {
            let dir = blackbox
                .and_then(|m| m.wire(port))
                .and_then(|w| w.port)
                .map(|(d, _)| match d {
                    PortDir::Input => Dir::Input,
                    PortDir::Output => Dir::Output,
                    PortDir::Inout => Dir::Inout,
                })
                .or_else(|| lib.port_dir(&c.kind, port))
                .ok_or_else(|| format!("unknown port {port} of cell type {}", c.kind))?;
            let sbits = ctx.bits(sig);
            let width = sbits.len();
            for (i, b) in sbits.into_iter().enumerate() {
                let pin_name = if width == 1 { port.clone() } else { format!("{port}[{i}]") };
                let net = net_of(&mut nl, &mut bits, b);
                if dir == Dir::Output && nl.nets[net].constant.is_some() {
                    // Output into a constant (unused output tied off): leave it unconnected.
                    nl.add_pin(cell, &pin_name, dir, None).map_err(|e| e.0)?;
                    continue;
                }
                nl.add_pin(cell, &pin_name, dir, Some(net)).map_err(|e| e.0)?;
            }
        }
    }
    nl.check().map_err(|e| e.0)?;
    Ok(nl)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    struct Lib;
    impl PrimitiveLibrary for Lib {
        fn port_dir(&self, kind: &str, port: &str) -> Option<Dir> {
            match (kind, port) {
                ("SB_LUT4", "O") => Some(Dir::Output),
                ("SB_LUT4", _) => Some(Dir::Input),
                _ => None,
            }
        }
    }

    #[test]
    fn simple_netlist() {
        let text = r#"
module \top
  wire width 2 input 1 \a
  wire output 2 \y
  wire $n
  cell \SB_LUT4 \l
    parameter \LUT_INIT 16'0000000000001000
    connect \I0 \a [0]
    connect \I1 \a [1]
    connect \I2 1'0
    connect \I3 1'0
    connect \O $n
  end
  connect \y $n
end
"#;
        let d = parse(text).unwrap();
        let nl = to_netlist(&d, &Lib).unwrap();
        assert_eq!(
            nl.stats(),
            vec![("$input".to_string(), 2), ("$output".to_string(), 1), ("SB_LUT4".to_string(), 1)]
        );
        let l = nl.cell_by_name("l").unwrap();
        let o = nl.pin_net(l, "O").unwrap();
        // The net takes the public name `y` rather than `$n`.
        assert_eq!(nl.str(nl.nets[o].name), "y");
        let i2 = nl.pin_net(l, "I2").unwrap();
        assert_eq!(nl.nets[i2].constant, Some(Const::Zero));
        assert_eq!(nl.str(nl.nets[nl.pin_net(l, "I1").unwrap()].name), "a[1]");
    }
}
