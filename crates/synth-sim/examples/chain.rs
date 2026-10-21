//! Throughput of the interpreter on a chain of simple cells.
use std::time::Instant;

use synth_rtl::{BinaryOp, CellKind, Module, PortDir};
use synth_sim::{Options, Sim};

fn main() {
    let n: usize = std::env::args().nth(1).map_or(5000, |s| s.parse().unwrap());
    let mut m = Module::new("chain");
    let a = m.add_wire("a", 16);
    m.wires[a].port = Some((PortDir::Input, 0));
    let mut prev = m.wire_sig(a);
    for i in 0..n {
        let y = m.new_sig(16);
        let op = if i % 2 == 0 { BinaryOp::Add } else { BinaryOp::Xor };
        m.add_cell(
            format!("c{i}"),
            CellKind::Binary { op, signed: false, a: prev.clone(), b: m.wire_sig(a), y: y.clone() },
        );
        prev = y;
    }
    let y = m.add_wire("y", 16);
    m.wires[y].port = Some((PortDir::Output, 1));
    m.connect(m.wire_sig(y), prev);
    for four in [false, true] {
        let mut sim = Sim::new(&m, &Options { four_state: four, prune: false }).unwrap();
        let (sa, sy) = (sim.signal("a").unwrap(), sim.signal("y").unwrap());
        let iters = 20_000;
        let t = Instant::now();
        let mut acc = 0;
        for i in 0..iters {
            sim.poke(&sa, i);
            sim.eval();
            acc ^= sim.peek(&sy).unwrap_or(0);
        }
        let el = t.elapsed();
        println!(
            "{}-state: {:.2} ns/op ({acc})",
            if four { 4 } else { 2 },
            el.as_nanos() as f64 / (iters as f64 * n as f64)
        );
    }
}
