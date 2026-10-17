//! Writes a netlist as structural Verilog (for simulation with the
//! primitive library's models).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::{Const, Dir, INOUT_PORT, INPUT_PORT, NetId, Netlist, OUTPUT_PORT, Param};

fn esc(s: &str) -> String {
    if !s.is_empty()
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        s.to_string()
    } else {
        format!("\\{s} ")
    }
}

/// Writes `nl` as module `name`. Port pseudo-cells named `base[i]` are
/// grouped back into vector ports.
pub fn write(nl: &Netlist, name: &str) -> String {
    let mut o = String::new();
    // Ports: base name -> (direction, bit -> net).
    let mut ports: BTreeMap<String, (&str, BTreeMap<u32, NetId>)> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for c in nl.live_cells() {
        let kind = nl.cell_kind(c);
        let dir = match kind {
            INPUT_PORT => "input",
            OUTPUT_PORT => "output",
            INOUT_PORT => "inout",
            _ => continue,
        };
        let cname = nl.str(nl.cells[c].name).to_string();
        let (base, idx) = match cname.rsplit_once('[') {
            Some((b, rest))
                if rest.ends_with(']') && rest[..rest.len() - 1].parse::<u32>().is_ok() =>
            {
                (b.to_string(), rest[..rest.len() - 1].parse().unwrap())
            }
            _ => (cname.clone(), 0),
        };
        let Some(net) = nl.cells[c].pins[0].net else { continue };
        if !ports.contains_key(&base) {
            order.push(base.clone());
        }
        ports.entry(base).or_insert((dir, BTreeMap::new())).1.insert(idx, net);
    }
    let net_name = |n: NetId| -> String {
        match nl.nets[n].constant {
            Some(Const::Zero) => "1'b0".into(),
            Some(Const::One) => "1'b1".into(),
            Some(Const::Undef) => "1'bx".into(),
            None => format!("\\n{} ", n.index()),
        }
    };
    let _ = writeln!(
        o,
        "module {}({});",
        esc(name),
        order.iter().map(|p| esc(p)).collect::<Vec<_>>().join(", ")
    );
    for p in &order {
        let (dir, bits) = &ports[p];
        let w = bits.keys().max().copied().unwrap_or(0) + 1;
        let range = if bits.len() > 1 || w > 1 { format!("[{}:0] ", w - 1) } else { String::new() };
        let _ = writeln!(o, "  {dir} wire {range}{};", esc(p));
    }
    for n in nl.live_nets() {
        if nl.nets[n].constant.is_none() {
            let _ = writeln!(o, "  wire {};", net_name(n));
        }
    }
    for p in &order {
        let (dir, bits) = &ports[p];
        let vector = bits.len() > 1 || bits.keys().any(|&k| k > 0);
        for (&i, &n) in bits {
            let port = if vector { format!("{}[{i}]", esc(p)) } else { esc(p) };
            match *dir {
                "input" => {
                    let _ = writeln!(o, "  assign {} = {port};", net_name(n));
                }
                "output" => {
                    let _ = writeln!(o, "  assign {port} = {};", net_name(n));
                }
                _ => {
                    let _ = writeln!(o, "  tran ({port}, {});", net_name(n));
                }
            }
        }
    }
    let mut dummies: Vec<String> = Vec::new();
    let mut body = String::new();
    for c in nl.live_cells() {
        let kind = nl.cell_kind(c);
        if matches!(kind, INPUT_PORT | OUTPUT_PORT | INOUT_PORT) {
            continue;
        }
        let cell = &nl.cells[c];
        let params: Vec<String> = cell
            .params
            .iter()
            .map(|(k, v)| {
                let val = match v {
                    Param::Bits(b) => {
                        format!(
                            "{}'b{}",
                            b.len(),
                            b.iter().rev().map(|x| x.to_char()).collect::<String>()
                        )
                    }
                    Param::Int(i) => i.to_string(),
                    Param::Str(s) => format!("{s:?}"),
                };
                format!(".{}({val})", esc(nl.str(*k)))
            })
            .collect();
        // Group pins `P[i]` into vectors.
        let mut pins: BTreeMap<String, BTreeMap<u32, Option<NetId>>> = BTreeMap::new();
        let mut outputs: std::collections::BTreeSet<String> = Default::default();
        if kind == "SB_LUT4" {
            // Unused LUT inputs simulate as x; tie them low.
            for i in ["I0", "I1", "I2", "I3"] {
                pins.entry(i.to_string()).or_default();
            }
        }
        for pin in &cell.pins {
            let pname = nl.str(pin.name);
            let (base, idx) = match pname.split_once('[') {
                Some((b, r)) => (b.to_string(), r.trim_end_matches(']').parse().unwrap_or(0)),
                None => (pname.to_string(), 0),
            };
            if pin.dir == Dir::Output {
                outputs.insert(base.clone());
            }
            pins.entry(base).or_default().insert(idx, pin.net);
        }
        let conns: Vec<String> = pins
            .iter()
            .map(|(p, bits)| {
                let w = bits.keys().max().copied().unwrap_or(0) + 1;
                let parts: Vec<String> = (0..w)
                    .rev()
                    .map(|i| match bits.get(&i).copied().flatten() {
                        Some(n) => net_name(n),
                        None if outputs.contains(p) => {
                            let d = format!("\\$nc{}_{} ", c.index(), dummies.len());
                            dummies.push(d.clone());
                            d
                        }
                        None => "1'b0".into(),
                    })
                    .collect();
                let e = if parts.len() == 1 {
                    parts[0].clone()
                } else {
                    format!("{{{}}}", parts.join(", "))
                };
                format!(".{}({e})", esc(p))
            })
            .collect();
        let ps =
            if params.is_empty() { String::new() } else { format!(" #({})", params.join(", ")) };
        let _ = writeln!(
            body,
            "  {}{ps} {} ({});",
            esc(kind),
            esc(nl.str(cell.name)),
            conns.join(", ")
        );
    }
    for d in &dummies {
        let _ = writeln!(o, "  wire {d};");
    }
    o.push_str(&body);
    o.push_str("endmodule\n");
    o
}
