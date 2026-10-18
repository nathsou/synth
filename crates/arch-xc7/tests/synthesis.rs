use synth_core::FxHashMap;
use synth_core::diag::SourceMap;
use synth_netlist::{Const, NetId, Netlist, Param};

fn synth(src: &str) -> Netlist {
    let mut sm = SourceMap::new();
    let f = sm.add("test.v", src);
    let (ast, d) = synth_verilog::parse_files(&mut sm, &[f], &Default::default());
    assert!(!d.has_errors(), "{}", d.render(&sm));
    let (m, d) = synth_elab::elaborate(&ast, &sm, &Default::default(), &synth_elab::NoBlackBoxes);
    assert!(!d.has_errors(), "{}", d.render(&sm));
    arch_xc7::synth::synthesize(m.unwrap()).unwrap().0
}

// Evaluate the emitted primitive network, then compare against integer RTL
// semantics. The independent vendor-model/formal tests live in ci_artix7.py.
fn evaluate(nl: &Netlist, inputs: &[(&str, u64)]) -> FxHashMap<String, u64> {
    let mut values: FxHashMap<NetId, bool> = FxHashMap::default();
    let port_bit = |name: &str| match name.split_once('[') {
        Some((base, i)) => (base.to_string(), i.trim_end_matches(']').parse::<u32>().unwrap()),
        None => (name.to_string(), 0),
    };
    for n in nl.live_nets() {
        if let Some(k) = nl.nets[n].constant {
            values.insert(n, k == Const::One);
        }
    }
    for c in nl.live_cells().filter(|&c| nl.cell_kind(c) == synth_netlist::INPUT_PORT) {
        let (name, i) = port_bit(nl.str(nl.cells[c].name));
        let value = inputs.iter().find(|(n, _)| *n == name).unwrap().1;
        values.insert(nl.pin_net(c, "PAD").unwrap(), value >> i & 1 == 1);
    }
    loop {
        let before = values.len();
        for c in nl.live_cells() {
            let kind = nl.cell_kind(c);
            let get = |pin: &str| nl.pin_net(c, pin).and_then(|n| values.get(&n).copied());
            let outputs: Vec<(String, bool)> = if let Some(k) = kind.strip_prefix("LUT") {
                let k: usize = k.parse().unwrap();
                let ins: Option<Vec<_>> = (0..k).map(|i| get(&format!("I{i}"))).collect();
                let Some(ins) = ins else { continue };
                let idx: usize = ins.iter().enumerate().map(|(i, &v)| (v as usize) << i).sum();
                let init = nl.param(c, "INIT").and_then(Param::as_u64).unwrap();
                vec![("O".into(), init >> idx & 1 == 1)]
            } else if kind == "CARRY4" {
                let (Some(ci), Some(cyinit)) = (get("CI"), get("CYINIT")) else { continue };
                let ins: Option<Vec<_>> = (0..4)
                    .map(|i| Some((get(&format!("S[{i}]"))?, get(&format!("DI[{i}]"))?)))
                    .collect();
                let Some(ins) = ins else { continue };
                let mut carry = ci | cyinit;
                let mut out = Vec::new();
                for (i, (s, di)) in ins.into_iter().enumerate() {
                    out.push((format!("O[{i}]"), s ^ carry));
                    carry = if s { carry } else { di };
                    out.push((format!("CO[{i}]"), carry));
                }
                out
            } else {
                continue;
            };
            for (pin, value) in outputs {
                if let Some(n) = nl.pin_net(c, &pin) {
                    values.insert(n, value);
                }
            }
        }
        if values.len() == before {
            break;
        }
    }
    let mut out = FxHashMap::default();
    for c in nl.live_cells().filter(|&c| nl.cell_kind(c) == synth_netlist::OUTPUT_PORT) {
        let (name, i) = port_bit(nl.str(nl.cells[c].name));
        *out.entry(name).or_default() |= (values[&nl.pin_net(c, "PAD").unwrap()] as u64) << i;
    }
    out
}

#[test]
fn arithmetic_across_carry4_boundaries() {
    for width in [3, 4, 5, 8, 9, 17] {
        let src = format!(
            "module top(input [{}:0] a,b, input cin, output [{}:0] sum, output [{}:0] diff, output lt); assign sum = {{1'b0,a}} + {{1'b0,b}} + cin; assign diff = a-b; assign lt = a<b; endmodule",
            width - 1,
            width,
            width - 1
        );
        let nl = synth(&src);
        let mask = (1u64 << width) - 1;
        for i in 0..256u64 {
            let a = (i.wrapping_mul(193) ^ (i << 12)) & mask;
            let b = (i.wrapping_mul(73) ^ (i << 7)) & mask;
            let cin = i & 1;
            let out = evaluate(&nl, &[("a", a), ("b", b), ("cin", cin)]);
            assert_eq!(out["sum"], a + b + cin, "width {width}, input {i}");
            assert_eq!(out["diff"], a.wrapping_sub(b) & mask);
            assert_eq!(out["lt"], (a < b) as u64);
        }
    }
}

#[test]
fn lut6_high_truth_bits_and_both_output_polarities() {
    let truth = 0xb6d39a50e7c1842fu64;
    let nl = synth(
        "module top(input [5:0] a, output y,z); assign y=64'hb6d39a50e7c1842f >> a; assign z=~y; endmodule",
    );
    for a in 0..64 {
        let out = evaluate(&nl, &[("a", a)]);
        assert_eq!(out["y"], truth >> a & 1);
        assert_eq!(out["z"], 1 ^ (truth >> a & 1));
    }
}

#[test]
fn async_memory_does_not_absorb_an_address_register_on_another_clock() {
    let src = "module top(input rclk,wclk,we, input [4:0] a, input [7:0] d, output [7:0] q); reg [4:0] ra; reg [7:0] mem[0:31]; always @(posedge rclk) ra<=a; always @(posedge wclk) if(we) mem[a]<=d; assign q=mem[ra]; endmodule";
    let mut sm = SourceMap::new();
    let f = sm.add("async.v", src);
    let (ast, _) = synth_verilog::parse_files(&mut sm, &[f], &Default::default());
    let (m, d) = synth_elab::elaborate(&ast, &sm, &Default::default(), &synth_elab::NoBlackBoxes);
    assert!(!d.has_errors(), "{}", d.render(&sm));
    let mut m = m.unwrap();
    synth_rtl::opt::clean(&mut m).unwrap();
    assert_eq!(synth_rtl::mem::merge_read_ffs(&mut m), 0);
    assert!(m.memories.iter().flat_map(|mem| &mem.reads).all(|rd| rd.clk.is_none()));
}
