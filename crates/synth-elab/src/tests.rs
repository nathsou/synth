use super::*;
use synth_rtl::{CellKind, Dff};
use synth_verilog::PreprocOptions;

fn elab(src: &str) -> Module {
    let mut sm = SourceMap::new();
    let f = sm.add("t.v", src);
    let (ast, d) = synth_verilog::parse_files(&mut sm, &[f], &PreprocOptions::default());
    assert!(!d.has_errors(), "{}", d.render(&sm));
    let (m, d) = elaborate(&ast, &sm, &ElabOptions::default(), &NoBlackBoxes);
    assert!(!d.has_errors(), "{}", d.render(&sm));
    m.unwrap()
}

fn elab_err(src: &str) -> String {
    let mut sm = SourceMap::new();
    let f = sm.add("t.v", src);
    let (ast, d) = synth_verilog::parse_files(&mut sm, &[f], &PreprocOptions::default());
    assert!(!d.has_errors(), "{}", d.render(&sm));
    let (_, d) = elaborate(&ast, &sm, &ElabOptions::default(), &NoBlackBoxes);
    assert!(d.has_errors());
    d.render(&sm)
}

fn count(m: &Module, f: impl Fn(&CellKind) -> bool) -> usize {
    m.cells.iter().filter(|c| c.alive && f(&c.kind)).count()
}

#[test]
fn counter_with_async_reset() {
    let m = elab(
        "module top(input clk, input rst_n, input en, output reg [7:0] q);
           always @(posedge clk or negedge rst_n)
             if (!rst_n) q <= 8'd0;
             else if (en) q <= q + 1'b1;
         endmodule",
    );
    let dffs: Vec<&Dff> = m
        .cells
        .iter()
        .filter_map(|c| match &c.kind {
            CellKind::Dff(d) if c.alive => Some(d.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(dffs.len(), 1);
    let d = dffs[0];
    assert_eq!(d.q.width(), 8);
    let (rst, val) = d.arst.as_ref().unwrap();
    assert!(!rst.pol);
    assert!(val.is_zero());
    assert_eq!(
        count(&m, |k| matches!(k, CellKind::Binary { op: synth_rtl::BinaryOp::Add, .. })),
        1
    );
}

#[test]
fn parameters_generate_and_functions() {
    let m = elab(
        "module top #(parameter N = 4) (input [N-1:0] a, output [N-1:0] y, output [3:0] z);
           function [3:0] inc(input [3:0] x); inc = x + 1; endfunction
           genvar i;
           generate for (i = 0; i < N; i = i + 1) begin : g
             assign y[i] = a[N-1-i];
           end endgenerate
           localparam K = $clog2(N) + 1;
           assign z = inc(K);
         endmodule",
    );
    // Reversal is pure wiring; z is constant 4.
    assert_eq!(count(&m, |_| true), 0);
    let z = m.wires.iter().position(|w| w.name == "z").unwrap();
    let (_, rhs) = m
        .connections
        .iter()
        .find(|(l, _)| l.0.iter().any(|b| matches!(b, SigBit::Wire(w, _) if w.index() == z)))
        .unwrap();
    assert_eq!(rhs.as_const().unwrap().to_u64(), Some(4));
}

#[test]
fn comb_case_and_latch_detection() {
    let m = elab(
        "module top(input [1:0] s, input [3:0] a, b, c, output reg [3:0] y, output reg l);
           always @* begin
             case (s)
               2'd0: y = a;
               2'd1: y = b;
               default: y = c;
             endcase
           end
           always @* if (s[0]) l = a[0];
         endmodule",
    );
    assert_eq!(count(&m, |k| matches!(k, CellKind::Latch(_))), 1);
    assert!(count(&m, |k| matches!(k, CellKind::Mux { .. })) >= 2);
}

#[test]
fn memories() {
    let m = elab(
        "module top(input clk, input we, input [3:0] wa, ra, input [7:0] d, output [7:0] q);
           reg [7:0] mem [0:15];
           initial begin mem[0] = 8'h12; mem[1] = 8'h34; end
           always @(posedge clk) if (we) mem[wa] <= d;
           assign q = mem[ra];
         endmodule",
    );
    let mem = m.memories.iter().find(|m| m.alive).unwrap();
    assert_eq!(mem.reads.len(), 1);
    assert_eq!(mem.writes.len(), 1);
    assert_eq!(mem.init[1].to_u64(), Some(0x34));
}

#[test]
fn hierarchy_flattening() {
    let m = elab(
        "module add #(parameter W = 8) (input [W-1:0] a, b, output [W-1:0] s); assign s = a + b; endmodule
         module top(input [3:0] x, y, output [3:0] z);
           add #(.W(4)) u (.a(x), .b(y), .s(z));
         endmodule",
    );
    assert_eq!(count(&m, |k| matches!(k, CellKind::Binary { .. })), 1);
    assert!(m.wires.iter().any(|w| w.name == "u.a"));
    assert_eq!(m.ports().len(), 3);
}

#[test]
fn signed_arithmetic_sizing() {
    // -1 (signed 4-bit) extended to 8 bits must be all ones.
    let m = elab(
        "module top(output [7:0] y, output [7:0] u);
           wire signed [3:0] s = -4'sd1;
           assign y = s;
           assign u = 4'b1111;
         endmodule",
    );
    let find = |name: &str| {
        let w = m.wires.iter().position(|w| w.name == name).unwrap();
        m.connections
            .iter()
            .find(|(l, _)| matches!(l.0[0], SigBit::Wire(x, _) if x.index() == w))
            .unwrap()
            .1
            .clone()
    };
    assert_eq!(find("y").as_const().unwrap().to_u64(), Some(0xFF));
    assert_eq!(find("u").as_const().unwrap().to_u64(), Some(0x0F));
}

#[test]
fn errors() {
    assert!(elab_err("module top(output y); assign y = nope; endmodule").contains("undeclared"));
    assert!(elab_err("module top(input a); foo u(.x(a)); endmodule").contains("unknown module"));
}
