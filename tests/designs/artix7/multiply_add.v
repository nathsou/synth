// A product plus an addend, truncated to Y bits: one DSP48E1 post-adder.
module multiply_add #(parameter A = 10, B = 10, C = 10, Y = 10, SIGNED = 0) (
  input wire [A-1:0] a,
  input wire [B-1:0] b,
  input wire [C-1:0] c,
  output wire [Y-1:0] y
);
  generate
    if (SIGNED) assign y = $signed(a) * $signed(b) + $signed(c);
    else assign y = a * b + c;
  endgenerate
endmodule
