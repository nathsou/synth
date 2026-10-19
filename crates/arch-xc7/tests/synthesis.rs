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
            } else if kind == "DSP48E1" {
                let bus = |p: &str, w: usize| -> Option<u64> {
                    (0..w)
                        .map(|i| get(&format!("{p}[{i}]")).map(|v| (v as u64) << i))
                        .collect::<Option<Vec<_>>>()
                        .map(|v| v.into_iter().sum())
                };
                let (Some(a), Some(b)) = (bus("A", 25), bus("B", 18)) else { continue };
                let a = ((a << 39) as i64) >> 39;
                let b = ((b << 46) as i64) >> 46;
                let p = (a * b) as u64;
                (0..48).map(|i| (format!("P[{i}]"), p >> i & 1 != 0)).collect()
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
fn three_bit_addition_fits_lut6s_without_a_carry_block() {
    let nl = synth("module top(input [2:0] a,b,output [2:0] y); assign y=a+b; endmodule");
    assert!(!nl.live_cells().any(|c| nl.cell_kind(c) == "CARRY4"));
    assert!(nl.live_cells().filter(|&c| arch_xc7::pack::is_lut(nl.cell_kind(c))).count() <= 3);
    for a in 0..8 {
        for b in 0..8 {
            assert_eq!(evaluate(&nl, &[("a", a), ("b", b)])["y"], (a + b) & 7);
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

#[test]
fn bram_width_selection_minimizes_blocks_across_depth_boundaries() {
    for (width, depth, count, mode) in [
        (24, 512, 1, "SDP"),
        (36, 512, 1, "SDP"),
        (36, 513, 2, "TDP"),
        (36, 1100, 3, "SDP"),
        (37, 512, 2, "SDP"),
    ] {
        let nl = synth(&format!(
            "module top(input clk,we,input [15:0] a,input [{}:0] d,output reg [{}:0] q); reg [{}:0] mem[0:{}]; always @(posedge clk) begin if(we) mem[a]<=d; q<=mem[a]; end endmodule",
            width - 1,
            width - 1,
            width - 1,
            depth - 1
        ));
        let brams: Vec<_> = nl.live_cells().filter(|&c| nl.cell_kind(c) == "RAMB18E1").collect();
        assert_eq!(brams.len(), count, "{width}x{depth}");
        for c in brams {
            assert_eq!(nl.param(c, "RAM_MODE").and_then(Param::as_str), Some(mode));
        }
    }
}

#[test]
fn packing_preserves_carry_arithmetic_and_lut_semantics() {
    for width in [3, 4, 5, 8, 9, 17] {
        let src = format!(
            "module top(input [{}:0] a,b, input cin, output [{}:0] sum, output [{}:0] diff, output lt); assign sum={{1'b0,a}}+{{1'b0,b}}+cin; assign diff=a-b; assign lt=a<b; endmodule",
            width - 1,
            width,
            width - 1
        );
        let mut nl = synth(&src);
        let packed = arch_xc7::pack::pack(&mut nl).unwrap();
        assert!(packed.chains.iter().all(|c| !c.is_empty()));
        let mask = (1u64 << width) - 1;
        for i in 0..512u64 {
            let a = (i.wrapping_mul(193) ^ (i << 12)) & mask;
            let b = (i.wrapping_mul(73) ^ (i << 7)) & mask;
            let out = evaluate(&nl, &[("a", a), ("b", b), ("cin", i & 1)]);
            assert_eq!(out["sum"], a + b + (i & 1));
            assert_eq!(out["diff"], a.wrapping_sub(b) & mask);
            assert_eq!(out["lt"], u64::from(a < b));
        }
    }
}

#[test]
fn packing_enforces_shared_control_sets() {
    let mut nl = synth(
        "module top(input aclk,bclk,en,rst,input [7:0] d,output reg [7:0] a,b,c); always @(posedge aclk) if(en) a<=d; always @(negedge aclk) if(rst)b<=0;else b<=d; always @(posedge bclk or posedge rst) if(rst)c<=0;else c<=d; endmodule",
    );
    let p = arch_xc7::pack::pack(&mut nl).unwrap();
    // Twenty-four direct-data registers in three control sets fill three
    // slices: four through the AX-DX bypasses and four through O5 route-
    // throughs into the second bank. A fourth slice holds the two shared
    // constant LUTs; the route-throughs are the only other LUTs.
    assert_eq!(p.slices.len(), 4);
    let luts = nl.live_cells().filter(|&c| arch_xc7::pack::is_lut(nl.cell_kind(c))).count();
    let through: usize = p.slices.iter().map(|s| s.lut5.iter().flatten().count()).sum();
    assert_eq!((luts, through), (14, 12));
    for slice in &p.slices {
        for ff in slice.ffs.iter().flatten() {
            let ctrl = slice.controls.as_ref().unwrap();
            assert_eq!(nl.pin_net(*ff, "C"), Some(ctrl.clock));
            assert_eq!(
                nl.param(*ff, "IS_C_INVERTED").and_then(Param::as_u64).unwrap_or(0) != 0,
                ctrl.inverted
            );
            assert_eq!(matches!(nl.cell_kind(*ff), "FDRE" | "FDSE"), ctrl.synchronous);
        }
    }
}

#[test]
fn unused_carry_lanes_take_no_propagate_luts() {
    // A six-bit sum leaves the top two lanes of its second CARRY4 unused;
    // their sites are free for other LUTs.
    let mut nl = synth(
        "module top(input clk,input [5:0] a,b,output reg [5:0] y); always @(posedge clk) y<=a+b; endmodule",
    );
    let p = arch_xc7::pack::pack(&mut nl).unwrap();
    let mut dead = 0;
    for slice in &p.slices {
        let Some(c) = slice.carry else { continue };
        let used = |pin: String| nl.pin_net(c, &pin).is_some_and(|n| !nl.nets[n].users.is_empty());
        for lane in 0..4 {
            if !(lane..4).any(|k| used(format!("O[{k}]")) || used(format!("CO[{k}]"))) {
                // Any LUT there is unrelated logic, not a propagate copy.
                let s = nl.pin_net(c, &format!("S[{lane}]"));
                assert!(slice.luts[lane].is_none_or(|l| nl.pin_net(l, "O") != s), "lane {lane}");
                dead += 1;
            }
        }
    }
    assert!(dead >= 2, "{dead} unused lanes");
}

#[test]
fn dsp_products_preserve_signed_extension_and_truncation() {
    for (aw, bw, yw) in [
        (9, 9, 18),
        (24, 17, 48),
        (25, 18, 48),
        (25, 25, 64),
        (33, 19, 64),
        (64, 64, 64),
        (32, 32, 17),
    ] {
        for signed in [false, true] {
            let sign = if signed { "signed" } else { "" };
            let src = format!(
                "module top(input {sign} [{}:0] a,input {sign} [{}:0] b,output [{}:0] y); assign y=a*b; endmodule",
                aw - 1,
                bw - 1,
                yw - 1
            );
            let mut nl = synth(&src);
            assert!(nl.live_cells().any(|c| nl.cell_kind(c) == "DSP48E1"));
            arch_xc7::pack::pack(&mut nl).unwrap();
            let mask = |w: u32| if w == 64 { u64::MAX } else { (1u64 << w) - 1 };
            let extend =
                |v: u64, w: u32| if signed && v >> (w - 1) & 1 != 0 { v | !mask(w) } else { v };
            for i in 0..128u64 {
                let a = (i.wrapping_mul(0x9e3779b97f4a7c15) ^ u64::MAX) & mask(aw);
                let b = (i.wrapping_mul(0xd1b54a32d192ed03) ^ u64::MAX) & mask(bw);
                assert_eq!(
                    evaluate(&nl, &[("a", a), ("b", b)])["y"],
                    extend(a, aw).wrapping_mul(extend(b, bw)) & mask(yw),
                    "{aw}x{bw}->{yw} signed={signed} vector={i}"
                );
            }
        }
    }
}

#[test]
fn dsp_packing_rejects_unsupported_modes_and_controls() {
    let original = synth("module top(input [11:0] a,b,output [23:0] y); assign y=a*b; endmodule");
    let dsp = original.live_cells().find(|&c| original.cell_kind(c) == "DSP48E1").unwrap();
    // One register stage at the inputs, product or output is supported.
    for key in ["AREG", "BREG", "MREG", "PREG"] {
        let mut nl = original.clone();
        nl.set_param(dsp, key, Param::Int(1));
        if key == "AREG" || key == "BREG" {
            nl.set_param(dsp, &format!("{}CASCREG", &key[..1]), Param::Int(1));
        }
        arch_xc7::pack::pack(&mut nl).unwrap();
    }
    for (key, value) in [
        ("AREG", Param::Int(2)),
        ("ACASCREG", Param::Int(1)),
        ("PREG", Param::Int(2)),
        ("IS_OPMODE_INVERTED", Param::Int(1)),
        ("USE_MULT", Param::Int(0)),
    ] {
        let mut nl = original.clone();
        nl.set_param(dsp, key, value);
        let error = arch_xc7::pack::pack(&mut nl).unwrap_err();
        assert!(error.contains(key), "{error}");
    }
    let mut nl = original;
    let dynamic = nl.pin_net(dsp, "A[0]").unwrap();
    let index = nl.cells[dsp].pins.iter().position(|p| nl.str(p.name) == "OPMODE[0]").unwrap();
    let pin = synth_netlist::PinRef { cell: dsp, pin: index as u32 };
    nl.disconnect(pin);
    nl.connect(pin, dynamic).unwrap();
    assert!(arch_xc7::pack::pack(&mut nl).unwrap_err().contains("constant control"));
}

#[test]
fn subtraction_and_comparison_share_carry_hardware() {
    // Both result orderings exercise a comparison (no sum output) being
    // encountered before or after the subtraction during lowering.
    for assignments in ["assign d=a-b; assign lt=a<b;", "assign lt=a<b; assign d=a-b;"] {
        let nl = synth(&format!(
            "module top(input [7:0] a,b, output [7:0] d, output lt); {assignments} endmodule"
        ));
        assert_eq!(nl.live_cells().filter(|&c| nl.cell_kind(c) == "CARRY4").count(), 2);
        for a in 0..256 {
            for b in [0, 1, 127, 128, 254, 255, a, a ^ 255] {
                let out = evaluate(&nl, &[("a", a), ("b", b)]);
                assert_eq!(out["d"], a.wrapping_sub(b) & 255);
                assert_eq!(out["lt"], u64::from(a < b));
            }
        }
    }
}

#[test]
fn shared_propagate_luts_are_replicated_without_an_extra_logic_level() {
    let mut nl = synth(
        "module top(input [7:0] a,b,output [7:0] x,y); assign x=a+b; assign y=(~a)+(~b); endmodule",
    );
    arch_xc7::pack::pack(&mut nl).unwrap();
    let replicated: Vec<_> = nl
        .live_cells()
        .filter(|&c| nl.str(nl.cells[c].name).starts_with("$carry_replicated_lut"))
        .collect();
    assert!(!replicated.is_empty());
    for c in replicated {
        assert_eq!(nl.cell_kind(c), "LUT2");
        for pin in ["I0", "I1"] {
            let n = nl.pin_net(c, pin).unwrap();
            let driver = nl.nets[n].driver.unwrap().cell;
            assert_eq!(nl.cell_kind(driver), synth_netlist::INPUT_PORT);
        }
    }
    nl.check().unwrap();
    for a in 0..256 {
        for b in [0, 1, 127, 128, 254, 255, a, a ^ 255] {
            let out = evaluate(&nl, &[("a", a), ("b", b)]);
            assert_eq!(out["x"], (a + b) & 255);
            assert_eq!(out["y"], (510 - a - b) & 255);
        }
    }
}

fn count(nl: &Netlist, kind: &str) -> usize {
    nl.live_cells().filter(|&c| nl.cell_kind(c) == kind).count()
}

#[test]
fn shallow_memories_become_distributed_ram() {
    // Seven bits read at another address take four RAM32M ports of two bits
    // (three per primitive); a second read at the write address can use
    // port D. Writes keep one write enable per primitive.
    let nl = synth(
        "module m(input clk, we, input [3:0] wa, ra, input [6:0] d, output [6:0] q, p);
         reg [6:0] mem [0:15];
         always @(posedge clk) if (we) mem[wa] <= d;
         assign q = mem[ra];
         assign p = mem[wa];
         endmodule",
    );
    assert_eq!(count(&nl, "RAM32M"), 2);
    assert_eq!(count(&nl, "RAMB18E1"), 0);
    let deep = synth(
        "module m(input clk, input [1:0] we, input [5:0] a, input [3:0] d, output reg [3:0] q);
         reg [3:0] mem [0:63];
         always @(posedge clk) begin
           if (we[0]) mem[a][1:0] <= d[1:0];
           if (we[1]) mem[a][3:2] <= d[3:2];
           q <= mem[a];
         end
         endmodule",
    );
    // Two write-enable groups of two bits, each read at the write address.
    assert_eq!(count(&deep, "RAM64M"), 2);
    assert_eq!(count(&deep, "FDRE"), 4);
}
