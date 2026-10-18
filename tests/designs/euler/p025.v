// Project Euler 25: the index of the first Fibonacci number with 1000 digits.
// Ported from HDLean (https://github.com/nathsou/hdlean, Designs/Euler/P025.lean).
//
// The leading digits of F_k are kept as an 18-digit BCD window, two terms per
// cycle: (a, b) -> (a + b, a + 2b). When a + 2b reaches 19 digits both terms
// drop their last digit and a counter of dropped digits goes up, so a term has
// dropped + 1 digits, or dropped + 2 when its window has 19.
module euler25 (
  input wire clk,
  input wire rst,
  input wire start,
  output reg done,
  output reg [15:0] answer
);
  localparam G = 18;
  localparam [10:0] NEED = 11'd998;   // digits wanted - 2

  reg [4*G-1:0] a, b;
  reg [15:0] k;
  reg [10:0] drop;

  wire [4*G+3:0] s1, b2, s2;
  euler_bcd_add #(.N(G)) add1 (.a(a), .b(b), .cin(1'b0), .s(s1));
  euler_bcd_double #(.N(G)) dbl (.a(b), .s(b2));
  wire [4*G+7:0] s2_full;
  euler_bcd_add #(.N(G + 1)) add2 (.a({4'd0, a}), .b(b2), .cin(1'b0), .s(s2_full));
  assign s2 = s2_full[4*G+3:0];

  wire top1 = s1[4*G +: 4] != 0;
  wire over = s2[4*G +: 4] != 0;
  wire long1 = NEED < drop || (drop == NEED && top1);
  wire long2 = NEED < drop || (drop == NEED && over);
  wire found = !done && (long1 || long2);

  always @(posedge clk)
    if (rst) begin
      a <= 72'd1 << (4 * (G - 1));
      b <= 72'd1 << (4 * (G - 1));
      k <= 16'd2;
      drop <= 0;
      done <= 0;
      answer <= 0;
    end else begin
      if (found) answer <= long1 ? k + 16'd1 : k + 16'd2;
      done <= done || found;
      a <= over ? s1[4*G+3:4] : s1[4*G-1:0];
      b <= over ? s2[4*G+3:4] : s2[4*G-1:0];
      if (over) drop <= drop + 11'd1;
      k <= k + 16'd2;
    end
endmodule
