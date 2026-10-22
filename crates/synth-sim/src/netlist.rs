//! Gate-level netlists as RTL modules, to simulate them like any design.
//!
//! Every net becomes a one-bit wire (named after the net), port pseudo-cells
//! become the module's ports (bits `name[i]` grouped back into vectors), and
//! each primitive cell is turned into RTL cells by the architecture's
//! models, given as a callback: synth-sim knows no architecture.

use synth_core::{FxHashMap, Logic};
use synth_netlist::{CellId, Const, INOUT_PORT, INPUT_PORT, NetId, Netlist, OUTPUT_PORT};
use synth_rtl::{Module, PortDir, Sig, SigBit, WireId};

/// Builds the RTL of one primitive cell. `pin(name)` gives the bit of the
/// net on a pin (`None` when unconnected). Returns `Ok(false)` for a kind
/// it does not model.
pub type CellModel<'a> = dyn FnMut(&mut Module, &Netlist, CellId, &dyn Fn(&str) -> Option<SigBit>) -> Result<bool, String>
    + 'a;

/// A port: its name, direction and bits (index, net).
type Port = (String, PortDir, Vec<(u32, NetId)>);

/// Splits a port bit name `name[i]` into its base and index.
fn port_bit(name: &str) -> (&str, Option<u32>) {
    if let Some(open) = name.rfind('[')
        && name.ends_with(']')
        && let Ok(i) = name[open + 1..name.len() - 1].parse()
    {
        return (&name[..open], Some(i));
    }
    (name, None)
}

/// Converts a netlist into a module, with `model` building each cell.
pub fn from_netlist(nl: &Netlist, model: &mut CellModel) -> Result<Module, String> {
    let top = nl.top.map(|t| nl.str(t).to_string()).unwrap_or_else(|| "top".into());
    let mut m = Module::new(top);
    // Ports: group the bits of each by name.
    let mut ports: Vec<Port> = Vec::new();
    for c in nl.live_cells() {
        let dir = match nl.cell_kind(c) {
            INPUT_PORT => PortDir::Input,
            OUTPUT_PORT => PortDir::Output,
            INOUT_PORT => PortDir::Inout,
            _ => continue,
        };
        let Some(net) = nl.pin_net(c, "PAD") else { continue };
        let name = nl.str(nl.cells[c].name);
        let (base, idx) = port_bit(name);
        match ports.iter_mut().find(|p| p.0 == base) {
            Some(p) => p.2.push((idx.unwrap_or(0), net)),
            None => ports.push((base.to_string(), dir, vec![(idx.unwrap_or(0), net)])),
        }
    }
    let mut bit_of: FxHashMap<NetId, SigBit> = FxHashMap::default();
    let mut outputs: Vec<(WireId, u32, NetId)> = Vec::new();
    for (pos, (name, dir, bits)) in ports.iter().enumerate() {
        let width = bits.iter().map(|b| b.0 + 1).max().unwrap_or(1);
        let w = m.add_wire(name.clone(), width);
        m.wires[w].port = Some((*dir, pos as u32));
        for &(i, net) in bits {
            match dir {
                PortDir::Input | PortDir::Inout => {
                    bit_of.insert(net, SigBit::Wire(w, i));
                }
                PortDir::Output => outputs.push((w, i, net)),
            }
        }
    }
    let mut net_bit = |m: &mut Module, net: NetId| -> SigBit {
        if let Some(&b) = bit_of.get(&net) {
            return b;
        }
        let n = &nl.nets[net];
        let b = match n.constant {
            Some(Const::Zero) => SigBit::ZERO,
            Some(Const::One) => SigBit::ONE,
            Some(Const::Undef) => SigBit::Const(Logic::X),
            None => {
                let w = m.add_wire(nl.str(n.name).to_string(), 1);
                SigBit::Wire(w, 0)
            }
        };
        bit_of.insert(net, b);
        b
    };
    // Give every connected net its bit first, so that models see them all.
    let mut pin_bits: FxHashMap<(CellId, String), SigBit> = FxHashMap::default();
    for c in nl.live_cells() {
        if matches!(nl.cell_kind(c), INPUT_PORT | OUTPUT_PORT | INOUT_PORT) {
            continue;
        }
        for p in &nl.cells[c].pins {
            if let Some(net) = p.net {
                let b = net_bit(&mut m, net);
                pin_bits.insert((c, nl.str(p.name).to_string()), b);
            }
        }
    }
    for c in nl.live_cells() {
        let kind = nl.cell_kind(c);
        if matches!(kind, INPUT_PORT | OUTPUT_PORT | INOUT_PORT) {
            continue;
        }
        let pin = |name: &str| pin_bits.get(&(c, name.to_string())).copied();
        if !model(&mut m, nl, c, &pin)? {
            return Err(format!(
                "cell `{}` of kind `{kind}` has no simulation model",
                nl.str(nl.cells[c].name)
            ));
        }
    }
    for (w, i, net) in outputs {
        let b = net_bit(&mut m, net);
        m.connect(Sig::bit(SigBit::Wire(w, i)), Sig::bit(b));
    }
    Ok(m)
}
